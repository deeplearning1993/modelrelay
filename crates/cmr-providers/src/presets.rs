use std::collections::BTreeMap;

use serde_json::json;

use crate::support::string_map;
use crate::types::{AuthStyle, ProtocolFamily, ProviderCapabilities, ProviderPreset, Result};

fn capabilities(protocol: ProtocolFamily) -> ProviderCapabilities {
    match protocol {
        ProtocolFamily::Responses => ProviderCapabilities {
            streaming: true,
            websocket: true,
            tools: true,
            parallel_tool_calls: true,
            reasoning: true,
            vision: true,
            audio: true,
            json_schema: true,
            native_compaction: true,
            context_window: None,
            max_output_tokens: None,
        },
        ProtocolFamily::AnthropicMessages => ProviderCapabilities {
            streaming: true,
            websocket: false,
            tools: true,
            parallel_tool_calls: true,
            reasoning: true,
            vision: true,
            audio: false,
            json_schema: false,
            native_compaction: false,
            context_window: None,
            max_output_tokens: None,
        },
        ProtocolFamily::GeminiGenerateContent => ProviderCapabilities {
            streaming: true,
            websocket: false,
            tools: true,
            parallel_tool_calls: true,
            reasoning: true,
            vision: true,
            audio: true,
            json_schema: true,
            native_compaction: false,
            context_window: None,
            max_output_tokens: None,
        },
        ProtocolFamily::OpenAiChatCompletions => ProviderCapabilities::compatible(),
    }
}

fn preset(
    id: &str,
    display_name: &str,
    protocol: ProtocolFamily,
    base_url: &str,
    auth: AuthStyle,
    default_model: Option<&str>,
) -> ProviderPreset {
    ProviderPreset {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        protocol,
        base_url: base_url.to_owned(),
        auth,
        default_model: default_model.map(str::to_owned),
        capabilities: capabilities(protocol),
        request_overrides: json!({}),
        headers: BTreeMap::new(),
    }
}

/// Returns all built-in provider presets. Presets never contain credentials and
/// remain disabled until the application creates a secret reference.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn built_in_presets() -> Vec<ProviderPreset> {
    let openai = preset(
        "openai",
        "OpenAI",
        ProtocolFamily::Responses,
        "https://api.openai.com/v1",
        AuthStyle::Bearer,
        None,
    );

    let mut anthropic = preset(
        "anthropic",
        "Anthropic",
        ProtocolFamily::AnthropicMessages,
        "https://api.anthropic.com/v1",
        AuthStyle::XApiKey,
        None,
    );
    anthropic.headers = string_map(&[("anthropic-version", "2023-06-01")]);

    let gemini = preset(
        "gemini",
        "Google Gemini",
        ProtocolFamily::GeminiGenerateContent,
        "https://generativelanguage.googleapis.com/v1beta",
        AuthStyle::GoogleApiKey,
        None,
    );

    let mut zhipu = preset(
        "zhipu",
        "Zhipu AI Coding Plan",
        ProtocolFamily::OpenAiChatCompletions,
        "https://open.bigmodel.cn/api/coding/paas/v4",
        AuthStyle::Bearer,
        Some("glm-5.2"),
    );
    zhipu.capabilities.reasoning = true;
    zhipu.capabilities.context_window = Some(1_000_000);
    zhipu.capabilities.max_output_tokens = Some(131_072);
    zhipu.request_overrides = json!({"thinking": {"type": "enabled", "clear_thinking": false}});

    let mut deepseek = preset(
        "deepseek",
        "DeepSeek",
        ProtocolFamily::OpenAiChatCompletions,
        "https://api.deepseek.com/v1",
        AuthStyle::Bearer,
        None,
    );
    deepseek.capabilities.reasoning = true;

    let qwen = preset(
        "qwen",
        "Alibaba Qwen",
        ProtocolFamily::OpenAiChatCompletions,
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        AuthStyle::Bearer,
        None,
    );
    let kimi = preset(
        "kimi",
        "Moonshot Kimi",
        ProtocolFamily::OpenAiChatCompletions,
        "https://api.moonshot.cn/v1",
        AuthStyle::Bearer,
        None,
    );
    let doubao = preset(
        "doubao",
        "ByteDance Doubao",
        ProtocolFamily::OpenAiChatCompletions,
        "https://ark.cn-beijing.volces.com/api/v3",
        AuthStyle::Bearer,
        None,
    );
    let minimax = preset(
        "minimax",
        "MiniMax",
        ProtocolFamily::OpenAiChatCompletions,
        "https://api.minimax.io/v1",
        AuthStyle::Bearer,
        None,
    );
    let xai = preset(
        "xai",
        "xAI",
        ProtocolFamily::OpenAiChatCompletions,
        "https://api.x.ai/v1",
        AuthStyle::Bearer,
        None,
    );
    let mistral = preset(
        "mistral",
        "Mistral AI",
        ProtocolFamily::OpenAiChatCompletions,
        "https://api.mistral.ai/v1",
        AuthStyle::Bearer,
        None,
    );
    let openrouter = preset(
        "openrouter",
        "OpenRouter",
        ProtocolFamily::OpenAiChatCompletions,
        "https://openrouter.ai/api/v1",
        AuthStyle::Bearer,
        None,
    );
    let ollama = preset(
        "ollama",
        "Ollama",
        ProtocolFamily::OpenAiChatCompletions,
        "http://127.0.0.1:11434/v1",
        AuthStyle::None,
        None,
    );

    vec![
        openai, anthropic, gemini, zhipu, deepseek, qwen, kimi, doubao, minimax, xai, mistral,
        openrouter, ollama,
    ]
}

