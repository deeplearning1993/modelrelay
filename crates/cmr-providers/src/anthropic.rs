use serde_json::{Map, Value, json};

use crate::support::{
    anthropic_usage, apply_overrides, copy_if_present, ensure_reasoning_item, ensure_tool,
    finish_events, function_call_output, function_tools, item_type, message_output, output_value,
    parse_arguments, provider_reasoning_payload, push_reasoning_delta, push_text_delta,
    push_tool_delta, reasoning_output_with_provenance, require_object, require_upstream_object,
    required_string, required_upstream_string, response_input_items, response_instructions,
    response_object, scrub_fake_exec_calls, set_reasoning_provenance, text_from_content,
};
use crate::types::{
    AdapterError, ProviderAdapter, ProviderPreset, ResponseEvent, Result, StreamState,
};

/// Converts Responses requests to Anthropic Messages payloads.
#[derive(Clone, Debug)]
pub struct AnthropicMessagesAdapter {
    preset: ProviderPreset,
}

const ANTHROPIC_REASONING_FORMAT: &str = "anthropic.messages.thinking_blocks.v1";

impl AnthropicMessagesAdapter {
    /// Creates an Anthropic Messages adapter.
    #[must_use]
    pub const fn new(preset: ProviderPreset) -> Self {
        Self { preset }
    }
}

