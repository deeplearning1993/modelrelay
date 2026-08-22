use std::collections::{HashMap, HashSet};

use cmr_providers::preset_by_id;
use cmr_storage::{ModelConfig, RouterConfig};
use serde_json::{Value, json};

use crate::{Result, RouterError};

pub(crate) fn is_published_external_model(config: &RouterConfig, model: &ModelConfig) -> bool {
    model.enabled
        && !config.hidden_models.iter().any(|id| id == &model.id)
        && config
            .providers
            .iter()
            .any(|provider| provider.enabled && provider.id == model.provider)
}

pub(crate) fn merge_catalog(config: &RouterConfig, mut official: Value) -> Result<Value> {
    let models = official
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            RouterError::upstream(
                axum::http::StatusCode::BAD_GATEWAY,
                "official catalog has no models array",
            )
        })?;
    // Clone one official entry as the structural template for injected
    // external models. Building the entry from a hardcoded field list breaks
    // whenever a newer Codex client starts requiring a field the list does not
    // know; inheriting the official shape keeps external entries renderable by
    // the exact client that produced the catalog.
    let template = models
        .iter()
        .find(|model| model.as_object().is_some())
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Collision checks must cover the signed-in account's complete catalog,
    // including official entries hidden only from the picker. Apply the same
    // publication predicate used for injection and routing so an external
    // mapping that cannot be routed cannot shadow an official model either.
    let all_official_ids: HashSet<String> = models
        .iter()
        .filter_map(model_id)
        .map(str::to_owned)
        .collect();
    let hidden: HashSet<&str> = config.hidden_models.iter().map(String::as_str).collect();
    models.retain(|model| model_id(model).is_some_and(|id| !hidden.contains(id)));
    let official_ids: HashSet<String> = models
        .iter()
        .filter_map(model_id)
        .map(str::to_owned)
        .collect();
    for model in config
        .models
        .iter()
        .filter(|model| is_published_external_model(config, model))
    {
        if all_official_ids.contains(&model.id) {
            return Err(RouterError::bad_request(format!(
                "external model id collides with official catalog: {}",
                model.id
            )));
        }
    }

    for model in config
        .models
        .iter()
        .filter(|model| is_published_external_model(config, model))
    {
        let max_output_tokens = model.max_output_tokens.or_else(|| {
            config
                .providers
                .iter()
                .find(|provider| provider.id == model.provider)
                .and_then(|provider| preset_by_id(&provider.preset))
                .and_then(|preset| preset.capabilities.max_output_tokens)
        });
        models.push(external_model(model, max_output_tokens, &template));
    }

    let ranks: HashMap<&str, usize> = config
        .catalog_order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let fallback = ranks.len();
    models.sort_by_key(|model| {
        let id = model_id(model).unwrap_or_default();
        let rank = ranks.get(id).copied().unwrap_or(fallback);
        let configured = config
            .models
            .iter()
            .find(|entry| entry.id == id)
            .map_or(i32::MAX, |entry| entry.order);
        (rank, configured)
    });

    // Capacity limits optional external entries, never the signed-in account's
    // official catalog. If official models alone exceed the configured capacity,
    // all of them remain visible and no external models are admitted.
    let mut remaining_external = config.picker_capacity.saturating_sub(official_ids.len());
    models.retain(|model| {
        if model_id(model).is_some_and(|id| official_ids.contains(id)) {
            true
        } else if remaining_external > 0 {
            remaining_external -= 1;
            true
        } else {
            false
        }
    });
    for (priority, model) in models.iter_mut().enumerate() {
        if let Some(object) = model.as_object_mut() {
            object.insert("priority".into(), json!(priority));
        }
    }
    Ok(official)
}

pub(crate) fn model_id(model: &Value) -> Option<&str> {
    model
        .get("slug")
        .or_else(|| model.get("id"))
        .or_else(|| model.get("model"))?
        .as_str()
}

pub(crate) fn injected_external_model_ids(config: &RouterConfig, catalog: &Value) -> Vec<String> {
    let mut ids: Vec<_> = catalog
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("cmr_external").and_then(Value::as_bool) == Some(true))
        .filter_map(model_id)
        .filter(|id| {
            config
                .models
                .iter()
                .any(|model| model.id == *id && is_published_external_model(config, model))
        })
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Overrides `key` only when the cloned template already carries it, so the
/// external entry keeps the exact field set of the client's own catalog.
fn set_if_present(object: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    if object.contains_key(key) {
        object.insert(key.to_owned(), value);
    }
}