/// Finds a built-in provider preset by stable identifier.
#[must_use]
pub fn preset_by_id(id: &str) -> Option<ProviderPreset> {
    built_in_presets()
        .into_iter()
        .find(|preset| preset.id == id)
}

/// Creates a custom OpenAI-compatible preset after validating its identifier and URL.
///
/// # Errors
///
/// Returns an error when the identifier or endpoint is invalid or unsafe.
pub fn custom_compatible_preset(
    id: impl Into<String>,
    display_name: impl Into<String>,
    base_url: impl Into<String>,
    allow_plain_http: bool,
) -> Result<ProviderPreset> {
    ProviderPreset::custom_compatible(id, display_name, base_url, allow_plain_http)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_have_unique_ids_and_no_secrets() {
        let presets = built_in_presets();
        let mut ids = presets.iter().map(|preset| &preset.id).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), presets.len());
        assert_eq!(presets.len(), 13);
        assert!(presets.iter().all(|preset| {
            !preset.base_url.contains('@')
                && !preset.headers.keys().any(|name| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key"
                    )
                })
        }));
    }

    #[test]
    fn zhipu_uses_coding_endpoint_and_official_thinking_shape() {
        let zhipu = preset_by_id("zhipu").expect("zhipu preset");
        assert_eq!(
            zhipu.base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(zhipu.default_model.as_deref(), Some("glm-5.2"));
        assert_eq!(zhipu.request_overrides["thinking"]["type"], "enabled");
        assert_eq!(zhipu.request_overrides["thinking"]["clear_thinking"], false);
        assert_eq!(zhipu.capabilities.context_window, Some(1_000_000));
        assert_eq!(zhipu.capabilities.max_output_tokens, Some(131_072));
    }

    #[test]
    fn compatible_endpoint_rejects_remote_plain_http() {
        let result =
            ProviderPreset::custom_compatible("unsafe", "Unsafe", "http://example.com/v1", false);
        assert!(result.is_err());
        assert!(
            ProviderPreset::custom_compatible("local", "Local", "http://127.0.0.1:8000/v1", false)
                .is_ok()
        );
        for rejected in [
            "https://user@example.com/v1",
            "https://user:password@example.com/v1",
            "https://example.com/v1?api_key=plaintext",
            "https://example.com/v1#credential",
        ] {
            assert!(
                ProviderPreset::custom_compatible("unsafe", "Unsafe", rejected, false).is_err(),
                "expected {rejected} to be rejected"
            );
        }
    }

    #[test]
    fn compatible_endpoint_allows_plain_http_only_with_explicit_opt_in() {
        assert!(
            ProviderPreset::custom_compatible(
                "selfhost",
                "SelfHost",
                "http://203.0.113.10:7000/v1",
                true
            )
            .is_ok(),
            "self-hosted plain HTTP requires the explicit opt-in"
        );
        assert!(
            ProviderPreset::custom_compatible(
                "selfhost",
                "SelfHost",
                "http://203.0.113.10:7000/v1",
                false
            )
            .is_err(),
            "the opt-in must stay opt-in"
        );
    }
}
