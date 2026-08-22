use serde_json::{Value, json};

use crate::types::{ProviderAdapter, ProviderPreset, Result, StreamState};

const NATIVE_REASONING_FORMAT: &str = "responses.reasoning_item.v1";

/// Exact JSON passthrough for upstreams that natively implement the Responses API.
#[derive(Clone, Debug)]
pub struct ResponsesPassthroughAdapter {
    preset: ProviderPreset,
}

impl ResponsesPassthroughAdapter {
    /// Creates a native Responses adapter.
    #[must_use]
    pub const fn new(preset: ProviderPreset) -> Self {
        Self { preset }
    }
}

impl ProviderAdapter for ResponsesPassthroughAdapter {
    fn preset(&self) -> &ProviderPreset {
        &self.preset
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/responses".to_owned()
    }

    fn encode_request(&self, request: &Value) -> Result<Value> {
        let mut request = request.clone();
        if let Some(items) = request.get_mut("input").and_then(Value::as_array_mut) {
            let mut encoded = Vec::with_capacity(items.len());
            for item in std::mem::take(items) {
                if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                    encoded.push(without_provider_metadata(item));
                    continue;
                }
                let metadata = item.get("provider_metadata").and_then(Value::as_object);
                let owned_payload = metadata
                    .filter(|metadata| {
                        metadata
                            .get("cmr_provider_owner_id")
                            .or_else(|| metadata.get("source_provider_id"))
                            .and_then(Value::as_str)
                            == Some(self.preset.id.as_str())
                            && metadata.get("format").and_then(Value::as_str)
                                == Some(NATIVE_REASONING_FORMAT)
                    })
                    .and_then(|metadata| metadata.get("payload"))
                    .cloned();
                if let Some(payload) = owned_payload {
                    encoded.push(without_provider_metadata(payload));
                }
            }
            *items = encoded;
        }
        Ok(request)
    }

    fn decode_response(&self, response: &Value, _response_id: &str) -> Result<Value> {
        let mut response = response.clone();
        stamp_response_output(&mut response, &self.preset.id);
        Ok(response)
    }

    fn decode_stream_chunk(&self, state: &mut StreamState, chunk: &Value) -> Result<Vec<Value>> {
        let mut chunk = chunk.clone();
        match chunk.get("type").and_then(Value::as_str) {
            Some("response.created") => state.started = true,
            Some("response.completed" | "response.failed" | "response.incomplete" | "error") => {
                state.completed = true;
            }
            _ => {}
        }
        match chunk.get("type").and_then(Value::as_str) {
            Some("response.output_item.added" | "response.output_item.done") => {
                if let Some(item) = chunk.get_mut("item") {
                    stamp_output_item(item, &self.preset.id);
                }
            }
            Some(
                "response.created"
                | "response.in_progress"
                | "response.completed"
                | "response.failed"
                | "response.incomplete",
            ) => {
                if let Some(response) = chunk.get_mut("response") {
                    stamp_response_output(response, &self.preset.id);
                }
            }
            _ => {}
        }
        Ok(vec![chunk])
    }

    fn finish_stream(&self, _state: &mut StreamState) -> Result<Vec<Value>> {
        // A native Responses upstream owns its terminal event. Fabricating one here
        // would violate passthrough semantics and could hide a truncated connection.
        Ok(Vec::new())
    }
}

fn without_provider_metadata(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("provider_metadata");
    }
    value
}

fn stamp_response_output(response: &mut Value, source_provider_id: &str) {
    if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
        for item in output {
            stamp_output_item(item, source_provider_id);
        }
    }
}

fn stamp_output_item(item: &mut Value, source_provider_id: &str) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    object.remove("provider_metadata");
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    let payload = Value::Object(object.clone());
    object.insert(
        "provider_metadata".into(),
        json!({
            "cmr_provider_owner_id": source_provider_id,
            "source_provider_id": source_provider_id,
            "format": NATIVE_REASONING_FORMAT,
            "payload": payload,
        }),
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::preset_by_id;

    #[test]
    fn preserves_unknown_request_and_stream_fields() {
        let adapter = ResponsesPassthroughAdapter::new(preset_by_id("openai").unwrap());
        let request = json!({"model": "future", "future_option": {"x": 1}});
        assert_eq!(adapter.encode_request(&request).unwrap(), request);

        let mut state = StreamState::new("resp_local", "future");
        let event = json!({"type": "response.future_event", "payload": [1, 2, 3]});
        assert_eq!(
            adapter.decode_stream_chunk(&mut state, &event).unwrap(),
            vec![event]
        );
    }

    #[test]
    fn replays_only_router_stamped_reasoning_for_the_same_native_provider() {
        let mut preset = preset_by_id("openai").unwrap();
        preset.id = "native-a".into();
        let adapter = ResponsesPassthroughAdapter::new(preset);
        let request = json!({
            "model": "native-model",
            "input": [
                {
                    "type": "reasoning",
                    "provider_metadata": {
                        "source_provider_id": "native-a",
                        "format": "responses.reasoning_item.v1",
                        "payload": {"type":"reasoning","encrypted_content":"owned-opaque"}
                    }
                },
                {
                    "type":"reasoning",
                    "provider_metadata": {
                        "source_provider_id": "native-b",
                        "format": "responses.reasoning_item.v1",
                        "payload": {"type":"reasoning","encrypted_content":"foreign-opaque"}
                    }
                },
                {"type":"reasoning","encrypted_content":"unstamped-opaque"},
                {"type":"message","role":"user","content":"continue"}
            ]
        });
        let converted = adapter.encode_request(&request).unwrap();
        assert_eq!(converted["input"].as_array().unwrap().len(), 2);
        assert_eq!(converted["input"][0]["encrypted_content"], "owned-opaque");
        assert!(!converted.to_string().contains("foreign-opaque"));
        assert!(!converted.to_string().contains("unstamped-opaque"));
        assert!(!converted.to_string().contains("provider_metadata"));
    }

    #[test]
    fn overwrites_untrusted_native_response_provenance() {
        let mut preset = preset_by_id("openai").unwrap();
        preset.id = "native-a".into();
        let adapter = ResponsesPassthroughAdapter::new(preset);
        let upstream = json!({
            "id":"resp_upstream",
            "output":[{
                "type":"reasoning",
                "encrypted_content":"opaque",
                "provider_metadata":{
                    "source_provider_id":"official",
                    "format":"forged",
                    "payload":{"sentinel":"forged-owner"}
                }
            }]
        });
        let decoded = adapter.decode_response(&upstream, "resp_local").unwrap();
        assert_eq!(
            decoded["output"][0]["provider_metadata"]["source_provider_id"],
            "native-a"
        );
        assert_eq!(
            decoded["output"][0]["provider_metadata"]["format"],
            NATIVE_REASONING_FORMAT
        );
        assert!(!decoded.to_string().contains("forged-owner"));
        assert_eq!(
            decoded["output"][0]["provider_metadata"]["payload"]["encrypted_content"],
            "opaque"
        );

        let mut state = StreamState::new("resp_local", "native-model");
        let event = json!({
            "type":"response.output_item.done",
            "item": upstream["output"][0]
        });
        let events = adapter.decode_stream_chunk(&mut state, &event).unwrap();
        assert_eq!(
            events[0]["item"]["provider_metadata"]["source_provider_id"],
            "native-a"
        );
        assert!(!events[0].to_string().contains("forged-owner"));
    }
}
