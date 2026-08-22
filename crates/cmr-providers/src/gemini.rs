use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::support::{
    apply_overrides, copy_if_present, ensure_reasoning_item, ensure_tool, finish_events,
    function_call_output, function_tools, gemini_usage, item_type, message_output, output_value,
    parse_arguments, provider_reasoning_payload, push_reasoning_delta, push_text_delta,
    push_tool_delta, reasoning_output_with_provenance, require_object, require_upstream_object,
    required_string, required_upstream_string, response_input_items, response_instructions,
    response_object, scrub_fake_exec_calls, set_reasoning_provenance, text_from_content,
};
use crate::types::{
    AdapterError, ProviderAdapter, ProviderPreset, ResponseEvent, Result, StreamState,
};

/// Converts Responses requests to Gemini `GenerateContent` payloads.
#[derive(Clone, Debug)]
pub struct GeminiGenerateContentAdapter {
    preset: ProviderPreset,
}

const GEMINI_REASONING_FORMAT: &str = "gemini.generate_content.thought_parts.v1";

impl GeminiGenerateContentAdapter {
    /// Creates a Gemini `GenerateContent` adapter.
    #[must_use]
    pub const fn new(preset: ProviderPreset) -> Self {
        Self { preset }
    }
}

fn gemini_parts(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({"text": text})]),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match item_type(part) {
                Some("input_text" | "output_text" | "text") => Ok(json!({
                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
                })),
                Some("input_image" | "image_url") => {
                    let image = part
                        .get("image_url")
                        .or_else(|| part.get("url"))
                        .and_then(|value| {
                            value
                                .as_str()
                                .or_else(|| value.get("url").and_then(Value::as_str))
                        })
                        .ok_or_else(|| {
                            AdapterError::InvalidRequest("image part is missing its URL".into())
                        })?;
                    if let Some(data) = image.strip_prefix("data:") {
                        let (mime_type, data) = data.split_once(";base64,").ok_or_else(|| {
                            AdapterError::InvalidRequest(
                                "Gemini image data URL must use base64 encoding".into(),
                            )
                        })?;
                        Ok(json!({
                            "inlineData": {"mimeType": mime_type, "data": data},
                        }))
                    } else {
                        Ok(json!({
                            "fileData": {"fileUri": image, "mimeType": "image/*"},
                        }))
                    }
                }
                Some("input_file") => Err(AdapterError::Unsupported(
                    "Gemini file conversion requires an uploaded file URI".into(),
                )),
                Some(other) => Err(AdapterError::InvalidRequest(format!(
                    "unsupported Gemini content part `{other}`"
                ))),
                None => Err(AdapterError::InvalidRequest(
                    "message content part is missing `type`".into(),
                )),
            })
            .collect(),
        Some(_) => Err(AdapterError::InvalidRequest(
            "message content must be a string or array".into(),
        )),
    }
}

fn append_part(contents: &mut Vec<Value>, role: &str, part: Value) {
    if let Some(last) = contents.last_mut().and_then(Value::as_object_mut)
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        parts.push(part);
        return;
    }
    contents.push(json!({"role": role, "parts": [part]}));
}

fn sanitized_thought_part(part: &Value) -> Option<Value> {
    if part.get("thought").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut sanitized = Map::new();
    sanitized.insert("thought".into(), Value::Bool(true));
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        sanitized.insert("text".into(), Value::String(text.to_owned()));
    }
    if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
        sanitized.insert(
            "thoughtSignature".into(),
            Value::String(signature.to_owned()),
        );
    }
    Some(Value::Object(sanitized))
}

fn stream_thought_parts_mut<'a>(
    state: &'a mut StreamState,
    provider_id: &str,
) -> Option<&'a mut Vec<Value>> {
    let provenance = state.reasoning.as_mut()?.provenance.as_mut()?;
    if provenance.source_provider_id != provider_id || provenance.format != GEMINI_REASONING_FORMAT
    {
        return None;
    }
    provenance.payload.get_mut("parts")?.as_array_mut()
}

