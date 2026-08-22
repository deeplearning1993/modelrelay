use serde_json::{Map, Value, json};

use crate::support::{
    apply_overrides, chat_usage, copy_if_present, custom_string_tools, ensure_tool, finish_events,
    finish_incomplete_events, function_call_output, function_tools, incomplete_response_object,
    item_type, message_output, normalize_arguments, output_value, provider_reasoning_payload,
    push_reasoning_delta, push_text_delta, push_tool_delta, reasoning_output_with_provenance,
    require_object, require_upstream_object, required_string, required_upstream_json_arguments,
    required_upstream_string, response_input_items, response_instructions, response_object,
    scrub_fake_exec_calls, set_reasoning_provenance, streamed_reasoning_text, text_from_content,
};
use crate::types::{
    AdapterError, ProviderAdapter, ProviderPreset, ResponseEvent, Result, StreamState,
};

/// Converts Responses requests to OpenAI-compatible Chat Completions payloads.
#[derive(Clone, Debug)]
pub struct OpenAiChatCompletionsAdapter {
    preset: ProviderPreset,
}

const CHAT_REASONING_FORMAT: &str = "openai_chat.reasoning_content.v1";

impl OpenAiChatCompletionsAdapter {
    /// Creates an OpenAI-compatible adapter.
    #[must_use]
    pub const fn new(preset: ProviderPreset) -> Self {
        Self { preset }
    }
}

fn chat_content(content: Option<&Value>) -> Result<Value> {
    match content {
        None | Some(Value::Null) => Ok(Value::String(String::new())),
        Some(Value::String(text)) => Ok(Value::String(text.clone())),
        Some(Value::Array(parts)) => {
            let mut converted = Vec::new();
            for part in parts {
                match item_type(part) {
                    Some("input_text" | "output_text" | "text") => converted.push(json!({
                        "type": "text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
                    })),
                    Some("input_image" | "image_url") => {
                        let image_url = part
                            .get("image_url")
                            .or_else(|| part.get("url"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let image_url = if image_url.is_object() {
                            image_url
                        } else {
                            json!({"url": image_url})
                        };
                        converted.push(json!({"type": "image_url", "image_url": image_url}));
                    }
                    Some("input_file") => {
                        return Err(AdapterError::Unsupported(
                            "Chat Completions file parts require a provider-specific extension"
                                .into(),
                        ));
                    }
                    Some(other) => {
                        return Err(AdapterError::InvalidRequest(format!(
                            "unsupported message content part `{other}`"
                        )));
                    }
                    None => {
                        return Err(AdapterError::InvalidRequest(
                            "message content part is missing `type`".into(),
                        ));
                    }
                }
            }
            if converted.len() == 1 && converted[0].get("type") == Some(&json!("text")) {
                Ok(converted[0]
                    .get("text")
                    .cloned()
                    .unwrap_or(Value::String(String::new())))
            } else {
                Ok(Value::Array(converted))
            }
        }
        Some(_) => Err(AdapterError::InvalidRequest(
            "message content must be a string or array".into(),
        )),
    }
}

fn append_function_call(messages: &mut Vec<Value>, item: &Map<String, Value>) -> Result<()> {
    let name = required_string(item, "name")?;
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::InvalidRequest("function_call is missing `call_id`".into()))?;
    let tool_call = json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": normalize_arguments(item.get("arguments")),
        },
    });

    if let Some(last) = messages.last_mut().and_then(Value::as_object_mut)
        && last.get("role").and_then(Value::as_str) == Some("assistant")
        && last.get("content").is_some_and(Value::is_null)
        && let Some(calls) = last.get_mut("tool_calls").and_then(Value::as_array_mut)
    {
        calls.push(tool_call);
        return Ok(());
    }
    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [tool_call],
    }));
    Ok(())
}

fn flush_pending_reasoning(messages: &mut Vec<Value>, pending: &mut String) {
    if !pending.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": std::mem::take(pending),
        }));
    }
}

fn attach_pending_reasoning(message: &mut Value, pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    if let Some(message) = message.as_object_mut() {
        message.insert(
            "reasoning_content".into(),
            Value::String(std::mem::take(pending)),
        );
    }
}

