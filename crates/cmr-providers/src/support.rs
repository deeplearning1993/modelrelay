use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::types::{
    AdapterError, ReasoningProvenance, ReasoningState, ResponseEvent, Result, StreamState,
    TextState, ToolState, Usage,
};

enum FinishedOutput {
    Text(TextState),
    Reasoning(ReasoningState),
    Tool(ToolState),
}

pub(crate) fn require_object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| AdapterError::InvalidRequest(format!("{what} must be a JSON object")))
}

pub(crate) fn require_upstream_object<'a>(
    value: &'a Value,
    what: &str,
) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| AdapterError::MalformedUpstream(format!("{what} must be a JSON object")))
}

pub(crate) fn required_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AdapterError::InvalidRequest(format!("missing or invalid `{field}`")))
}

pub(crate) fn required_upstream_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    what: &str,
) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AdapterError::MalformedUpstream(format!(
                "{what} is missing a non-empty string `{field}`"
            ))
        })
}

pub(crate) fn required_upstream_json_arguments<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    what: &str,
) -> Result<&'a str> {
    let arguments = required_upstream_string(object, field, what)?;
    let parsed: Value = serde_json::from_str(arguments).map_err(|error| {
        AdapterError::MalformedUpstream(format!("{what} contains invalid JSON `{field}`: {error}"))
    })?;
    if !parsed.is_object() {
        return Err(AdapterError::MalformedUpstream(format!(
            "{what} `{field}` must encode a JSON object"
        )));
    }
    Ok(arguments)
}

pub(crate) fn response_input_items(request: &Map<String, Value>) -> Result<Vec<Value>> {
    match request.get("input") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        })]),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(AdapterError::InvalidRequest(
            "`input` must be a string or an array of Responses items".into(),
        )),
    }
}

/// Normalizes both Responses `instructions` wire forms while preserving the
/// order of developer/system messages. Converted protocols cannot faithfully
/// represent other item kinds in this field, so reject them locally.
pub(crate) fn response_instructions(request: &Map<String, Value>) -> Result<Vec<(String, String)>> {
    match request.get("instructions") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![("system".to_owned(), text.clone())]),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let object = item.as_object().ok_or_else(|| {
                    AdapterError::InvalidRequest(format!(
                        "instructions[{index}] must be a message object"
                    ))
                })?;
                if object.get("type").and_then(Value::as_str) != Some("message") {
                    return Err(AdapterError::Unsupported(format!(
                        "instructions[{index}] must have type `message`"
                    )));
                }
                let role = object.get("role").and_then(Value::as_str).ok_or_else(|| {
                    AdapterError::InvalidRequest(format!("instructions[{index}] is missing `role`"))
                })?;
                if !matches!(role, "system" | "developer") {
                    return Err(AdapterError::Unsupported(format!(
                        "instructions[{index}] role `{role}` cannot be represented by this provider"
                    )));
                }
                let text =
                    strict_text_content(object.get("content")).map_err(|error| match error {
                        AdapterError::InvalidRequest(message) => {
                            AdapterError::InvalidRequest(format!("instructions[{index}] {message}"))
                        }
                        other => other,
                    })?;
                Ok((role.to_owned(), text))
            })
            .collect(),
        Some(_) => Err(AdapterError::InvalidRequest(
            "`instructions` must be a string or an array of developer/system messages".into(),
        )),
    }
}

fn strict_text_content(content: Option<&Value>) -> Result<String> {
    match content {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).ok_or_else(|| {
                    AdapterError::InvalidRequest(
                        "contains a content part without a string `type`".into(),
                    )
                })?;
                if !matches!(kind, "input_text" | "output_text" | "text") {
                    return Err(AdapterError::Unsupported(format!(
                        "contains unsupported instruction content part `{kind}`"
                    )));
                }
                let part_text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                    AdapterError::InvalidRequest(format!(
                        "contains `{kind}` content without string `text`"
                    ))
                })?;
                text.push_str(part_text);
            }
            Ok(text)
        }
        _ => Err(AdapterError::InvalidRequest(
            "must contain string or text-part-array `content`".into(),
        )),
    }
}

pub(crate) fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

