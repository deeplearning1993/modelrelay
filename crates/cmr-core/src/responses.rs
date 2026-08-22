//! Provider-neutral representations of the `OpenAI` Responses wire protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A canonical request accepted by the router's Responses endpoint.
// Its fields are verbatim Responses wire keys, so the protocol is their documentation.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseRequest {
    pub model: String,
    pub input: ResponseInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<ResponseInstructions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Fields from a newer client version that this crate does not yet model.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Instructions accepted either as developer text or as a list of input items.
// Its variants are the two untagged wire shapes accepted by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInstructions {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Either the shorthand text input or a normalized list of input items.
// Its variants are the two untagged wire shapes accepted by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Input and output items share the same normalized representation.
pub type ResponseInputItem = ResponseItem;

/// Input and output items share the same normalized representation.
pub type ResponseOutputItem = ResponseItem;

/// A normalized item in a Responses conversation.
// Its variants are the exact public Responses item discriminators.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message(MessageItem),
    FunctionCall(FunctionCallItem),
    FunctionCallOutput(FunctionCallOutputItem),
    Reasoning(ReasoningItem),
    Compaction(CompactionItem),
}

/// A message item with typed content parts.
// Its fields are verbatim Responses message wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: MessageRole,
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Message content in either wire shorthand or normalized-part form.
// Its variants are the two untagged Responses message-content wire shapes.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// Roles accepted by the normalized message representation.
// Its variants are the exact role strings accepted on the wire.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Developer,
    User,
    Assistant,
}

/// A typed message content part.
// Its variants and members mirror the Responses content-part wire schema.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText {
        text: String,
    },
    OutputText {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        logprobs: Vec<Value>,
    },
    InputImage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    InputFile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    Refusal {
        refusal: String,
    },
}

/// A function call emitted by a model.
// Its fields are verbatim Responses function-call wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub name: String,
    /// Complete JSON arguments encoded as a string, as defined by Responses.
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The result supplied for a previous function call.
// Its fields are verbatim Responses function-call-output wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub output: ToolOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A tool result, preserving both traditional text and structured output.
// Its variants are the two untagged tool-output wire shapes.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolOutput {
    Text(String),
    Json(Value),
}

/// Provider reasoning state. `encrypted_content` must only return to its owner.
// Its fields are verbatim Responses reasoning-item wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<ReasoningSummaryPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// One textual reasoning-summary part.
// Its fields are verbatim Responses reasoning-summary-part wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSummaryPart {
    #[serde(rename = "type")]
    pub kind: ReasoningSummaryPartType,
    pub text: String,
}

/// The wire discriminator for a reasoning summary part.
// Its variant is the exact discriminator string defined by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryPartType {
    SummaryText,
}

/// The only output item allowed in a compact response.
// Its fields are verbatim Responses compaction-item wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub encrypted_content: String,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Lifecycle state shared by response and output items.
// Its variants are the exact lifecycle strings defined by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

/// A function tool advertised to a model.
// Its variant members mirror the Responses function-tool wire schema.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    Function {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        parameters: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Tool selection in either shorthand or named-tool form.
// Its variants are the two untagged tool-choice wire shapes.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Named(NamedToolChoice),
}

/// Shorthand tool-selection modes.
// Its variants are the exact shorthand strings defined by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    None,
    Auto,
    Required,
}

/// A request to invoke one named tool.
// Its variant members mirror the named-tool-choice wire schema.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NamedToolChoice {
    Function { name: String },
}

/// A complete response object.
// Its fields are verbatim Responses response-object wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseObject {
    pub id: String,
    #[serde(default = "response_object_name")]
    pub object: String,
    pub created_at: u64,
    pub status: ResponseStatus,
    pub model: String,
    #[serde(default)]
    pub output: Vec<ResponseOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn response_object_name() -> String {
    "response".to_owned()
}

/// The state of a complete response object.
// Its variants are the exact response-status strings defined by Responses.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Queued,
    InProgress,
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

/// Provider-neutral token accounting.
// Its fields retain the standard Responses usage wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A normalized API error.
// Its fields retain the standard Responses error wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for response lifecycle events.
// Its fields are verbatim lifecycle-event payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseLifecycleEvent {
    pub response: ResponseObject,
    pub sequence_number: u64,
}

