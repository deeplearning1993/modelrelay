use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

/// Result type returned by provider conversion code.
pub type Result<T> = std::result::Result<T, AdapterError>;

/// Errors produced while validating presets or converting provider payloads.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The incoming Responses request is invalid or cannot be represented by the target protocol.
    #[error("invalid Responses request: {0}")]
    InvalidRequest(String),
    /// The upstream provider returned an invalid payload.
    #[error("malformed upstream payload: {0}")]
    MalformedUpstream(String),
    /// The requested operation is intentionally unsupported by this adapter.
    #[error("unsupported provider feature: {0}")]
    Unsupported(String),
    /// A custom provider preset is invalid.
    #[error("invalid provider preset: {0}")]
    InvalidPreset(String),
    /// JSON serialization or deserialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// The upstream wire protocol spoken by a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFamily {
    /// `OpenAI` Responses API. Requests and stream events are passed through.
    Responses,
    /// OpenAI-compatible `/chat/completions` API.
    OpenAiChatCompletions,
    /// Anthropic `/messages` API.
    AnthropicMessages,
    /// Google Gemini `generateContent` API.
    GeminiGenerateContent,
}

/// Describes how the transport layer should attach a referenced secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `x-api-key: <secret>`.
    XApiKey,
    /// `x-goog-api-key: <secret>`.
    GoogleApiKey,
    /// No credential, normally used by a loopback Ollama service.
    None,
}

/// Capabilities used by catalog filtering and preflight checks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderCapabilities {
    /// Server-sent event streaming is supported.
    pub streaming: bool,
    /// Native Responses WebSocket transport is supported.
    pub websocket: bool,
    /// Function tools are supported.
    pub tools: bool,
    /// More than one tool call may be emitted in a turn.
    pub parallel_tool_calls: bool,
    /// The provider can return reasoning separately from visible assistant text.
    pub reasoning: bool,
    /// Image inputs are supported.
    pub vision: bool,
    /// Audio inputs are supported.
    pub audio: bool,
    /// Structured JSON schema output is supported.
    pub json_schema: bool,
    /// The provider implements a native compaction operation.
    pub native_compaction: bool,
    /// Advertised context window, when known.
    pub context_window: Option<u64>,
    /// Advertised maximum generated tokens, when known.
    pub max_output_tokens: Option<u64>,
}

impl ProviderCapabilities {
    /// A conservative OpenAI-compatible capability set.
    #[must_use]
    pub const fn compatible() -> Self {
        Self {
            streaming: true,
            websocket: false,
            tools: true,
            parallel_tool_calls: true,
            reasoning: false,
            vision: false,
            audio: false,
            json_schema: false,
            native_compaction: false,
            context_window: None,
            max_output_tokens: None,
        }
    }
}

/// A built-in or user-defined provider endpoint without any credential value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderPreset {
    /// Stable configuration identifier.
    pub id: String,
    /// Human-readable provider name.
    pub display_name: String,
    /// Upstream protocol family.
    pub protocol: ProtocolFamily,
    /// Base endpoint with no request path and no embedded credential.
    pub base_url: String,
    /// How the router supplies a credential reference.
    pub auth: AuthStyle,
    /// Suggested upstream model for provider setup.
    pub default_model: Option<String>,
    /// Capabilities exposed to catalog policy.
    pub capabilities: ProviderCapabilities,
    /// Safe, non-secret top-level request values applied after conversion.
    #[serde(default)]
    pub request_overrides: Value,
    /// Safe, non-secret headers required by the protocol.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl ProviderPreset {
    /// Creates a user-defined OpenAI-compatible Chat Completions preset.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier or endpoint is invalid or unsafe.
    pub fn custom_compatible(
        id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: impl Into<String>,
        allow_plain_http: bool,
    ) -> Result<Self> {
        let id = id.into();
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(AdapterError::InvalidPreset(
                "id must contain only lowercase ASCII letters, digits, and '-'".into(),
            ));
        }

        let base_url = base_url.into();
        let parsed = Url::parse(&base_url)
            .map_err(|error| AdapterError::InvalidPreset(format!("invalid base URL: {error}")))?;
        let loopback = parsed.host().is_some_and(|host| match host {
            url::Host::Domain(domain) => domain == "localhost",
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        });
        if parsed.scheme() != "https"
            && !(parsed.scheme() == "http" && (loopback || allow_plain_http))
        {
            return Err(AdapterError::InvalidPreset(
                "base URL must use HTTPS, except for loopback HTTP endpoints or an explicit plain-HTTP opt-in"
                    .into(),
            ));
        }
        if parsed.username() != ""
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AdapterError::InvalidPreset(
                "base URL cannot contain credentials, a query string, or a fragment".into(),
            ));
        }

        Ok(Self {
            id,
            display_name: display_name.into(),
            protocol: ProtocolFamily::OpenAiChatCompletions,
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth: AuthStyle::Bearer,
            default_model: None,
            capabilities: ProviderCapabilities::compatible(),
            request_overrides: json!({}),
            headers: BTreeMap::new(),
        })
    }
}