pub(crate) fn text_from_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let kind = part.get("type").and_then(Value::as_str);
                if matches!(kind, Some("input_text" | "output_text" | "text")) {
                    part.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub(crate) fn copy_if_present(
    from: &Map<String, Value>,
    to: &mut Map<String, Value>,
    source: &str,
    target: &str,
) {
    if let Some(value) = from.get(source) {
        to.insert(target.to_owned(), value.clone());
    }
}

const FAKE_EXEC_OPEN: &str = "<exec>";
const FAKE_EXEC_CLOSE: &str = "</exec>";

/// Remove literal `<exec>...</exec>` blocks from replayed assistant text.
///
/// When a model lacks the client's custom tools it sometimes writes the
/// intended shell command as an `<exec>` tag inside plain prose. Replaying
/// those blocks teaches the model to keep emitting tool calls as text
/// instead of using the advertised function tools, so history scrubbing
/// removes the tag and its payload while keeping the surrounding prose.
pub(crate) fn strip_fake_exec_text(text: &str) -> String {
    if !text.contains(FAKE_EXEC_OPEN) {
        return text.to_owned();
    }
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(FAKE_EXEC_OPEN) {
        cleaned.push_str(&rest[..start]);
        let after_open = &rest[start + FAKE_EXEC_OPEN.len()..];
        match after_open.find(FAKE_EXEC_CLOSE) {
            Some(end) => rest = &after_open[end + FAKE_EXEC_CLOSE.len()..],
            // Unterminated block: drop the dangling remainder.
            None => rest = "",
        }
    }
    cleaned.push_str(rest);
    cleaned
}

/// Apply [`strip_fake_exec_text`] to converted assistant message content —
/// a plain string or an array of parts carrying a `text` field. Text parts
/// that scrub to empty are dropped; if nothing remains the content becomes
/// an empty string so callers can skip the message entirely.
pub(crate) fn scrub_fake_exec_calls(content: &mut Value) {
    match content {
        Value::String(text) => *text = strip_fake_exec_text(text),
        Value::Array(parts) => {
            parts.retain_mut(|part| match part.get_mut("text") {
                Some(Value::String(text)) => {
                    *text = strip_fake_exec_text(text);
                    !text.is_empty() || item_type(part) != Some("text")
                }
                _ => true,
            });
            if parts.is_empty() {
                *content = Value::String(String::new());
            }
        }
        _ => {}
    }
}

pub(crate) fn apply_overrides(body: &mut Map<String, Value>, overrides: &Value) -> Result<()> {
    if overrides.is_null() {
        return Ok(());
    }
    let overrides = overrides.as_object().ok_or_else(|| {
        AdapterError::InvalidPreset("request_overrides must be a JSON object".into())
    })?;
    for (key, value) in overrides {
        body.insert(key.clone(), value.clone());
    }
    Ok(())
}

pub(crate) fn normalize_arguments(arguments: Option<&Value>) -> String {
    match arguments {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => "{}".to_owned(),
    }
}

pub(crate) fn parse_arguments(arguments: Option<&Value>) -> Result<Value> {
    match arguments {
        Some(Value::String(value)) => serde_json::from_str(value).map_err(|error| {
            AdapterError::InvalidRequest(format!("function arguments are not valid JSON: {error}"))
        }),
        Some(value @ Value::Object(_)) => Ok(value.clone()),
        None => Ok(json!({})),
        Some(_) => Err(AdapterError::InvalidRequest(
            "function arguments must be a JSON object or encoded object".into(),
        )),
    }
}

pub(crate) fn output_value(output: Option<&Value>) -> Value {
    match output {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(value) => value.clone(),
        None => Value::String(String::new()),
    }
}

pub(crate) fn response_object(
    response_id: &str,
    model: &str,
    created_at: Option<u64>,
    output: &[Value],
    usage: Option<&Usage>,
) -> Value {
    response_object_with_status(
        response_id,
        model,
        created_at,
        "completed",
        output,
        usage,
        None,
    )
}

pub(crate) fn incomplete_response_object(
    response_id: &str,
    model: &str,
    created_at: Option<u64>,
    output: &[Value],
    usage: Option<&Usage>,
    reason: &str,
) -> Value {
    response_object_with_status(
        response_id,
        model,
        created_at,
        "incomplete",
        output,
        usage,
        Some(reason),
    )
}

fn response_usage_value(usage: Option<&Usage>) -> Value {
    usage.map_or(Value::Null, |usage| {
        json!({
            "input_tokens": usage.input_tokens,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": usage.output_tokens,
            "output_tokens_details": {
                "reasoning_tokens": usage.reasoning_tokens.unwrap_or(0)
            },
            "total_tokens": usage.total_tokens,
        })
    })
}

fn response_object_with_status(
    response_id: &str,
    model: &str,
    created_at: Option<u64>,
    status: &str,
    output: &[Value],
    usage: Option<&Usage>,
    incomplete_reason: Option<&str>,
) -> Value {
    let created_at = created_at.unwrap_or(0);
    let completed_at = (status == "completed").then_some(created_at);
    json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "completed_at": completed_at,
        "error": null,
        "incomplete_details": incomplete_reason.map(|reason| json!({"reason": reason})),
        "instructions": null,
        "max_output_tokens": null,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": null, "summary": null},
        "store": false,
        "temperature": null,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": null,
        "truncation": "disabled",
        "usage": response_usage_value(usage),
        "metadata": {},
    })
}