/// Payload for item-added and item-done events.
// Its fields are verbatim output-item-event payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputItemEvent {
    pub output_index: usize,
    pub item: ResponseOutputItem,
    pub sequence_number: u64,
}

/// Payload for content-part-added and content-part-done events.
// Its fields are verbatim content-part-event payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPartEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub part: ContentPart,
    pub sequence_number: u64,
}

/// Payload for an output-text delta.
// Its fields are verbatim output-text-delta payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDeltaEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub delta: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logprobs: Vec<Value>,
    pub sequence_number: u64,
}

/// Payload for completed output text.
// Its fields are verbatim output-text-done payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDoneEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub text: String,
    pub sequence_number: u64,
}

/// Payload for a function-argument delta.
// Its fields are verbatim function-arguments-delta payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionArgumentsDeltaEvent {
    pub item_id: String,
    pub output_index: usize,
    pub delta: String,
    pub sequence_number: u64,
}

/// Payload for completed function arguments.
// Its fields are verbatim function-arguments-done payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionArgumentsDoneEvent {
    pub item_id: String,
    pub output_index: usize,
    pub name: String,
    pub arguments: String,
    pub sequence_number: u64,
}

/// Payload for a refusal delta.
// Its fields are verbatim refusal-delta payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalDeltaEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub delta: String,
    pub sequence_number: u64,
}

/// Payload for a completed refusal.
// Its fields are verbatim refusal-done payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalDoneEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub refusal: String,
    pub sequence_number: u64,
}

/// Payload for a reasoning-summary part being added or completed.
// Its fields are verbatim reasoning-summary-part-event payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSummaryPartEvent {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub part: ReasoningSummaryPart,
    pub sequence_number: u64,
}

/// Payload for a reasoning-summary text delta.
// Its fields are verbatim reasoning-summary-text-delta payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSummaryTextDeltaEvent {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub delta: String,
    pub sequence_number: u64,
}

/// Payload for completed reasoning-summary text.
// Its fields are verbatim reasoning-summary-text-done payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSummaryTextDoneEvent {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub text: String,
    pub sequence_number: u64,
}

/// Payload for an exposed reasoning-text delta.
// Its fields are verbatim reasoning-text-delta payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningTextDeltaEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub delta: String,
    pub sequence_number: u64,
}

/// Payload for completed exposed reasoning text.
// Its fields are verbatim reasoning-text-done payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningTextDoneEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub text: String,
    pub sequence_number: u64,
}

/// Payload for an output-text annotation being added.
// Its fields are verbatim annotation-added payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputTextAnnotationAddedEvent {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub annotation_index: usize,
    pub annotation: Value,
    pub sequence_number: u64,
}

/// Payload for a stream-level error.
// Its fields are verbatim Responses stream-error payload wire keys.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    pub sequence_number: u64,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Canonical Responses SSE/WebSocket events.
// Its variants are the exact public Responses stream event names.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    #[serde(rename = "response.created")]
    Created(ResponseLifecycleEvent),
    #[serde(rename = "response.queued")]
    Queued(ResponseLifecycleEvent),
    #[serde(rename = "response.in_progress")]
    InProgress(ResponseLifecycleEvent),
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded(OutputItemEvent),
    #[serde(rename = "response.output_item.done")]
    OutputItemDone(OutputItemEvent),
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded(ContentPartEvent),
    #[serde(rename = "response.content_part.done")]
    ContentPartDone(ContentPartEvent),
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta(TextDeltaEvent),
    #[serde(rename = "response.output_text.done")]
    OutputTextDone(TextDoneEvent),
    #[serde(rename = "response.output_text.annotation.added")]
    OutputTextAnnotationAdded(OutputTextAnnotationAddedEvent),
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta(RefusalDeltaEvent),
    #[serde(rename = "response.refusal.done")]
    RefusalDone(RefusalDoneEvent),
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta(FunctionArgumentsDeltaEvent),
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone(FunctionArgumentsDoneEvent),
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded(ReasoningSummaryPartEvent),
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone(ReasoningSummaryPartEvent),
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaEvent),
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone(ReasoningSummaryTextDoneEvent),
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta(ReasoningTextDeltaEvent),
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone(ReasoningTextDoneEvent),
    #[serde(rename = "response.completed")]
    Completed(ResponseLifecycleEvent),
    #[serde(rename = "response.incomplete")]
    Incomplete(ResponseLifecycleEvent),
    #[serde(rename = "response.failed")]
    Failed(ResponseLifecycleEvent),
    #[serde(rename = "error")]
    Error(ErrorEvent),
}