fn update_stream_thought_part(state: &mut StreamState, provider_id: &str, part: &Value) {
    let valid = state
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.provenance.as_ref())
        .is_some_and(|provenance| {
            provenance.source_provider_id == provider_id
                && provenance.format == GEMINI_REASONING_FORMAT
        });
    if !valid {
        set_reasoning_provenance(
            state,
            provider_id,
            GEMINI_REASONING_FORMAT,
            json!({"parts": [{"thought": true, "text": ""}]}),
        );
    }
    let Some(parts) = stream_thought_parts_mut(state, provider_id) else {
        return;
    };
    if parts.is_empty() {
        parts.push(json!({"thought": true, "text": ""}));
    }
    let Some(target) = parts.last_mut().and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(delta) = part.get("text").and_then(Value::as_str) {
        let current = target
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        target.insert("text".into(), Value::String(format!("{current}{delta}")));
    }
    if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
        target.insert(
            "thoughtSignature".into(),
            Value::String(signature.to_owned()),
        );
    }
}

fn gemini_tool_config(choice: &Value) -> Value {
    match choice.as_str() {
        Some("none") => json!({"functionCallingConfig":{"mode":"NONE"}}),
        Some("required") => json!({"functionCallingConfig":{"mode":"ANY"}}),
        Some("auto") => json!({"functionCallingConfig":{"mode":"AUTO"}}),
        _ if choice.get("type").and_then(Value::as_str) == Some("function") => json!({
            "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": [choice.get("name").cloned().unwrap_or(Value::Null)],
            },
        }),
        _ => choice.clone(),
    }
}