pub(crate) fn message_output(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": [],
        }],
    })
}

pub(crate) fn reasoning_output(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "reasoning",
        "status": "completed",
        "summary": [{"type": "summary_text", "text": text}],
    })
}

pub(crate) fn reasoning_output_with_provenance(
    id: &str,
    text: &str,
    source_provider_id: &str,
    format: &str,
    payload: &Value,
) -> Value {
    let mut item = reasoning_output(id, text);
    item.as_object_mut()
        .expect("reasoning_output always returns an object")
        .insert(
            "provider_metadata".into(),
            json!({
                "cmr_provider_owner_id": source_provider_id,
                "source_provider_id": source_provider_id,
                "format": format,
                "payload": payload,
            }),
        );
    item
}

pub(crate) fn provider_reasoning_payload<'a>(
    item: &'a Map<String, Value>,
    source_provider_id: &str,
    format: &str,
) -> Option<&'a Value> {
    let metadata = item.get("provider_metadata")?.as_object()?;
    (metadata
        .get("cmr_provider_owner_id")
        .or_else(|| metadata.get("source_provider_id"))
        .and_then(Value::as_str)
        == Some(source_provider_id)
        && metadata.get("format").and_then(Value::as_str) == Some(format))
    .then(|| metadata.get("payload"))
    .flatten()
}

pub(crate) fn set_reasoning_provenance(
    state: &mut StreamState,
    source_provider_id: &str,
    format: &str,
    payload: Value,
) {
    if let Some(reasoning) = state.reasoning.as_mut() {
        reasoning.provenance = Some(ReasoningProvenance {
            source_provider_id: source_provider_id.to_owned(),
            format: format.to_owned(),
            payload,
        });
    }
}

pub(crate) fn streamed_reasoning_text(state: &StreamState) -> Option<&str> {
    state
        .reasoning
        .as_ref()
        .map(|reasoning| reasoning.text.as_str())
}

pub(crate) fn function_call_output(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

/// Builds the completed item for a call to a harness custom string tool.
/// The upstream arguments are a JSON object such as `{"input": "..."}`;
/// the client executes the extracted `input` string as the tool payload.
pub(crate) fn custom_tool_call_output(
    id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Value {
    json!({
        "id": id,
        "type": "custom_tool_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "input": custom_tool_input(arguments),
    })
}

fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| arguments.to_owned())
}

fn event(event: ResponseEvent) -> Result<Value> {
    event.into_json()
}

pub(crate) fn ensure_started(state: &mut StreamState, events: &mut Vec<Value>) -> Result<()> {
    if !state.started {
        state.started = true;
        let response = response_object_with_status(
            &state.response_id,
            &state.model,
            Some(0),
            "in_progress",
            &[],
            None,
            None,
        );
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::Created {
            response: response.clone(),
            sequence_number,
        })?);
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::InProgress {
            response,
            sequence_number,
        })?);
    }
    Ok(())
}