impl ResponseStreamEvent {
    /// Return the exact wire event name used in SSE `event:` lines and JSON.
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::Created(_) => "response.created",
            Self::Queued(_) => "response.queued",
            Self::InProgress(_) => "response.in_progress",
            Self::OutputItemAdded(_) => "response.output_item.added",
            Self::OutputItemDone(_) => "response.output_item.done",
            Self::ContentPartAdded(_) => "response.content_part.added",
            Self::ContentPartDone(_) => "response.content_part.done",
            Self::OutputTextDelta(_) => "response.output_text.delta",
            Self::OutputTextDone(_) => "response.output_text.done",
            Self::OutputTextAnnotationAdded(_) => "response.output_text.annotation.added",
            Self::RefusalDelta(_) => "response.refusal.delta",
            Self::RefusalDone(_) => "response.refusal.done",
            Self::FunctionCallArgumentsDelta(_) => "response.function_call_arguments.delta",
            Self::FunctionCallArgumentsDone(_) => "response.function_call_arguments.done",
            Self::ReasoningSummaryPartAdded(_) => "response.reasoning_summary_part.added",
            Self::ReasoningSummaryPartDone(_) => "response.reasoning_summary_part.done",
            Self::ReasoningSummaryTextDelta(_) => "response.reasoning_summary_text.delta",
            Self::ReasoningSummaryTextDone(_) => "response.reasoning_summary_text.done",
            Self::ReasoningTextDelta(_) => "response.reasoning_text.delta",
            Self::ReasoningTextDone(_) => "response.reasoning_text.done",
            Self::Completed(_) => "response.completed",
            Self::Incomplete(_) => "response.incomplete",
            Self::Failed(_) => "response.failed",
            Self::Error(_) => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_map() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    fn response() -> ResponseObject {
        ResponseObject {
            id: "resp_1".into(),
            object: "response".into(),
            created_at: 42,
            status: ResponseStatus::InProgress,
            model: "model-a".into(),
            output: Vec::new(),
            usage: None,
            error: None,
            metadata: BTreeMap::new(),
            extra: empty_map(),
        }
    }