/// Builds the picker entry for one external model by cloning an official
/// catalog entry and overriding identity, capability, and routing fields.
/// Template cloning mirrors the approach that already worked with real Codex
/// clients: every official field the current client expects stays present, so
/// the picker never silently drops the injected entry for a missing field.
fn external_model(model: &ModelConfig, max_output_tokens: Option<u64>, template: &Value) -> Value {
    let context = model.context_window.unwrap_or(128_000);
    let mut entry = template.clone();
    let Some(object) = entry.as_object_mut() else {
        return entry;
    };
    // Every identifier variant current and older Codex catalogs have used.
    for key in ["slug", "id", "model"] {
        object.insert(key.to_owned(), json!(model.id));
    }
    set_if_present(object, "name", json!(model.id));
    object.insert("display_name".to_owned(), json!(model.display_name));
    for key in ["displayName", "title"] {
        set_if_present(object, key, json!(model.display_name));
    }
    object.insert(
        "description".to_owned(),
        json!(format!(
            "External model via {} (local ModelRelay)",
            model.provider
        )),
    );
    for key in ["context_window", "max_context_window"] {
        object.insert(key.to_owned(), json!(context));
    }
    for key in ["contextWindow", "context_length", "max_context_tokens"] {
        set_if_present(object, key, json!(context));
    }
    if let Some(limit) = max_output_tokens {
        set_if_present(object, "max_output_tokens", json!(limit));
        for key in ["maxOutputTokens", "max_completion_tokens"] {
            set_if_present(object, key, json!(limit));
        }
    }
    object.insert("visibility".to_owned(), json!("list"));
    set_if_present(object, "hidden", json!(false));
    object.insert("supported_in_api".to_owned(), json!(true));
    object.insert("supports_parallel_tool_calls".to_owned(), json!(true));
    set_if_present(object, "supports_streaming", json!(true));
    set_if_present(object, "supports_function_calling", json!(true));
    set_if_present(object, "supports_reasoning_summaries", json!(true));
    set_if_present(object, "is_default", json!(false));
    if !object.contains_key("default_reasoning_level") {
        object.insert("default_reasoning_level".to_owned(), json!("medium"));
    }
    if !object.contains_key("supported_reasoning_levels") {
        object.insert(
            "supported_reasoning_levels".to_owned(),
            json!([
                {"effort": "low", "description": "Low"},
                {"effort": "medium", "description": "Medium"},
                {"effort": "high", "description": "High"}
            ]),
        );
    }
    // The Rust router has no vision MCP bridge: external models are text-only.
    object.insert("input_modalities".to_owned(), json!(["text"]));
    set_if_present(object, "supports_image_detail_original", json!(false));
    for key in ["additional_speed_tiers", "service_tiers"] {
        set_if_present(object, key, json!([]));
    }
    set_if_present(object, "default_service_tier", Value::Null);
    for key in [
        "supports_web_search",
        "supports_file_search",
        "supports_computer_use",
        "supports_search_tool",
    ] {
        set_if_present(object, key, json!(false));
    }
    for key in ["upgrade", "upgrade_info", "deprecation"] {
        set_if_present(object, key, Value::Null);
    }
    // Router-private markers used by health reporting and request routing.
    object.insert("cmr_provider".to_owned(), json!(model.provider));
    object.insert("cmr_external".to_owned(), json!(true));
    if let Some(limit) = max_output_tokens {
        object.insert("cmr_max_output_tokens".to_owned(), json!(limit));
    }
    entry
}

#[cfg(test)]
mod tests {
    use cmr_storage::{ModelConfig, ProviderConfig};
    use serde_json::json;

    use super::*;