pub(crate) fn push_text_delta(
    state: &mut StreamState,
    delta: &str,
    events: &mut Vec<Value>,
) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    ensure_started(state, events)?;
    if state.text.is_none() {
        let output_index = state.next_output_index;
        state.next_output_index += 1;
        let id = format!("msg_{}_{output_index}", state.response_id);
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::OutputItemAdded {
            output_index,
            item: json!({
                "id": id.clone(),
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            }),
            sequence_number,
        })?);
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::ContentPartAdded {
            item_id: id.clone(),
            output_index,
            content_index: 0,
            part: json!({
                "type": "output_text",
                "text": "",
                "annotations": [],
                "logprobs": [],
            }),
            sequence_number,
        })?);
        state.text = Some(TextState {
            id,
            output_index,
            text: String::new(),
        });
    }
    let text = state.text.as_mut().expect("text state initialized");
    text.text.push_str(delta);
    let item_id = text.id.clone();
    let output_index = text.output_index;
    let sequence_number = state.take_sequence_number();
    events.push(event(ResponseEvent::OutputTextDelta {
        item_id,
        output_index,
        content_index: 0,
        delta: delta.to_owned(),
        logprobs: Vec::new(),
        sequence_number,
    })?);
    Ok(())
}

pub(crate) fn push_reasoning_delta(
    state: &mut StreamState,
    delta: &str,
    events: &mut Vec<Value>,
) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    ensure_reasoning_item(state, events)?;
    let reasoning = state
        .reasoning
        .as_mut()
        .expect("reasoning state initialized");
    reasoning.text.push_str(delta);
    let item_id = reasoning.id.clone();
    let output_index = reasoning.output_index;
    let sequence_number = state.take_sequence_number();
    events.push(event(ResponseEvent::ReasoningDelta {
        item_id,
        output_index,
        summary_index: 0,
        delta: delta.to_owned(),
        sequence_number,
    })?);
    Ok(())
}

pub(crate) fn ensure_reasoning_item(
    state: &mut StreamState,
    events: &mut Vec<Value>,
) -> Result<()> {
    ensure_started(state, events)?;
    if state.reasoning.is_none() {
        let output_index = state.next_output_index;
        state.next_output_index += 1;
        let id = format!("rs_{}_{output_index}", state.response_id);
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::OutputItemAdded {
            output_index,
            item: json!({
                "id": id.clone(),
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
            }),
            sequence_number,
        })?);
        let sequence_number = state.take_sequence_number();
        events.push(event(ResponseEvent::ReasoningSummaryPartAdded {
            item_id: id.clone(),
            output_index,
            summary_index: 0,
            part: json!({"type": "summary_text", "text": ""}),
            sequence_number,
        })?);
        state.reasoning = Some(ReasoningState {
            id,
            output_index,
            text: String::new(),
            provenance: None,
        });
    }
    Ok(())
}

pub(crate) fn ensure_tool(
    state: &mut StreamState,
    provider_index: usize,
    call_id: Option<&str>,
    name: Option<&str>,
    events: &mut Vec<Value>,
) -> Result<()> {
    ensure_started(state, events)?;
    if let Some(tool) = state.tools.get_mut(&provider_index) {
        if let Some(call_id) = call_id.filter(|value| !value.is_empty()) {
            call_id.clone_into(&mut tool.call_id);
        }
        if let Some(name) = name.filter(|value| !value.is_empty()) {
            name.clone_into(&mut tool.name);
        }
        return Ok(());
    }

    let output_index = state.next_output_index;
    state.next_output_index += 1;
    let id = format!("fc_{}_{output_index}", state.response_id);
    let call_id = call_id.filter(|value| !value.is_empty()).map_or_else(
        || format!("call_{}_{output_index}", state.response_id),
        str::to_owned,
    );
    let name = name.unwrap_or_default().to_owned();
    // Calls to the harness's custom string tools must come back as
    // `custom_tool_call` items; `function_call` items for those tools are
    // never dispatched by the client.
    let custom = !name.is_empty() && state.custom_tools.contains(&name);
    let sequence_number = state.take_sequence_number();
    events.push(event(ResponseEvent::OutputItemAdded {
        output_index,
        item: if custom {
            json!({
                "id": id.clone(),
                "type": "custom_tool_call",
                "status": "in_progress",
                "call_id": call_id.clone(),
                "name": name.clone(),
                "input": "",
            })
        } else {
            json!({
                "id": id.clone(),
                "type": "function_call",
                "status": "in_progress",
                "call_id": call_id.clone(),
                "name": name.clone(),
                "arguments": "",
            })
        },
        sequence_number,
    })?);
    state.tools.insert(
        provider_index,
        ToolState {
            id,
            call_id,
            name,
            output_index,
            arguments: String::new(),
            custom,
        },
    );
    Ok(())
}