impl ProviderAdapter for GeminiGenerateContentAdapter {
    fn preset(&self) -> &ProviderPreset {
        &self.preset
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        if stream {
            format!("/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("/models/{model}:generateContent")
        }
    }

    #[allow(clippy::too_many_lines)]
    fn encode_request(&self, request: &Value) -> Result<Value> {
        let request = require_object(request, "Responses request")?;
        required_string(request, "model")?;
        let mut system = response_instructions(request)?
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut contents = Vec::new();
        let mut function_names = BTreeMap::new();

        for item in response_input_items(request)? {
            let object = item.as_object().ok_or_else(|| {
                AdapterError::InvalidRequest("each input item must be an object".into())
            })?;
            match item_type(&item) {
                Some("message") => {
                    let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
                    if matches!(role, "system" | "developer") {
                        let text = text_from_content(object.get("content"));
                        if !text.is_empty() {
                            if !system.is_empty() {
                                system.push_str("\n\n");
                            }
                            system.push_str(&text);
                        }
                    } else {
                        let role = if role == "assistant" { "model" } else { "user" };
                        let mut parts = gemini_parts(object.get("content"))?;
                        if role == "model" {
                            let mut content = Value::Array(std::mem::take(&mut parts));
                            scrub_fake_exec_calls(&mut content);
                            parts = match content {
                                Value::Array(items) => items,
                                Value::String(text) if text.is_empty() => Vec::new(),
                                Value::String(text) => vec![json!({"text": text})],
                                _ => Vec::new(),
                            };
                        }
                        for part in parts {
                            append_part(&mut contents, role, part);
                        }
                    }
                }
                Some("function_call" | "custom_tool_call") => {
                    let call_id = object
                        .get("call_id")
                        .or_else(|| object.get("id"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            AdapterError::InvalidRequest(
                                "function_call is missing `call_id`".into(),
                            )
                        })?;
                    let name = required_string(object, "name")?;
                    function_names.insert(call_id.to_owned(), name.to_owned());
                    // Custom tools carry a freeform text `input` instead of JSON
                    // `arguments`; accept either so replayed history stays paired.
                    let arguments = object.get("arguments").or_else(|| object.get("input"));
                    append_part(
                        &mut contents,
                        "model",
                        json!({
                            "functionCall": {
                                "id": call_id,
                                "name": name,
                                "args": parse_arguments(arguments)?,
                            }
                        }),
                    );
                }
                Some("function_call_output" | "custom_tool_call_output") => {
                    let call_id =
                        object
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                AdapterError::InvalidRequest(
                                    "function_call_output is missing `call_id`".into(),
                                )
                            })?;
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| function_names.get(call_id).map(String::as_str))
                        .unwrap_or(call_id);
                    append_part(
                        &mut contents,
                        "user",
                        json!({
                            "functionResponse": {
                                "id": call_id,
                                "name": name,
                                "response": {"output": output_value(object.get("output"))},
                            }
                        }),
                    );
                }
                Some("reasoning") => {
                    if let Some(parts) =
                        provider_reasoning_payload(object, &self.preset.id, GEMINI_REASONING_FORMAT)
                            .and_then(|payload| payload.get("parts"))
                            .and_then(Value::as_array)
                    {
                        for part in parts.iter().filter_map(sanitized_thought_part) {
                            append_part(&mut contents, "model", part);
                        }
                    }
                }
                // Opaque compaction is native-Responses-only, and newer Codex
                // clients attach extra tool definitions via `additional_tools`;
                // the top-level tools field already carries the standard set.
                Some("compaction" | "additional_tools") => {}
                Some(other) => {
                    return Err(AdapterError::InvalidRequest(format!(
                        "unsupported Responses input item `{other}`"
                    )));
                }
                None => {
                    return Err(AdapterError::InvalidRequest(
                        "input item is missing `type`".into(),
                    ));
                }
            }
        }

        let mut body = Map::new();
        body.insert("contents".into(), Value::Array(contents));
        if !system.is_empty() {
            body.insert(
                "systemInstruction".into(),
                json!({"parts":[{"text":system}]}),
            );
        }

        let tools = function_tools(request)
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            body.insert("tools".into(), json!([{"functionDeclarations": tools}]));
        }
        if let Some(choice) = request.get("tool_choice") {
            body.insert("toolConfig".into(), gemini_tool_config(choice));
        }

        let mut generation = Map::new();
        copy_if_present(
            request,
            &mut generation,
            "max_output_tokens",
            "maxOutputTokens",
        );
        copy_if_present(request, &mut generation, "temperature", "temperature");
        copy_if_present(request, &mut generation, "top_p", "topP");
        if !generation.is_empty() {
            body.insert("generationConfig".into(), Value::Object(generation));
        }
        apply_overrides(&mut body, &self.preset.request_overrides)?;
        Ok(Value::Object(body))
    }

    fn decode_response(&self, response: &Value, response_id: &str) -> Result<Value> {
        let response = require_upstream_object(response, "Gemini response")?;
        let parts = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AdapterError::MalformedUpstream("Gemini response has no candidate parts".into())
            })?;

        let mut visible = String::new();
        let mut reasoning = String::new();
        let mut thought_parts = Vec::new();
        let mut calls = Vec::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    reasoning.push_str(text);
                } else {
                    visible.push_str(text);
                }
            }
            if let Some(part) = sanitized_thought_part(part) {
                thought_parts.push(part);
            }
            if let Some(call) = part.get("functionCall") {
                calls.push(call);
            }
        }

        let mut output = Vec::new();
        if !thought_parts.is_empty() {
            output.push(reasoning_output_with_provenance(
                &format!("rs_{response_id}_{}", output.len()),
                &reasoning,
                &self.preset.id,
                GEMINI_REASONING_FORMAT,
                &json!({"parts": thought_parts}),
            ));
        }
        if !visible.is_empty() {
            output.push(message_output(
                &format!("msg_{response_id}_{}", output.len()),
                &visible,
            ));
        }
        for call in calls {
            let call = call.as_object().ok_or_else(|| {
                AdapterError::MalformedUpstream("Gemini functionCall must be an object".into())
            })?;
            let name = required_upstream_string(call, "name", "Gemini functionCall")?;
            let call_id = required_upstream_string(call, "id", "Gemini functionCall")?;
            let arguments = call.get("args").and_then(Value::as_object).ok_or_else(|| {
                AdapterError::MalformedUpstream(
                    "Gemini functionCall is missing object `args`".into(),
                )
            })?;
            output.push(function_call_output(
                &format!("fc_{response_id}_{}", output.len()),
                call_id,
                name,
                &Value::Object(arguments.clone()).to_string(),
            ));
        }

        Ok(response_object(
            response_id,
            self.preset.default_model.as_deref().unwrap_or("gemini"),
            None,
            &output,
            gemini_usage(response.get("usageMetadata")).as_ref(),
        ))
    }

    fn decode_stream_chunk(&self, state: &mut StreamState, chunk: &Value) -> Result<Vec<Value>> {
        let chunk = require_upstream_object(chunk, "Gemini stream chunk")?;
        if let Some(error) = chunk.get("error") {
            let sequence_number = state.take_sequence_number();
            state.completed = true;
            return Ok(vec![
                ResponseEvent::Error {
                    code: error
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("provider_error")
                        .to_owned(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider stream error")
                        .to_owned(),
                    param: None,
                    sequence_number,
                }
                .into_json()?,
            ]);
        }
        if let Some(usage) = gemini_usage(chunk.get("usageMetadata")) {
            state.usage = Some(usage);
        }
        let Some(candidate) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for (index, part) in parts.iter().enumerate() {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        ensure_reasoning_item(state, &mut events)?;
                        update_stream_thought_part(state, &self.preset.id, part);
                        push_reasoning_delta(state, text, &mut events)?;
                    } else {
                        push_text_delta(state, text, &mut events)?;
                    }
                }
                if part.get("thought").and_then(Value::as_bool) == Some(true)
                    && part.get("text").is_none()
                {
                    ensure_reasoning_item(state, &mut events)?;
                    update_stream_thought_part(state, &self.preset.id, part);
                }
                if let Some(call) = part.get("functionCall") {
                    let name = call.get("name").and_then(Value::as_str);
                    ensure_tool(
                        state,
                        index,
                        call.get("id").and_then(Value::as_str).or(name),
                        name,
                        &mut events,
                    )?;
                    let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    push_tool_delta(state, index, &arguments.to_string(), &mut events)?;
                }
            }
        }
        if candidate
            .get("finishReason")
            .is_some_and(|value| !value.is_null())
        {
            events.extend(finish_events(state)?);
        }
        Ok(events)
    }

    fn finish_stream(&self, state: &mut StreamState) -> Result<Vec<Value>> {
        finish_events(state)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::preset_by_id;

    fn adapter() -> GeminiGenerateContentAdapter {
        GeminiGenerateContentAdapter::new(preset_by_id("gemini").unwrap())
    }

    #[test]
    fn maps_tools_and_tool_results() {
        let request = json!({
            "model":"gemini-test",
            "input":[
                {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"id\":7}"},
                {"type":"function_call_output","call_id":"c1","output":{"ok":true}}
            ],
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]
        });
        let converted = adapter().encode_request(&request).unwrap();
        assert_eq!(
            converted["contents"][0]["parts"][0]["functionCall"]["name"],
            "lookup"
        );
        assert_eq!(
            converted["contents"][1]["parts"][0]["functionResponse"]["id"],
            "c1"
        );
        assert_eq!(
            converted["contents"][1]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );
        assert_eq!(
            converted["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
    }

    #[test]
    fn isolates_thought_parts() {
        let response = json!({
            "candidates":[{"content":{"parts":[
                {"thought":true,"text":"hidden","thoughtSignature":"opaque-signature"},
                {"text":"visible"}
            ]}}],
            "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}
        });
        let converted = adapter().decode_response(&response, "r1").unwrap();
        assert_eq!(converted["output"][0]["type"], "reasoning");
        assert_eq!(converted["output"][1]["type"], "message");
        assert!(!converted["output"][1].to_string().contains("hidden"));

        let replay = json!({"model":"gemini-test", "input":converted["output"].clone()});
        let same_provider = adapter().encode_request(&replay).unwrap();
        assert_eq!(same_provider["contents"][0]["parts"][0]["thought"], true);
        assert_eq!(
            same_provider["contents"][0]["parts"][0]["thoughtSignature"],
            "opaque-signature"
        );
        assert_eq!(same_provider["contents"][0]["parts"][1]["text"], "visible");

        let mut foreign_preset = preset_by_id("gemini").unwrap();
        foreign_preset.id = "other-gemini".into();
        let foreign = GeminiGenerateContentAdapter::new(foreign_preset)
            .encode_request(&replay)
            .unwrap();
        assert_eq!(foreign["contents"][0]["parts"][0]["text"], "visible");
        assert!(!foreign.to_string().contains("opaque-signature"));
        assert!(!foreign.to_string().contains("hidden"));
    }
}