/// Token usage normalized to Responses field names.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
    /// Total tokens, including cached tokens if reported by the provider.
    pub total_tokens: u64,
    /// Reasoning tokens included in output usage, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// A normalized Responses streaming event emitted by converted protocols.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ResponseEvent {
    /// The synthetic response has started.
    #[serde(rename = "response.created")]
    Created {
        /// Complete in-progress Responses object.
        response: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// The synthetic response entered the in-progress state.
    #[serde(rename = "response.in_progress")]
    InProgress {
        /// Complete in-progress Responses object.
        response: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A response output item has been opened.
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        /// Position in the response output array.
        output_index: usize,
        /// In-progress Responses item.
        item: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A content part has been opened on an assistant message.
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        /// Output item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the message content array.
        content_index: usize,
        /// Empty in-progress content part.
        part: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// Visible assistant text delta.
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        /// Output item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the message content array.
        content_index: usize,
        /// Newly emitted visible text.
        delta: String,
        /// Token log probabilities, empty when unavailable upstream.
        logprobs: Vec<Value>,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// Visible assistant text has completed.
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        /// Output item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the message content array.
        content_index: usize,
        /// Full visible text.
        text: String,
        /// Token log probabilities, empty when unavailable upstream.
        logprobs: Vec<Value>,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A content part on an assistant message has completed.
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        /// Output item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the message content array.
        content_index: usize,
        /// Completed content part.
        part: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A reasoning summary part has been opened.
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        /// Reasoning item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the reasoning summary array.
        summary_index: usize,
        /// Empty in-progress reasoning summary part.
        part: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// Reasoning delta, kept separate from visible assistant text.
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningDelta {
        /// Reasoning item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the reasoning summary array.
        summary_index: usize,
        /// Newly emitted reasoning text.
        delta: String,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A reasoning summary text has completed.
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningDone {
        /// Reasoning item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the reasoning summary array.
        summary_index: usize,
        /// Full reasoning summary text.
        text: String,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A reasoning summary part has completed.
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        /// Reasoning item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Position in the reasoning summary array.
        summary_index: usize,
        /// Completed reasoning summary part.
        part: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// Incremental JSON arguments for a function call.
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        /// Function-call item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Newly emitted JSON fragment.
        delta: String,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// The complete JSON arguments for a function call.
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        /// Function-call item identifier.
        item_id: String,
        /// Position in the response output array.
        output_index: usize,
        /// Complete JSON argument string.
        arguments: String,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// An output item has finished.
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        /// Position in the response output array.
        output_index: usize,
        /// Completed Responses item.
        item: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// The response has completed.
    #[serde(rename = "response.completed")]
    Completed {
        /// Complete terminal Responses object, including output and usage.
        response: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// The provider stopped before satisfying the complete response request.
    #[serde(rename = "response.incomplete")]
    Incomplete {
        /// Complete terminal Responses object, including partial output and reason.
        response: Value,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
    /// A provider stream error.
    #[serde(rename = "error")]
    Error {
        /// Stable error category.
        code: String,
        /// Safe diagnostic text.
        message: String,
        /// Related request field, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        param: Option<String>,
        /// Monotonically increasing event position.
        sequence_number: u64,
    },
}

impl ResponseEvent {
    /// Converts the typed event to a JSON object suitable for SSE or WebSocket encoding.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization fails.
    pub fn into_json(self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TextState {
    pub(crate) id: String,
    pub(crate) output_index: usize,
    pub(crate) text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ReasoningProvenance {
    pub(crate) source_provider_id: String,
    pub(crate) format: String,
    pub(crate) payload: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct ReasoningState {
    pub(crate) id: String,
    pub(crate) output_index: usize,
    pub(crate) text: String,
    pub(crate) provenance: Option<ReasoningProvenance>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolState {
    pub(crate) id: String,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) output_index: usize,
    pub(crate) arguments: String,
    pub(crate) custom: bool,
}

/// Mutable, transport-independent state for converting an upstream stream offline.
#[derive(Clone, Debug)]
pub struct StreamState {
    pub(crate) response_id: String,
    pub(crate) model: String,
    pub(crate) started: bool,
    pub(crate) completed: bool,
    pub(crate) next_output_index: usize,
    pub(crate) next_sequence_number: u64,
    pub(crate) text: Option<TextState>,
    pub(crate) reasoning: Option<ReasoningState>,
    pub(crate) tools: BTreeMap<usize, ToolState>,
    pub(crate) usage: Option<Usage>,
    pub(crate) custom_tools: BTreeSet<String>,
}

impl StreamState {
    /// Starts an empty conversion state for one Responses request.
    #[must_use]
    pub fn new(response_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            response_id: response_id.into(),
            model: model.into(),
            started: false,
            completed: false,
            next_output_index: 0,
            next_sequence_number: 0,
            text: None,
            reasoning: None,
            tools: BTreeMap::new(),
            usage: None,
            custom_tools: BTreeSet::new(),
        }
    }

    /// Marks tool names the client registered as custom string tools. Calls to
    /// those names are converted back into `custom_tool_call` output items,
    /// the only shape the client harness can dispatch for them.
    pub fn set_custom_tools(&mut self, names: BTreeSet<String>) {
        self.custom_tools = names;
    }

    /// Returns whether a terminal event has already been emitted.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.completed
    }

    /// Returns the upstream model associated with this stream.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn take_sequence_number(&mut self) -> u64 {
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number += 1;
        sequence_number
    }
}

/// Collects the names of tools the client declared as `type:"custom"`, from
/// the request's top-level `tools` array and from any `additional_tools`
/// input items. The router stamps them onto [`StreamState`] so provider
/// function calls convert back into dispatchable `custom_tool_call` items.
#[must_use]
pub fn custom_tool_names(request: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut absorb = |tools: Option<&Vec<Value>>| {
        if let Some(tools) = tools {
            for tool in tools {
                if tool.get("type").and_then(Value::as_str) == Some("custom")
                    && let Some(name) = tool.get("name").and_then(Value::as_str)
                {
                    names.insert(name.to_owned());
                }
            }
        }
    };
    absorb(request.get("tools").and_then(Value::as_array));
    if let Some(items) = request.get("input").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                absorb(item.get("tools").and_then(Value::as_array));
            }
        }
    }
    names
}

/// Offline request, response, and stream conversion implemented by each protocol family.
pub trait ProviderAdapter: Send + Sync {
    /// Returns the immutable provider preset.
    fn preset(&self) -> &ProviderPreset;

    /// Returns the request path relative to `ProviderPreset::base_url`.
    fn request_path(&self, model: &str, stream: bool) -> String;

    /// Converts a Responses-shaped request into the upstream protocol body.
    ///
    /// # Errors
    ///
    /// Returns an error when required request fields are missing or unsupported.
    fn encode_request(&self, request: &Value) -> Result<Value>;

    /// Converts a non-streaming upstream response into a Responses-shaped object.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream response is malformed or incomplete.
    fn decode_response(&self, response: &Value, response_id: &str) -> Result<Value>;

    /// Converts one already-decoded upstream stream payload into zero or more Responses events.
    ///
    /// # Errors
    ///
    /// Returns an error when a stream payload violates its provider protocol.
    fn decode_stream_chunk(&self, state: &mut StreamState, chunk: &Value) -> Result<Vec<Value>>;

    /// Finalizes a stream when the transport receives its protocol-specific end marker.
    ///
    /// # Errors
    ///
    /// Returns an error when buffered state cannot form a valid terminal response.
    fn finish_stream(&self, state: &mut StreamState) -> Result<Vec<Value>>;
}