pub(crate) fn push_tool_delta(
    state: &mut StreamState,
    provider_index: usize,
    delta: &str,
    events: &mut Vec<Value>,
) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    ensure_tool(state, provider_index, None, None, events)?;
    let tool = state
        .tools
        .get_mut(&provider_index)
        .expect("tool state initialized");
    tool.arguments.push_str(delta);
    if tool.custom {
        // Custom tool calls are delivered whole in the output_item.done item;
        // there is no function-arguments delta stream for them.
        return Ok(());
    }
    let item_id = tool.id.clone();
    let output_index = tool.output_index;
    let sequence_number = state.take_sequence_number();
    events.push(event(ResponseEvent::FunctionCallArgumentsDelta {
        item_id,
        output_index,
        delta: delta.to_owned(),
        sequence_number,
    })?);
    Ok(())
}

pub(crate) fn finish_events(state: &mut StreamState) -> Result<Vec<Value>> {
    finish_events_with_status(state, None)
}

pub(crate) fn finish_incomplete_events(
    state: &mut StreamState,
    reason: &str,
) -> Result<Vec<Value>> {
    finish_events_with_status(state, Some(reason))
}

#[allow(clippy::too_many_lines)]
fn finish_events_with_status(
    state: &mut StreamState,
    incomplete_reason: Option<&str>,
) -> Result<Vec<Value>> {
    if state.completed {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    ensure_started(state, &mut events)?;

    let mut finished = Vec::new();
    if let Some(text) = state.text.take() {
        finished.push((text.output_index, FinishedOutput::Text(text)));
    }
    if let Some(reasoning) = state.reasoning.take() {
        finished.push((reasoning.output_index, FinishedOutput::Reasoning(reasoning)));
    }
    let tools = std::mem::take(&mut state.tools);
    finished.extend(
        tools
            .into_values()
            .map(|tool| (tool.output_index, FinishedOutput::Tool(tool))),
    );
    finished.sort_by_key(|(index, _)| *index);

    let mut output = Vec::with_capacity(finished.len());
    for (output_index, item) in finished {
        match item {
            FinishedOutput::Text(text) => {
                let part = json!({
                    "type": "output_text",
                    "text": text.text,
                    "annotations": [],
                    "logprobs": [],
                });
                let item = message_output(&text.id, part["text"].as_str().unwrap_or_default());
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::OutputTextDone {
                    item_id: text.id.clone(),
                    output_index,
                    content_index: 0,
                    text: part["text"].as_str().unwrap_or_default().to_owned(),
                    logprobs: Vec::new(),
                    sequence_number,
                })?);
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::ContentPartDone {
                    item_id: text.id,
                    output_index,
                    content_index: 0,
                    part,
                    sequence_number,
                })?);
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::OutputItemDone {
                    output_index,
                    item: item.clone(),
                    sequence_number,
                })?);
                output.push(item);
            }
            FinishedOutput::Reasoning(reasoning) => {
                let part = json!({"type": "summary_text", "text": reasoning.text});
                let mut item =
                    reasoning_output(&reasoning.id, part["text"].as_str().unwrap_or_default());
                if let Some(provenance) = reasoning.provenance {
                    item.as_object_mut()
                        .expect("reasoning_output always returns an object")
                        .insert(
                            "provider_metadata".into(),
                            json!({
                                "cmr_provider_owner_id": provenance.source_provider_id,
                                "source_provider_id": provenance.source_provider_id,
                                "format": provenance.format,
                                "payload": provenance.payload,
                            }),
                        );
                }
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::ReasoningDone {
                    item_id: reasoning.id.clone(),
                    output_index,
                    summary_index: 0,
                    text: part["text"].as_str().unwrap_or_default().to_owned(),
                    sequence_number,
                })?);
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::ReasoningSummaryPartDone {
                    item_id: reasoning.id,
                    output_index,
                    summary_index: 0,
                    part,
                    sequence_number,
                })?);
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::OutputItemDone {
                    output_index,
                    item: item.clone(),
                    sequence_number,
                })?);
                output.push(item);
            }
            FinishedOutput::Tool(tool) => {
                let item = if tool.custom {
                    custom_tool_call_output(&tool.id, &tool.call_id, &tool.name, &tool.arguments)
                } else {
                    function_call_output(&tool.id, &tool.call_id, &tool.name, &tool.arguments)
                };
                if !tool.custom {
                    let sequence_number = state.take_sequence_number();
                    events.push(event(ResponseEvent::FunctionCallArgumentsDone {
                        item_id: tool.id.clone(),
                        output_index,
                        arguments: tool.arguments.clone(),
                        sequence_number,
                    })?);
                }
                let sequence_number = state.take_sequence_number();
                events.push(event(ResponseEvent::OutputItemDone {
                    output_index,
                    item: item.clone(),
                    sequence_number,
                })?);
                output.push(item);
            }
        }
    }
    let response = response_object_with_status(
        &state.response_id,
        &state.model,
        Some(0),
        if incomplete_reason.is_some() {
            "incomplete"
        } else {
            "completed"
        },
        &output,
        state.usage.as_ref(),
        incomplete_reason,
    );
    let sequence_number = state.take_sequence_number();
    let terminal = if incomplete_reason.is_some() {
        ResponseEvent::Incomplete {
            response,
            sequence_number,
        }
    } else {
        ResponseEvent::Completed {
            response,
            sequence_number,
        }
    };
    events.push(event(terminal)?);
    state.completed = true;
    Ok(events)
}