fn convert_tool_choice(choice: &Value) -> Value {
    if let Some(object) = choice.as_object()
        && object.get("type").and_then(Value::as_str) == Some("function")
    {
        return json!({
            "type": "function",
            "function": {
                "name": object.get("name").cloned().unwrap_or(Value::Null),
            },
        });
    }
    choice.clone()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatFinish {
    Completed,
    Incomplete(&'static str),
}

fn chat_finish(reason: Option<&str>) -> Result<Option<ChatFinish>> {
    match reason {
        None => Ok(None),
        Some("stop" | "tool_calls" | "function_call") => Ok(Some(ChatFinish::Completed)),
        Some("length" | "max_tokens") => Ok(Some(ChatFinish::Incomplete("max_output_tokens"))),
        Some("content_filter" | "sensitive") => Ok(Some(ChatFinish::Incomplete("content_filter"))),
        Some("network_error") => Err(AdapterError::MalformedUpstream(
            "provider terminated generation with `network_error`".into(),
        )),
        Some("context_length_exceeded" | "context_exceeded") => {
            Err(AdapterError::MalformedUpstream(
                "provider reported that the input context limit was exceeded".into(),
            ))
        }
        Some(other) => Err(AdapterError::MalformedUpstream(format!(
            "unsupported Chat Completions finish_reason `{other}`"
        ))),
    }
}

impl ProviderAdapter for OpenAiChatCompletionsAdapter {
    fn preset(&self) -> &ProviderPreset {
        &self.preset
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/chat/completions".to_owned()
    }

    #[allow(clippy::too_many_lines)]
    fn encode_request(&self, request: &Value) -> Result<Value> {
        let request = require_object(request, "Responses request")?;
        let model = required_string(request, "model")?;
        let mut messages = Vec::new();
        let mut pending_reasoning = String::new();
        let mut additional_tools: Vec<Value> = Vec::new();
        for (role, content) in response_instructions(request)? {
            messages.push(json!({"role": role, "content": content}));
        }

        for item in response_input_items(request)? {
            let object = item.as_object().ok_or_else(|| {
                AdapterError::InvalidRequest("each input item must be an object".into())
            })?;
            match item_type(&item) {
                Some("message") => {
                    let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
                    let mut message = json!({
                        "role": role,
                        "content": chat_content(object.get("content"))?,
                    });
                    if role == "assistant" {
                        if let Some(content) = message.get_mut("content") {
                            scrub_fake_exec_calls(content);
                        }
                        attach_pending_reasoning(&mut message, &mut pending_reasoning);
                    } else {
                        flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                    }
                    messages.push(message);
                }
                Some("function_call") => {
                    append_function_call(&mut messages, object)?;
                    if let Some(message) = messages.last_mut() {
                        attach_pending_reasoning(message, &mut pending_reasoning);
                    }
                }
                // Custom tools carry a freeform text `input`; map the
                // call/output pair onto the same assistant tool_calls and tool
                // message shape as function calls so history stays paired.
                Some("custom_tool_call") => {
                    let mut normalized = object.clone();
                    if !normalized.contains_key("arguments") {
                        if let Some(input) = normalized.get("input").cloned() {
                            normalized.insert("arguments".into(), input);
                        }
                    }
                    append_function_call(&mut messages, &normalized)?;
                    if let Some(message) = messages.last_mut() {
                        attach_pending_reasoning(message, &mut pending_reasoning);
                    }
                }
                Some("custom_tool_call_output") => {
                    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                    let call_id =
                        object
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                AdapterError::InvalidRequest(
                                    "custom_tool_call_output is missing `call_id`".into(),
                                )
                            })?;
                    let output = output_value(object.get("output"));
                    let content = output
                        .as_str()
                        .map_or_else(|| output.to_string(), str::to_owned);
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content,
                    }));
                }
                Some("function_call_output") => {
                    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
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
                    let content = output
                        .as_str()
                        .map_or_else(|| output.to_string(), str::to_owned);
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content,
                    }));
                }
                Some("reasoning") => {
                    if let Some(reasoning) =
                        provider_reasoning_payload(object, &self.preset.id, CHAT_REASONING_FORMAT)
                            .and_then(|payload| payload.get("reasoning_content"))
                            .and_then(Value::as_str)
                    {
                        pending_reasoning.push_str(reasoning);
                    }
                }
                // Opaque compaction is owned by the native Responses path. It is
                // never reinterpreted as an assistant message.
                Some("compaction") => {}
                // Newer Codex clients attach extra tool definitions as an
                // `additional_tools` input item. Collect the embedded tools so
                // the external chat conversion still advertises them; items
                // without a tools array are skipped instead of failing the turn.
                Some("additional_tools") => {
                    if let Some(embedded) = object.get("tools").and_then(Value::as_array) {
                        additional_tools.extend(embedded.iter().cloned());
                    }
                }
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
        flush_pending_reasoning(&mut messages, &mut pending_reasoning);

        let mut body = Map::new();
        body.insert("model".into(), Value::String(model.to_owned()));
        body.insert("messages".into(), Value::Array(messages));
        copy_if_present(request, &mut body, "stream", "stream");
        copy_if_present(request, &mut body, "max_output_tokens", "max_tokens");
        copy_if_present(request, &mut body, "temperature", "temperature");
        copy_if_present(request, &mut body, "top_p", "top_p");
        copy_if_present(request, &mut body, "seed", "seed");
        copy_if_present(
            request,
            &mut body,
            "parallel_tool_calls",
            "parallel_tool_calls",
        );

        let mut flat_tools: Vec<_> = function_tools(request).into_iter().cloned().collect();
        flat_tools.extend(
            additional_tools
                .iter()
                .filter_map(Value::as_object)
                // Custom string tools carry no `parameters`; they are advertised
                // through the synthesized single-input schemas below instead of
                // empty-parameter functions the model cannot call usefully.
                .filter(|tool| tool.get("type").and_then(Value::as_str) != Some("custom"))
                .filter(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty())
                })
                .cloned(),
        );
        // Advertise the harness's custom tools (e.g. `exec`) as callable
        // single-input string functions so the model uses native tool calls
        // instead of falling back to provider-specific `<exec>` text blocks
        // that the client would never execute.
        let mut custom_names = std::collections::BTreeSet::new();
        let custom_tool_schemas: Vec<Map<String, Value>> = custom_string_tools(request)
            .into_iter()
            .chain(
                additional_tools
                    .iter()
                    .filter_map(Value::as_object)
                    .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom")),
            )
            .filter(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.is_empty() && custom_names.insert(name.to_owned()))
            })
            .map(|tool| {
                let mut schema = Map::new();
                schema.insert(
                    "name".into(),
                    tool.get("name").cloned().unwrap_or(Value::Null),
                );
                schema.insert(
                    "description".into(),
                    tool.get("description").cloned().unwrap_or(Value::Null),
                );
                schema.insert(
                    "parameters".into(),
                    json!({
                        "type": "object",
                        "properties": {"input": {"type": "string"}},
                        "required": ["input"],
                    }),
                );
                schema
            })
            .collect();
        flat_tools.extend(custom_tool_schemas);
        let mut tools = flat_tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.get("name").cloned().unwrap_or(Value::Null),
                        "description": tool.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({})),
                        "strict": tool.get("strict").cloned().unwrap_or(Value::Null),
                    },
                })
            })
            .collect::<Vec<_>>();
        let mut converted_choice = request.get("tool_choice").map(convert_tool_choice);
        if self.preset.id == "zhipu"
            && let Some(choice) = request.get("tool_choice")
        {
            match choice.as_str() {
                Some("auto") => converted_choice = Some(json!("auto")),
                Some("none") => {
                    // Zhipu's Coding Plan endpoint only accepts `auto`. Removing
                    // the tool definitions faithfully simulates `none`.
                    tools.clear();
                    converted_choice = None;
                }
                Some("required") => {
                    return Err(AdapterError::Unsupported(
                        "Zhipu Coding Plan does not support tool_choice `required`".into(),
                    ));
                }
                _ if choice.get("type").and_then(Value::as_str) == Some("function") => {
                    return Err(AdapterError::Unsupported(
                        "Zhipu Coding Plan does not support a specific function tool_choice".into(),
                    ));
                }
                _ => {
                    return Err(AdapterError::InvalidRequest(
                        "invalid tool_choice for Zhipu Coding Plan".into(),
                    ));
                }
            }
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(choice) = converted_choice {
            body.insert("tool_choice".into(), choice);
        }
        apply_overrides(&mut body, &self.preset.request_overrides)?;
        Ok(Value::Object(body))
    }

    fn decode_response(&self, response: &Value, response_id: &str) -> Result<Value> {
        let response = require_upstream_object(response, "Chat Completions response")?;
        let model = response
            .get("model")
            .and_then(Value::as_str)
            .or(self.preset.default_model.as_deref())
            .unwrap_or("unknown");
        let choice = response
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(Value::as_object)
            .ok_or_else(|| {
                AdapterError::MalformedUpstream(
                    "Chat Completions response has no first choice".into(),
                )
            })?;
        let message = choice
            .get("message")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                AdapterError::MalformedUpstream(
                    "Chat Completions response has no first choice message".into(),
                )
            })?;

        let mut output = Vec::new();
        let reasoning = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !reasoning.is_empty() {
            output.push(reasoning_output_with_provenance(
                &format!("rs_{response_id}_{}", output.len()),
                reasoning,
                &self.preset.id,
                CHAT_REASONING_FORMAT,
                &json!({"reasoning_content": reasoning}),
            ));
        }

        let text = text_from_content(message.get("content"));
        if !text.is_empty() {
            output.push(message_output(
                &format!("msg_{response_id}_{}", output.len()),
                &text,
            ));
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let function = tool_call
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        AdapterError::MalformedUpstream(
                            "tool call is missing its `function` object".into(),
                        )
                    })?;
                let name = required_upstream_string(function, "name", "tool call function")?;
                let tool_call = tool_call.as_object().ok_or_else(|| {
                    AdapterError::MalformedUpstream("tool call must be an object".into())
                })?;
                let call_id = required_upstream_string(tool_call, "id", "tool call")?;
                let arguments =
                    required_upstream_json_arguments(function, "arguments", "tool call function")?;
                output.push(function_call_output(
                    &format!("fc_{response_id}_{}", output.len()),
                    call_id,
                    name,
                    arguments,
                ));
            }
        }

        let finish =
            chat_finish(choice.get("finish_reason").and_then(Value::as_str))?.ok_or_else(|| {
                AdapterError::MalformedUpstream(
                    "Chat Completions response has no terminal finish_reason".into(),
                )
            })?;
        let usage = chat_usage(response.get("usage"));
        Ok(match finish {
            ChatFinish::Completed => response_object(
                response_id,
                model,
                response.get("created").and_then(Value::as_u64),
                &output,
                usage.as_ref(),
            ),
            ChatFinish::Incomplete(reason) => incomplete_response_object(
                response_id,
                model,
                response.get("created").and_then(Value::as_u64),
                &output,
                usage.as_ref(),
                reason,
            ),
        })
    }

    fn decode_stream_chunk(&self, state: &mut StreamState, chunk: &Value) -> Result<Vec<Value>> {
        let chunk = require_upstream_object(chunk, "Chat Completions stream chunk")?;
        if let Some(error) = chunk.get("error") {
            let sequence_number = state.take_sequence_number();
            state.completed = true;
            return Ok(vec![
                ResponseEvent::Error {
                    code: error
                        .get("code")
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

        if let Some(usage) = chat_usage(chunk.get("usage")) {
            state.usage = Some(usage);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(Vec::new());
        };
        let delta = choice
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                AdapterError::MalformedUpstream("stream choice has no `delta` object".into())
            })?;
        let mut events = Vec::new();

        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            push_reasoning_delta(state, reasoning, &mut events)?;
            let full_reasoning = streamed_reasoning_text(state)
                .unwrap_or_default()
                .to_owned();
            set_reasoning_provenance(
                state,
                &self.preset.id,
                CHAT_REASONING_FORMAT,
                json!({"reasoning_content": full_reasoning}),
            );
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            push_text_delta(state, text, &mut events)?;
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (fallback_index, tool_call) in tool_calls.iter().enumerate() {
                let index = tool_call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(fallback_index);
                let function = tool_call.get("function").and_then(Value::as_object);
                ensure_tool(
                    state,
                    index,
                    tool_call.get("id").and_then(Value::as_str),
                    function
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str),
                    &mut events,
                )?;
                if let Some(arguments) = function
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    push_tool_delta(state, index, arguments, &mut events)?;
                }
            }
        }
        match chat_finish(choice.get("finish_reason").and_then(Value::as_str))? {
            Some(ChatFinish::Completed) => events.extend(finish_events(state)?),
            Some(ChatFinish::Incomplete(reason)) => {
                events.extend(finish_incomplete_events(state, reason)?);
            }
            None => {}
        }
        Ok(events)
    }

    fn finish_stream(&self, state: &mut StreamState) -> Result<Vec<Value>> {
        if state.is_completed() {
            Ok(Vec::new())
        } else {
            Err(AdapterError::MalformedUpstream(
                "Chat Completions stream ended without a terminal finish_reason".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::preset_by_id;

    fn adapter(id: &str) -> OpenAiChatCompletionsAdapter {
        OpenAiChatCompletionsAdapter::new(preset_by_id(id).unwrap())
    }

    #[test]
    fn additional_tools_custom_tools_get_single_input_schemas() {
        let request = json!({
            "model": "glm-5.3",
            "input": [
                {"type":"additional_tools","role":"developer","tools":[
                    {"type":"custom","name":"exec","description":"Run JavaScript code to orchestrate tool calls"},
                    {"type":"function","name":"wait","description":"Waits","parameters":{"type":"object","properties":{"call_id":{"type":"string"}}}}
                ]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"run a command"}]}
            ]
        });
        let converted = adapter("zhipu").encode_request(&request).unwrap();
        let tools = converted["tools"].as_array().expect("tools advertised");
        let by_name = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .unwrap_or_else(|| panic!("tool {name} missing"))
                .clone()
        };
        let exec = by_name("exec");
        assert_eq!(
            exec["function"]["parameters"]["properties"]["input"]["type"],
            "string"
        );
        assert_eq!(exec["function"]["parameters"]["required"][0], "input");
        assert_eq!(
            exec["function"]["description"],
            "Run JavaScript code to orchestrate tool calls"
        );
        let wait = by_name("wait");
        assert_eq!(
            wait["function"]["parameters"]["properties"]["call_id"]["type"],
            "string"
        );
        assert_eq!(tools.len(), 2, "no duplicate tool entries");
    }

    #[test]
    fn scrubs_literal_exec_blocks_from_replayed_assistant_history() {
        let request = json!({
            "model": "glm-5.3",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"check models"}]},
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"我继续执行查询，马上给你结果。<exec>脚本内容…</exec>"}
                ]},
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"<exec>Invoke-RestMethod 'https://example.invalid'</exec>"}
                ]},
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"no tags here"}
                ]}
            ]
        });
        let converted = adapter("zhipu").encode_request(&request).unwrap();
        let body = converted.to_string();
        assert!(
            !body.contains("<exec>"),
            "literal exec tags must not reach the provider"
        );
        assert_eq!(
            converted["messages"][1]["content"],
            json!("我继续执行查询，马上给你结果。")
        );
        assert_eq!(converted["messages"][2]["content"], json!(""));
        assert_eq!(converted["messages"][3]["content"], json!("no tags here"));
    }

    #[test]
    fn converts_tool_round_trip_and_drops_opaque_reasoning_input() {
        let request = json!({
            "model": "glm-5.2",
            "instructions": "be exact",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"weather"}]},
                {"type":"reasoning","encrypted_content":"must-not-leak"},
                {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Paris\"}"},
                {"type":"function_call_output","call_id":"call_1","output":{"weather":"sunny"}}
            ],
            "tools": [{"type":"function","name":"weather","description":"Weather","parameters":{"type":"object"}}],
            "max_output_tokens": 1024,
            "stream": true
        });
        let converted = adapter("zhipu").encode_request(&request).unwrap();
        assert_eq!(
            converted["thinking"],
            json!({"type":"enabled","clear_thinking":false})
        );
        assert_eq!(converted["max_tokens"], 1024);
        assert_eq!(converted["messages"][3]["role"], "tool");
        assert_eq!(converted["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(
            converted["messages"][3]["content"],
            "{\"weather\":\"sunny\"}"
        );
        assert!(!converted.to_string().contains("must-not-leak"));
    }

    #[test]
    fn separates_reasoning_from_visible_assistant_text() {
        let response = json!({
            "model": "glm-5.2",
            "choices": [{"message": {
                "role": "assistant",
                "reasoning_content": "private thought",
                "content": "visible answer"
            }, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        });
        let converted = adapter("zhipu")
            .decode_response(&response, "resp_1")
            .unwrap();
        assert_eq!(converted["output"][0]["type"], "reasoning");
        assert_eq!(converted["output"][1]["type"], "message");
        assert!(
            !converted["output"][1]
                .to_string()
                .contains("private thought")
        );
        assert_eq!(
            converted["output"][0]["provider_metadata"]["source_provider_id"],
            "zhipu"
        );

        let same_provider_request = json!({
            "model": "glm-5.2",
            "input": converted["output"].clone(),
        });
        let replayed = adapter("zhipu")
            .encode_request(&same_provider_request)
            .unwrap();
        assert_eq!(
            replayed["messages"][0]["reasoning_content"],
            "private thought"
        );
        assert_eq!(replayed["messages"][0]["content"], "visible answer");

        let switched_request = json!({
            "model": "deepseek-chat",
            "input": converted["output"].clone(),
        });
        let switched = adapter("deepseek")
            .encode_request(&switched_request)
            .unwrap();
        assert_eq!(switched["messages"][0]["content"], "visible answer");
        assert!(switched["messages"][0].get("reasoning_content").is_none());
        assert!(!switched.to_string().contains("private thought"));
    }

    #[test]
    fn joins_streamed_tool_arguments() {
        let adapter = adapter("zhipu");
        let mut state = StreamState::new("resp_2", "glm-5.2");
        let first = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"call_9","function":{"name":"lookup","arguments":"{\"q\":"}
        }]},"finish_reason":null}]});
        let second = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"function":{"arguments":"\"rust\"}"}
        }]},"finish_reason":"tool_calls"}]});
        let mut events = adapter.decode_stream_chunk(&mut state, &first).unwrap();
        events.extend(adapter.decode_stream_chunk(&mut state, &second).unwrap());
        let done = events
            .iter()
            .find(|event| event["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(done["arguments"], "{\"q\":\"rust\"}");
    }

    #[test]
    fn streamed_reasoning_is_replayable_only_by_its_source_provider() {
        let zhipu = adapter("zhipu");
        let mut state = StreamState::new("resp_stream_reasoning", "glm-5.2");
        let first = json!({"choices":[{"delta":{
            "reasoning_content":"private "
        },"finish_reason":null}]});
        let second = json!({"choices":[{"delta":{
            "reasoning_content":"thought",
            "content":"visible answer"
        },"finish_reason":"stop"}]});

        let mut events = zhipu.decode_stream_chunk(&mut state, &first).unwrap();
        events.extend(zhipu.decode_stream_chunk(&mut state, &second).unwrap());
        let completed = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        let output = completed["response"]["output"].clone();
        assert_eq!(
            output[0]["provider_metadata"]["source_provider_id"],
            "zhipu"
        );
        assert_eq!(
            output[0]["provider_metadata"]["payload"]["reasoning_content"],
            "private thought"
        );

        let replayed = zhipu
            .encode_request(&json!({"model":"glm-5.2","input":output.clone()}))
            .unwrap();
        assert_eq!(
            replayed["messages"][0]["reasoning_content"],
            "private thought"
        );
        assert_eq!(replayed["messages"][0]["content"], "visible answer");

        let switched = adapter("deepseek")
            .encode_request(&json!({"model":"deepseek-chat","input":output}))
            .unwrap();
        assert!(switched["messages"][0].get("reasoning_content").is_none());
        assert!(!switched.to_string().contains("private thought"));
    }
}