    fn external(id: &str, provider: &str, order: i32) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            display_name: id.into(),
            provider: provider.into(),
            upstream_model: id.into(),
            order,
            enabled: true,
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
        }
    }

    #[test]
    fn order_hide_capacity_apply_to_both_namespaces() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-5.2".into(),
            display_name: "GLM".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.2".into(),
            order: 0,
            enabled: true,
            context_window: Some(1_000_000),
            max_output_tokens: Some(131_072),
        });
        config.catalog_order = vec!["glm-5.2".into(), "gpt-b".into()];
        config.hidden_models = vec!["gpt-a".into()];
        config.picker_capacity = 2;
        let merged = merge_catalog(
            &config,
            json!({"models":[{"slug":"gpt-a"},{"slug":"gpt-b"}]}),
        )
        .expect("merge");
        let ids: Vec<_> = merged["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(model_id)
            .collect();
        assert_eq!(ids, vec!["glm-5.2", "gpt-b"]);
        let glm = merged["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model_id(model) == Some("glm-5.2"))
            .expect("external model");
        assert_eq!(glm["cmr_max_output_tokens"], 131_072);
        assert!(glm.get("max_output_tokens").is_none());
    }

    #[test]
    fn external_entry_inherits_official_template_fields() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-5.3".into(),
            display_name: "GLM-5.3".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.3".into(),
            order: 0,
            enabled: true,
            context_window: Some(1_000_000),
            max_output_tokens: Some(131_072),
        });
        let merged = merge_catalog(
            &config,
            json!({"models":[{
                "slug": "gpt-official",
                "id": "gpt-official",
                "model": "gpt-official-alias",
                "name": "gpt-official",
                "display_name": "Official Model",
                "max_output_tokens": 4096,
                "supports_streaming": true,
                "supports_function_calling": true,
                "supports_reasoning_summaries": true,
                "is_default": true,
                "service_tiers": ["priority"],
                "default_service_tier": "priority",
                "upgrade": {"cta": "upgrade now"},
                "supports_web_search": true,
                "supports_image_detail_original": true
            }]}),
        )
        .expect("merge");
        let glm = merged["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model_id(model) == Some("glm-5.3"))
            .expect("external entry present");
        // Structural fields inherited from the official template stay present
        // so the current client never filters the entry for a missing field.
        assert_eq!(glm["supports_streaming"], true);
        assert_eq!(glm["supports_function_calling"], true);
        assert_eq!(glm["supports_reasoning_summaries"], true);
        // Identity and capability fields are overridden.
        for key in ["slug", "id", "model", "name"] {
            assert_eq!(
                glm[key], "glm-5.3",
                "{key} must identify the external model"
            );
        }
        assert_eq!(glm["display_name"], "GLM-5.3");
        assert_eq!(glm["context_window"], 1_000_000);
        assert_eq!(glm["max_context_window"], 1_000_000);
        assert_eq!(glm["max_output_tokens"], 131_072);
        // Marketing and unsupported-capability fields are neutralized.
        assert_eq!(glm["service_tiers"], json!([]));
        assert!(glm["default_service_tier"].is_null());
        assert!(glm["upgrade"].is_null());
        assert_eq!(glm["supports_web_search"], false);
        assert_eq!(glm["supports_image_detail_original"], false);
        assert_eq!(glm["is_default"], false);
        assert_eq!(glm["visibility"], "list");
        assert_eq!(glm["input_modalities"], json!(["text"]));
        // Router-private markers used by health reporting and routing.
        assert_eq!(glm["cmr_external"], true);
        assert_eq!(glm["cmr_provider"], "zhipu");
        assert_eq!(glm["cmr_max_output_tokens"], 131_072);
    }

    #[test]
    fn disabled_provider_models_are_not_published() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "disabled-provider".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: false,
            allow_insecure_http: false,
        });
        config
            .models
            .push(external("disabled-model", "disabled-provider", 0));

        let merged = merge_catalog(&config, json!({"models":[{"slug":"gpt-a"}]})).expect("merge");
        let ids: Vec<_> = merged["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(model_id)
            .collect();
        assert_eq!(ids, vec!["gpt-a"]);
    }

    #[test]
    fn published_external_definition_includes_provider_model_and_hide_state() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "published-test-provider".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        let mut model = external("external-a", "published-test-provider", 0);
        assert!(is_published_external_model(&config, &model));

        model.enabled = false;
        assert!(!is_published_external_model(&config, &model));
        model.enabled = true;

        config.hidden_models.push(model.id.clone());
        assert!(!is_published_external_model(&config, &model));
        config.hidden_models.clear();
        config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "published-test-provider")
            .expect("test provider")
            .enabled = false;
        assert!(!is_published_external_model(&config, &model));
    }

    #[test]
    fn hidden_external_model_neither_collides_with_nor_replaces_official_id() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "external".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(external("gpt-hidden", "external", 0));
        config.hidden_models.push("gpt-hidden".into());

        let merged = merge_catalog(
            &config,
            json!({"models":[{"slug":"gpt-hidden"},{"slug":"gpt-visible"}]}),
        )
        .expect("hidden external mapping is not publishable");

        let ids: Vec<_> = merged["models"]
            .as_array()
            .expect("models")
            .iter()
            .filter_map(model_id)
            .collect();
        assert_eq!(ids, vec!["gpt-visible"]);
    }

    #[test]
    fn capacity_truncates_only_external_models() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "external".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.extend([
            external("external-a", "external", 1),
            external("external-b", "external", 0),
        ]);
        config.catalog_order = vec!["external-b".into(), "gpt-c".into(), "external-a".into()];
        config.picker_capacity = 4;

        let merged = merge_catalog(
            &config,
            json!({"models":[
                {"slug":"gpt-a"},
                {"slug":"gpt-b"},
                {"slug":"gpt-c"}
            ]}),
        )
        .expect("merge");
        let ids: Vec<_> = merged["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(model_id)
            .collect();
        assert_eq!(ids, vec!["external-b", "gpt-c", "gpt-a", "gpt-b"]);
        assert_eq!(
            injected_external_model_ids(&config, &merged),
            ["external-b"]
        );
    }

    #[test]
    fn official_catalog_may_exceed_capacity_without_truncation() {
        let config = RouterConfig {
            picker_capacity: 2,
            ..RouterConfig::default()
        };

        let merged = merge_catalog(
            &config,
            json!({"models":[
                {"slug":"gpt-a"},
                {"slug":"gpt-b"},
                {"slug":"gpt-c"}
            ]}),
        )
        .expect("merge");
        let ids: Vec<_> = merged["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(model_id)
            .collect();
        assert_eq!(ids, vec!["gpt-a", "gpt-b", "gpt-c"]);
    }
}