pub(crate) fn chat_usage(value: Option<&Value>) -> Option<Usage> {
    let object = value?.as_object()?;
    let input_tokens = object
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = object
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = object
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    let reasoning_tokens = object
        .get("completion_tokens_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        reasoning_tokens,
    })
}

pub(crate) fn anthropic_usage(value: Option<&Value>, previous: Option<&Usage>) -> Option<Usage> {
    let object = value?.as_object()?;
    let input_tokens = object
        .get("input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| previous.map(|usage| usage.input_tokens))
        .unwrap_or(0);
    let output_tokens = object
        .get("output_tokens")
        .and_then(Value::as_u64)
        .or_else(|| previous.map(|usage| usage.output_tokens))
        .unwrap_or(0);
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        reasoning_tokens: None,
    })
}

pub(crate) fn gemini_usage(value: Option<&Value>) -> Option<Usage> {
    let object = value?.as_object()?;
    let input_tokens = object
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = object
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = object
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    let reasoning_tokens = object.get("thoughtsTokenCount").and_then(Value::as_u64);
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        reasoning_tokens,
    })
}

pub(crate) fn function_tools(request: &Map<String, Value>) -> Vec<&Map<String, Value>> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .collect()
}

/// Custom tools the Codex harness exposes with freeform string input (for
/// example `exec`). External chat providers have no custom-tool concept, so
/// these are advertised as single-`input` string functions; when the model
/// calls one, the existing function-call streaming conversion carries it back
/// to the client harness by name.
pub(crate) fn custom_string_tools(request: &Map<String, Value>) -> Vec<&Map<String, Value>> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter(|tool| tool.get("name").and_then(Value::as_str).is_some())
        .collect()
}