fn anthropic_parts(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({"type":"text", "text":text})]),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match item_type(part) {
                Some("input_text" | "output_text" | "text") => Ok(json!({
                    "type": "text",
                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
                })),
                Some("input_image" | "image_url") => {
                    let url = part
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
                    Ok(json!({
                        "type": "image",
                        "source": {"type": "url", "url": url},
                    }))
                }
                Some("input_file") => Err(AdapterError::Unsupported(
                    "Anthropic file conversion is not portable; use text or image input".into(),
                )),
                Some(other) => Err(AdapterError::InvalidRequest(format!(
                    "unsupported Anthropic content part `{other}`"
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

fn append_message(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut().and_then(Value::as_object_mut)
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.push(block);
        return;
    }
    messages.push(json!({"role": role, "content": [block]}));
}

fn append_message_parts(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    for block in blocks {
        append_message(messages, role, block);
    }
}

fn sanitized_thinking_block(block: &Value) -> Option<Value> {
    match item_type(block)? {
        "thinking" => {
            let mut sanitized = Map::new();
            sanitized.insert("type".into(), Value::String("thinking".into()));
            sanitized.insert(
                "thinking".into(),
                Value::String(
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            );
            if let Some(signature) = block.get("signature").and_then(Value::as_str) {
                sanitized.insert("signature".into(), Value::String(signature.to_owned()));
            }
            Some(Value::Object(sanitized))
        }
        "redacted_thinking" => block
            .get("data")
            .and_then(Value::as_str)
            .map(|data| json!({"type": "redacted_thinking", "data": data})),
        _ => None,
    }
}

fn stream_reasoning_blocks_mut<'a>(
    state: &'a mut StreamState,
    provider_id: &str,
) -> Option<&'a mut Vec<Value>> {
    let provenance = state.reasoning.as_mut()?.provenance.as_mut()?;
    if provenance.source_provider_id != provider_id
        || provenance.format != ANTHROPIC_REASONING_FORMAT
    {
        return None;
    }
    provenance.payload.get_mut("blocks")?.as_array_mut()
}

fn ensure_stream_reasoning_payload(state: &mut StreamState, provider_id: &str) {
    let valid = state
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.provenance.as_ref())
        .is_some_and(|provenance| {
            provenance.source_provider_id == provider_id
                && provenance.format == ANTHROPIC_REASONING_FORMAT
        });
    if !valid {
        set_reasoning_provenance(
            state,
            provider_id,
            ANTHROPIC_REASONING_FORMAT,
            json!({"blocks": []}),
        );
    }
}

fn append_stream_reasoning_field(
    state: &mut StreamState,
    provider_id: &str,
    field: &str,
    delta: &str,
) {
    ensure_stream_reasoning_payload(state, provider_id);
    let Some(blocks) = stream_reasoning_blocks_mut(state, provider_id) else {
        return;
    };
    if blocks.last().and_then(item_type) != Some("thinking") {
        blocks.push(json!({"type": "thinking", "thinking": ""}));
    }
    if let Some(block) = blocks.last_mut().and_then(Value::as_object_mut) {
        let current = block
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        block.insert(field.to_owned(), Value::String(format!("{current}{delta}")));
    }
}

fn anthropic_tool_choice(choice: &Value) -> Value {
    match choice.as_str() {
        Some("auto") => json!({"type":"auto"}),
        Some("required") => json!({"type":"any"}),
        Some("none") => json!({"type":"none"}),
        _ if choice.get("type").and_then(Value::as_str) == Some("function") => json!({
            "type": "tool",
            "name": choice.get("name").cloned().unwrap_or(Value::Null),
        }),
        _ => choice.clone(),
    }
}

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn preset(&self) -> &ProviderPreset {
        &self.preset
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/messages".to_owned()
    }

    #[allow(clippy::too_many_lines)]
    fn encode_request(&self, request: &Value) -> Result<Value> {
        let request = require_object(request, "Responses request")?;
        let model = required_string(request, "model")?;
        let mut system = response_instructions(request)?
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut messages = Vec::new();

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
                        let role = if role == "assistant" {
                            "assistant"
                        } else {
                            "user"
                        };
                        let mut blocks = anthropic_parts(object.get("content"))?;
                        if role == "assistant" {
                            let mut content = Value::Array(std::mem::take(&mut blocks));
                            scrub_fake_exec_calls(&mut content);
                            blocks = match content {
                                Value::Array(parts) => parts,
                                Value::String(text) if text.is_empty() => Vec::new(),
                                Value::String(text) => vec![json!({"type": "text", "text": text})],
                                _ => Vec::new(),
                            };
                        }
                        append_message_parts(&mut messages, role, blocks);
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
                    // Custom tools carry a freeform text `input` instead of JSON
                    // `arguments`; accept either so replayed history stays paired.
                    let arguments = object.get("arguments").or_else(|| object.get("input"));
                    append_message(
                        &mut messages,
                        "assistant",
                        json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": required_string(object, "name")?,
                            "input": parse_arguments(arguments)?,
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
                    let output = output_value(object.get("output"));
                    let output = output
                        .as_str()
                        .map_or_else(|| output.to_string(), str::to_owned);
                    append_message(
                        &mut messages,
                        "user",
                        json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": output,
                        }),
                    );
                }
                Some("reasoning") => {
                    if let Some(blocks) = provider_reasoning_payload(
                        object,
                        &self.preset.id,
                        ANTHROPIC_REASONING_FORMAT,
                    )
                    .and_then(|payload| payload.get("blocks"))
                    .and_then(Value::as_array)
                    {
                        for block in blocks.iter().filter_map(sanitized_thinking_block) {
                            append_message(&mut messages, "assistant", block);
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
        body.insert("model".into(), Value::String(model.to_owned()));
        body.insert("messages".into(), Value::Array(messages));
        body.insert(
            "max_tokens".into(),
            request
                .get("max_output_tokens")
                .cloned()
                .unwrap_or_else(|| json!(4096)),
        );
        if !system.is_empty() {
            body.insert("system".into(), Value::String(system));
        }
        copy_if_present(request, &mut body, "stream", "stream");
        copy_if_present(request, &mut body, "temperature", "temperature");
        copy_if_present(request, &mut body, "top_p", "top_p");

        let tools = function_tools(request)
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "input_schema": tool.get("parameters").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(choice) = request.get("tool_choice") {
            body.insert("tool_choice".into(), anthropic_tool_choice(choice));
        }
        apply_overrides(&mut body, &self.preset.request_overrides)?;
        Ok(Value::Object(body))
    }

    fn decode_response(&self, response: &Value, response_id: &str) -> Result<Value> {
        let response = require_upstream_object(response, "Anthropic Messages response")?;
        let model = response
            .get("model")
            .and_then(Value::as_str)
            .or(self.preset.default_model.as_deref())
            .unwrap_or("unknown");
        let blocks = response
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AdapterError::MalformedUpstream("Anthropic response has no content array".into())
            })?;

        let mut visible = String::new();
        let mut reasoning = String::new();
        let mut reasoning_blocks = Vec::new();
        let mut calls = Vec::new();
        for block in blocks {
            match item_type(block) {
                Some("text") => visible.push_str(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                Some("thinking" | "redacted_thinking") => {
                    if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(thinking);
                    }
                    if let Some(block) = sanitized_thinking_block(block) {
                        reasoning_blocks.push(block);
                    }
                }
                Some("tool_use") => calls.push(block),
                _ => {}
            }
        }

        let mut output = Vec::new();
        if !reasoning_blocks.is_empty() {
            output.push(reasoning_output_with_provenance(
                &format!("rs_{response_id}_{}", output.len()),
                &reasoning,
                &self.preset.id,
                ANTHROPIC_REASONING_FORMAT,
                &json!({"blocks": reasoning_blocks}),
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
                AdapterError::MalformedUpstream("Anthropic tool_use must be an object".into())
            })?;
            let name = required_upstream_string(call, "name", "Anthropic tool_use")?;
            let call_id = required_upstream_string(call, "id", "Anthropic tool_use")?;
            let input = call
                .get("input")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    AdapterError::MalformedUpstream(
                        "Anthropic tool_use is missing object `input`".into(),
                    )
                })?;
            output.push(function_call_output(
                &format!("fc_{response_id}_{}", output.len()),
                call_id,
                name,
                &Value::Object(input.clone()).to_string(),
            ));
        }

        Ok(response_object(
            response_id,
            model,
            None,
            &output,
            anthropic_usage(response.get("usage"), None).as_ref(),
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_stream_chunk(&self, state: &mut StreamState, chunk: &Value) -> Result<Vec<Value>> {
        let chunk = require_upstream_object(chunk, "Anthropic stream event")?;
        let kind = chunk
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match kind {
            "message_start" => {
                if let Some(usage) = anthropic_usage(
                    chunk
                        .get("message")
                        .and_then(|message| message.get("usage")),
                    state.usage.as_ref(),
                ) {
                    state.usage = Some(usage);
                }
            }
            "content_block_start" => {
                let index = chunk
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(0);
                let block = chunk.get("content_block").ok_or_else(|| {
                    AdapterError::MalformedUpstream(
                        "content_block_start has no content_block".into(),
                    )
                })?;
                match item_type(block) {
                    Some("text") => push_text_delta(
                        state,
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        &mut events,
                    )?,
                    Some("thinking" | "redacted_thinking") => {
                        ensure_reasoning_item(state, &mut events)?;
                        ensure_stream_reasoning_payload(state, &self.preset.id);
                        if let Some(block) = sanitized_thinking_block(block)
                            && let Some(blocks) =
                                stream_reasoning_blocks_mut(state, &self.preset.id)
                        {
                            blocks.push(block);
                        }
                        if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                            push_reasoning_delta(state, thinking, &mut events)?;
                        }
                    }
                    Some("tool_use") => {
                        ensure_tool(
                            state,
                            index,
                            block.get("id").and_then(Value::as_str),
                            block.get("name").and_then(Value::as_str),
                            &mut events,
                        )?;
                        if let Some(input) = block.get("input").filter(|input| {
                            input.as_object().is_some_and(|object| !object.is_empty())
                        }) {
                            push_tool_delta(state, index, &input.to_string(), &mut events)?;
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = chunk
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(0);
                let delta = chunk.get("delta").ok_or_else(|| {
                    AdapterError::MalformedUpstream("content block delta is missing".into())
                })?;
                match item_type(delta) {
                    Some("text_delta") => push_text_delta(
                        state,
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        &mut events,
                    )?,
                    Some("thinking_delta") => {
                        let thinking = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        push_reasoning_delta(state, thinking, &mut events)?;
                        append_stream_reasoning_field(state, &self.preset.id, "thinking", thinking);
                    }
                    Some("signature_delta") => {
                        let signature = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        append_stream_reasoning_field(
                            state,
                            &self.preset.id,
                            "signature",
                            signature,
                        );
                    }
                    Some("input_json_delta") => push_tool_delta(
                        state,
                        index,
                        delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        &mut events,
                    )?,
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = anthropic_usage(chunk.get("usage"), state.usage.as_ref()) {
                    state.usage = Some(usage);
                }
            }
            "message_stop" => events.extend(finish_events(state)?),
            "error" => {
                let sequence_number = state.take_sequence_number();
                state.completed = true;
                let error = chunk.get("error").unwrap_or(&Value::Null);
                events.push(
                    ResponseEvent::Error {
                        code: error
                            .get("type")
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
                );
            }
            _ => {}
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

    fn adapter() -> AnthropicMessagesAdapter {
        AnthropicMessagesAdapter::new(preset_by_id("anthropic").unwrap())
    }

    #[test]
    fn maps_function_calls_and_results_to_tool_blocks() {
        let request = json!({
            "model":"claude-test",
            "input":[
                {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"id\":7}"},
                {"type":"function_call_output","call_id":"c1","output":"ok"}
            ],
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]
        });
        let converted = adapter().encode_request(&request).unwrap();
        assert_eq!(converted["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(
            converted["messages"][1]["content"][0]["type"],
            "tool_result"
        );
        assert_eq!(converted["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn keeps_thinking_out_of_visible_text() {
        let response = json!({
            "model":"claude-test",
            "content":[
                {"type":"thinking","thinking":"hidden","signature":"signed-by-provider"},
                {"type":"text","text":"shown"}
            ],
            "usage":{"input_tokens":1,"output_tokens":2}
        });
        let converted = adapter().decode_response(&response, "r1").unwrap();
        assert_eq!(converted["output"][0]["type"], "reasoning");
        assert_eq!(converted["output"][1]["type"], "message");
        assert!(!converted["output"][1].to_string().contains("hidden"));

        let replay = json!({"model":"claude-test", "input":converted["output"].clone()});
        let same_provider = adapter().encode_request(&replay).unwrap();
        assert_eq!(
            same_provider["messages"][0]["content"][0]["type"],
            "thinking"
        );
        assert_eq!(
            same_provider["messages"][0]["content"][0]["signature"],
            "signed-by-provider"
        );
        assert_eq!(same_provider["messages"][0]["content"][1]["text"], "shown");

        let mut foreign_preset = preset_by_id("anthropic").unwrap();
        foreign_preset.id = "other-anthropic".into();
        let foreign = AnthropicMessagesAdapter::new(foreign_preset)
            .encode_request(&replay)
            .unwrap();
        assert_eq!(foreign["messages"][0]["content"][0]["type"], "text");
        assert!(!foreign.to_string().contains("signed-by-provider"));
        assert!(!foreign.to_string().contains("hidden"));
    }
}