    #[test]
    fn response_request_round_trips_unknown_fields() {
        let raw = json!({
            "model": "third-party/model",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": {"temperature": 21}
            }],
            "stream": true,
            "future_client_field": {"enabled": true}
        });

        let parsed: ResponseRequest = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            parsed.extra["future_client_field"],
            json!({"enabled": true})
        );
        assert_eq!(serde_json::to_value(parsed).unwrap(), raw);
    }

    #[test]
    fn request_accepts_item_instructions_and_message_content_shorthand() {
        let raw = json!({
            "model": "model-a",
            "input": "hello",
            "instructions": [{
                "type": "message",
                "role": "developer",
                "content": "Use concise answers"
            }],
            "stream": false
        });

        let parsed: ResponseRequest = serde_json::from_value(raw.clone()).unwrap();

        assert!(matches!(
            parsed.instructions,
            Some(ResponseInstructions::Items(ref items)) if items.len() == 1
        ));
        assert_eq!(serde_json::to_value(parsed).unwrap(), raw);
    }

    #[test]
    fn image_and_file_inputs_preserve_supported_reference_forms() {
        let raw = json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_image", "file_id": "file-image", "detail": "high"},
                {
                    "type": "input_file",
                    "file_data": "data:application/pdf;base64,AA==",
                    "filename": "brief.pdf"
                }
            ]
        });

        let parsed: ResponseItem = serde_json::from_value(raw.clone()).unwrap();

        assert_eq!(serde_json::to_value(parsed).unwrap(), raw);
    }

    #[test]
    fn stream_event_uses_exact_responses_event_name() {
        let event = ResponseStreamEvent::Created(ResponseLifecycleEvent {
            response: response(),
            sequence_number: 0,
        });
        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["type"], "response.created");
        assert_eq!(value["response"]["id"], "resp_1");
        assert_eq!(event.event_type(), "response.created");
        assert_eq!(
            serde_json::from_value::<ResponseStreamEvent>(value).unwrap(),
            event
        );
    }

    #[test]
    fn function_arguments_done_contains_name_and_complete_arguments() {
        let event = ResponseStreamEvent::FunctionCallArgumentsDone(FunctionArgumentsDoneEvent {
            item_id: "fc_1".into(),
            output_index: 0,
            name: "lookup".into(),
            arguments: r#"{"city":"Shanghai"}"#.into(),
            sequence_number: 7,
        });

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": "fc_1",
                "output_index": 0,
                "name": "lookup",
                "arguments": "{\"city\":\"Shanghai\"}",
                "sequence_number": 7
            })
        );
    }

    #[test]
    fn reasoning_summary_delta_and_done_use_distinct_payload_fields() {
        let delta =
            ResponseStreamEvent::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaEvent {
                item_id: "rs_1".into(),
                output_index: 0,
                summary_index: 0,
                delta: "partial".into(),
                sequence_number: 3,
            });
        let done = ResponseStreamEvent::ReasoningSummaryTextDone(ReasoningSummaryTextDoneEvent {
            item_id: "rs_1".into(),
            output_index: 0,
            summary_index: 0,
            text: "complete".into(),
            sequence_number: 4,
        });
        let delta_json = serde_json::to_value(delta).unwrap();
        let done_json = serde_json::to_value(done).unwrap();

        assert_eq!(delta_json["delta"], "partial");
        assert!(delta_json.get("text").is_none());
        assert_eq!(done_json["text"], "complete");
        assert!(done_json.get("delta").is_none());
    }

    #[test]
    fn stream_error_fields_are_at_event_top_level() {
        let event = ResponseStreamEvent::Error(ErrorEvent {
            code: "server_error".into(),
            message: "upstream failed".into(),
            param: Some("input".into()),
            sequence_number: 9,
            extra: empty_map(),
        });
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "server_error");
        assert_eq!(value["param"], "input");
        assert!(value.get("error").is_none());
    }

    #[test]
    fn tool_choice_supports_shorthand_and_named_function() {
        let auto = serde_json::to_value(ToolChoice::Mode(ToolChoiceMode::Auto)).unwrap();
        let named = serde_json::to_value(ToolChoice::Named(NamedToolChoice::Function {
            name: "lookup".into(),
        }))
        .unwrap();

        assert_eq!(auto, json!("auto"));
        assert_eq!(named, json!({"type": "function", "name": "lookup"}));
    }

    #[test]
    fn reasoning_preserves_provider_metadata_without_core_coupling() {
        let raw = json!({
            "type": "reasoning",
            "id": "reasoning_1",
            "summary": [],
            "provider_metadata": {
                "source_provider_id": "zhipu",
                "format": "glm_reasoning_v1",
                "payload": {"opaque": [1, 2, 3]}
            }
        });

        let parsed: ResponseItem = serde_json::from_value(raw.clone()).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();

        assert_eq!(serialized["provider_metadata"], raw["provider_metadata"]);
        assert_eq!(
            serialized,
            json!({
                "type": "reasoning",
                "id": "reasoning_1",
                "provider_metadata": {
                    "source_provider_id": "zhipu",
                    "format": "glm_reasoning_v1",
                    "payload": {"opaque": [1, 2, 3]}
                }
            })
        );
    }

    #[test]
    fn compaction_item_has_standard_discriminator() {
        let item = ResponseItem::Compaction(CompactionItem {
            id: Some("cmp_1".into()),
            encrypted_content: "opaque".into(),
            extra: empty_map(),
        });

        assert_eq!(
            serde_json::to_value(item).unwrap(),
            json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "opaque"})
        );
    }
}