pub(crate) fn string_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_types(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .map(|event| {
                event["type"]
                    .as_str()
                    .expect("every normalized event has a type")
            })
            .collect()
    }

    fn assert_contiguous_sequence(events: &[Value]) {
        let actual = events
            .iter()
            .map(|event| {
                event["sequence_number"]
                    .as_u64()
                    .expect("every normalized event has a sequence number")
            })
            .collect::<Vec<_>>();
        let expected = (0..events.len() as u64).collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn strip_fake_exec_text_removes_tagged_blocks_only() {
        assert_eq!(strip_fake_exec_text("no tags"), "no tags");
        assert_eq!(
            strip_fake_exec_text("before <exec>dir</exec> after"),
            "before  after"
        );
        assert_eq!(
            strip_fake_exec_text("<exec>a</exec> and <exec>b</exec>"),
            " and "
        );
        assert_eq!(
            strip_fake_exec_text("dangling <exec>never closed"),
            "dangling "
        );
    }

    #[test]
    fn scrub_fake_exec_calls_drops_parts_that_become_empty() {
        let mut string = json!("only <exec>x</exec>");
        scrub_fake_exec_calls(&mut string);
        assert_eq!(string, json!("only "));

        let mut parts = json!([
            {"type": "text", "text": "keeps <exec>y</exec> prose"},
            {"type": "text", "text": "<exec>z</exec>"},
            {"type": "image_url", "image_url": {"url": "https://example.invalid/a.png"}}
        ]);
        scrub_fake_exec_calls(&mut parts);
        assert_eq!(
            parts,
            json!([
                {"type": "text", "text": "keeps  prose"},
                {"type": "image_url", "image_url": {"url": "https://example.invalid/a.png"}}
            ])
        );

        let mut only_exec = json!([{"type": "text", "text": "<exec>gone</exec>"}]);
        scrub_fake_exec_calls(&mut only_exec);
        assert_eq!(only_exec, json!(""));
    }

    #[test]
    fn custom_tool_stream_emits_custom_tool_call_items() {
        let mut state = StreamState::new("resp_custom", "model-x");
        let mut names = std::collections::BTreeSet::new();
        names.insert("exec".to_owned());
        state.set_custom_tools(names);
        let mut events = Vec::new();
        ensure_tool(
            &mut state,
            0,
            Some("call_exec_1"),
            Some("exec"),
            &mut events,
        )
        .expect("custom tool opens");
        push_tool_delta(
            &mut state,
            0,
            "{\"input\":\"const r = await tools.shell_command({command:\\\"dir\\\"})\"}",
            &mut events,
        )
        .expect("custom tool arguments buffer");
        events.extend(finish_events(&mut state).expect("stream finishes"));

        let types = event_types(&events);
        assert!(
            !types.contains(&"response.function_call_arguments.delta")
                && !types.contains(&"response.function_call_arguments.done"),
            "custom tool calls must not emit function-argument events: {types:?}"
        );
        assert_contiguous_sequence(&events);
        let added = events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .expect("added event");
        assert_eq!(added["item"]["type"], "custom_tool_call");
        let done = events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .expect("done event");
        assert_eq!(done["item"]["type"], "custom_tool_call");
        assert_eq!(done["item"]["name"], "exec");
        assert_eq!(
            done["item"]["input"],
            "const r = await tools.shell_command({command:\"dir\"})"
        );
        let completed = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("completed event");
        assert_eq!(
            completed["response"]["output"][0]["type"],
            "custom_tool_call"
        );
    }

    #[test]
    fn text_stream_has_complete_responses_lifecycle() {
        let mut state = StreamState::new("resp_text", "model-a");
        let mut events = Vec::new();
        push_text_delta(&mut state, "hello", &mut events).expect("text delta converts");
        events.extend(finish_events(&mut state).expect("stream finishes"));

        assert_eq!(
            event_types(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_contiguous_sequence(&events);
        assert!(events[0].get("response_id").is_none());
        assert_eq!(events[0]["response"]["status"], "in_progress");
        assert_eq!(events[8]["response"]["status"], "completed");
        assert_eq!(events[8]["response"]["output"][0]["type"], "message");
        assert_eq!(
            events[8]["response"]["output"][0]["content"][0]["text"],
            "hello"
        );
    }

    #[test]
    fn tool_stream_finishes_arguments_and_item_before_response() {
        let mut state = StreamState::new("resp_tool", "model-b");
        let mut events = Vec::new();
        ensure_tool(&mut state, 0, Some("call_42"), Some("lookup"), &mut events)
            .expect("tool opens");
        push_tool_delta(&mut state, 0, "{\"q\":\"rust\"}", &mut events)
            .expect("tool arguments convert");
        events.extend(finish_events(&mut state).expect("stream finishes"));

        assert_eq!(
            event_types(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_contiguous_sequence(&events);
        assert_eq!(events[4]["arguments"], "{\"q\":\"rust\"}");
        assert_eq!(events[6]["response"]["output"][0]["type"], "function_call");
        assert_eq!(events[6]["response"]["output"][0]["call_id"], "call_42");
    }

    #[test]
    fn reasoning_stream_never_becomes_an_assistant_message() {
        let mut state = StreamState::new("resp_reasoning", "model-c");
        let mut events = Vec::new();
        push_reasoning_delta(&mut state, "internal summary", &mut events)
            .expect("reasoning delta converts");
        events.extend(finish_events(&mut state).expect("stream finishes"));

        assert_eq!(
            event_types(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_contiguous_sequence(&events);
        assert_eq!(events[8]["response"]["output"][0]["type"], "reasoning");
        assert!(
            !events[8]["response"]
                .to_string()
                .contains("\"type\":\"message\"")
        );
    }
}
