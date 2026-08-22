//! Provider protocol adapters for Codex Model Router.
//!
//! The crate deliberately performs no network or credential access. It converts
//! Responses-shaped JSON requests and offline upstream response/stream payloads,
//! leaving transport and secret handling to the router.

mod anthropic;
mod gemini;
mod openai_chat;
mod presets;
mod responses;
mod support;
mod types;

pub use anthropic::AnthropicMessagesAdapter;
pub use gemini::GeminiGenerateContentAdapter;
pub use openai_chat::OpenAiChatCompletionsAdapter;
pub use presets::{built_in_presets, custom_compatible_preset, preset_by_id};
pub use responses::ResponsesPassthroughAdapter;
pub use types::{
    AdapterError, AuthStyle, ProtocolFamily, ProviderAdapter, ProviderCapabilities, ProviderPreset,
    ResponseEvent, Result, StreamState, Usage, custom_tool_names,
};

/// Builds the appropriate offline adapter for a provider preset.
#[must_use]
pub fn adapter_for_preset(preset: ProviderPreset) -> Box<dyn ProviderAdapter> {
    match preset.protocol {
        ProtocolFamily::Responses => Box::new(ResponsesPassthroughAdapter::new(preset)),
        ProtocolFamily::OpenAiChatCompletions => {
            Box::new(OpenAiChatCompletionsAdapter::new(preset))
        }
        ProtocolFamily::AnthropicMessages => Box::new(AnthropicMessagesAdapter::new(preset)),
        ProtocolFamily::GeminiGenerateContent => {
            Box::new(GeminiGenerateContentAdapter::new(preset))
        }
    }
}
