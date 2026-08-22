//! Deterministic model catalog validation, ordering, hiding, and publication.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Origin and upstream routing identity of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSource {
    /// A model served by the official `ChatGPT` backend.
    Official,
    /// A model served by a configured external provider.
    External {
        /// Stable identifier of the provider configuration.
        provider_id: String,
        /// Model identifier sent to the provider upstream.
        upstream_model_id: String,
    },
}

/// Capabilities used for routing-time compatibility checks.
// These independent wire capabilities intentionally remain directly serializable flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Whether the model accepts the Responses protocol.
    #[serde(default)]
    pub responses: bool,
    /// Whether the model can stream response events.
    #[serde(default)]
    pub streaming: bool,
    /// Whether the model can stream over a WebSocket transport.
    #[serde(default)]
    pub websocket: bool,
    /// Whether the model supports tool definitions and calls.
    #[serde(default)]
    pub tools: bool,
    /// Whether the model can emit multiple tool calls concurrently.
    #[serde(default)]
    pub parallel_tool_calls: bool,
    /// Whether the model accepts visual inputs.
    #[serde(default)]
    pub vision: bool,
    /// Whether the model exposes reasoning support.
    #[serde(default)]
    pub reasoning: bool,
    /// Whether the model supports standard Responses compaction.
    #[serde(default)]
    pub compaction: bool,
    /// Maximum input context size in tokens when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Maximum output size in tokens when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// One model candidate before user policy and client capacity are applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Stable model identifier exposed to clients.
    pub id: String,
    /// Human-readable model name exposed in a picker.
    pub display_name: String,
    /// Origin and upstream routing identity.
    pub source: ModelSource,
    /// Capabilities advertised for routing and client compatibility.
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// Provider/client catalog fields not interpreted by the publication policy.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// User-controlled catalog policy.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CatalogPolicy {
    /// Model IDs in desired picker order. Unlisted models retain source order.
    #[serde(default)]
    pub order: Vec<String>,
    /// Model IDs omitted from the published picker.
    #[serde(default)]
    pub hidden: BTreeSet<String>,
    /// Maximum number of models supported by the current client profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<usize>,
}

/// Deterministic output of catalog publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedCatalog {
    /// Models included in the client picker, in publication order.
    pub models: Vec<CatalogModel>,
    /// Known models excluded by policy or picker capacity.
    pub omitted: Vec<CatalogOmission>,
    /// Unknown IDs in `order` or `hidden`, useful for UI diagnostics.
    pub unknown_policy_ids: Vec<String>,
}

/// A known model omitted from publication and the exact reason why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogOmission {
    /// Identifier of the excluded model.
    pub model_id: String,
    /// Reason the model was excluded.
    pub reason: CatalogOmissionReason,
}

/// Reasons a valid model did not fit in the published picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogOmissionReason {
    /// The user explicitly hid the model.
    Hidden,
    /// The client picker had no remaining capacity.
    Capacity,
}

/// Invalid catalog input that cannot be published safely.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatalogError {
    /// A candidate model has an empty identifier.
    #[error("model id must not be empty")]
    EmptyModelId,
    /// Multiple candidates use the same identifier.
    #[error("duplicate model id: {0}")]
    DuplicateModelId(String),
    /// A model was supplied in a list that does not match its source.
    #[error("model {model_id} was supplied in the {list_name} list but has source {actual_source}")]
    SourceMismatch {
        /// Identifier of the misplaced model.
        model_id: String,
        /// Name of the list in which the model was supplied.
        list_name: &'static str,
        /// Source declared by the model.
        actual_source: &'static str,
    },
    /// An external model is missing information needed to route it.
    #[error("external model {model_id} has an empty {field}")]
    EmptyExternalRoutingField {
        /// Identifier of the invalid external model.
        model_id: String,
        /// Name of the empty routing field.
        field: &'static str,
    },
}

/// Merge and publish official and enabled external model candidates.
///
/// User-listed IDs come first in the exact requested order. Remaining models
/// preserve their source order (official first, then external). Hiding is
/// applied before capacity, making omission reasons stable and inspectable.
///
/// # Errors
///
/// Returns [`CatalogError`] when a model identifier is empty or duplicated,
/// when a candidate appears in the wrong source list, or when an external
/// candidate lacks its provider or upstream model identifier.
pub fn publish_catalog(
    official: &[CatalogModel],
    external: &[CatalogModel],
    policy: &CatalogPolicy,
) -> Result<PublishedCatalog, CatalogError> {
    validate_sources(official, true)?;
    validate_sources(external, false)?;

    let mut candidates = Vec::with_capacity(official.len() + external.len());
    candidates.extend(official.iter().cloned());
    candidates.extend(external.iter().cloned());

    let mut by_id = BTreeMap::new();
    for (source_index, model) in candidates.into_iter().enumerate() {
        if model.id.trim().is_empty() {
            return Err(CatalogError::EmptyModelId);
        }
        let model_id = model.id.clone();
        if by_id
            .insert(model_id.clone(), (source_index, model))
            .is_some()
        {
            return Err(CatalogError::DuplicateModelId(model_id));
        }
    }

    let known_ids: BTreeSet<&str> = by_id.keys().map(String::as_str).collect();
    let unknown_policy_ids: BTreeSet<String> = policy
        .order
        .iter()
        .chain(policy.hidden.iter())
        .filter(|id| !known_ids.contains(id.as_str()))
        .cloned()
        .collect();

    let mut ordered_ids = Vec::with_capacity(by_id.len());
    let mut seen_order = BTreeSet::new();
    for id in &policy.order {
        if known_ids.contains(id.as_str()) && seen_order.insert(id.as_str()) {
            ordered_ids.push(id.as_str());
        }
    }
    let mut unlisted: Vec<_> = by_id
        .iter()
        .filter(|(id, _)| !seen_order.contains(id.as_str()))
        .collect();
    unlisted.sort_by_key(|(_, (source_index, _))| *source_index);
    ordered_ids.extend(unlisted.into_iter().map(|(id, _)| id.as_str()));

    let mut models = Vec::new();
    let mut omitted = Vec::new();
    for id in ordered_ids {
        let model = &by_id[id].1;
        if policy.hidden.contains(id) {
            omitted.push(CatalogOmission {
                model_id: id.to_owned(),
                reason: CatalogOmissionReason::Hidden,
            });
        } else if policy
            .capacity
            .is_some_and(|capacity| models.len() >= capacity)
        {
            omitted.push(CatalogOmission {
                model_id: id.to_owned(),
                reason: CatalogOmissionReason::Capacity,
            });
        } else {
            models.push(model.clone());
        }
    }

    Ok(PublishedCatalog {
        models,
        omitted,
        unknown_policy_ids: unknown_policy_ids.into_iter().collect(),
    })
}

fn validate_sources(models: &[CatalogModel], official: bool) -> Result<(), CatalogError> {
    for model in models {
        match (&model.source, official) {
            (ModelSource::Official, true) => {}
            (
                ModelSource::External {
                    provider_id,
                    upstream_model_id,
                },
                false,
            ) => {
                if provider_id.trim().is_empty() {
                    return Err(CatalogError::EmptyExternalRoutingField {
                        model_id: model.id.clone(),
                        field: "provider_id",
                    });
                }
                if upstream_model_id.trim().is_empty() {
                    return Err(CatalogError::EmptyExternalRoutingField {
                        model_id: model.id.clone(),
                        field: "upstream_model_id",
                    });
                }
            }
            (ModelSource::Official, false) => {
                return Err(CatalogError::SourceMismatch {
                    model_id: model.id.clone(),
                    list_name: "external",
                    actual_source: "official",
                });
            }
            (ModelSource::External { .. }, true) => {
                return Err(CatalogError::SourceMismatch {
                    model_id: model.id.clone(),
                    list_name: "official",
                    actual_source: "external",
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn official(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            source: ModelSource::Official,
            capabilities: ModelCapabilities::default(),
            extra: BTreeMap::new(),
        }
    }

    fn external(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            source: ModelSource::External {
                provider_id: "provider".into(),
                upstream_model_id: id.into(),
            },
            capabilities: ModelCapabilities::default(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn publication_preserves_uninterpreted_catalog_fields() {
        let mut model = official("gpt");
        model.extra.insert("visibility".into(), json!("list"));

        let result = publish_catalog(&[model.clone()], &[], &CatalogPolicy::default()).unwrap();

        assert_eq!(result.models, vec![model]);
    }

    #[test]
    fn applies_order_then_hiding_then_capacity() {
        let policy = CatalogPolicy {
            order: vec!["glm-5.2".into(), "gpt-b".into()],
            hidden: BTreeSet::from(["gpt-b".into()]),
            capacity: Some(2),
        };
        let result = publish_catalog(
            &[official("gpt-a"), official("gpt-b")],
            &[external("glm-5.2"), external("deepseek")],
            &policy,
        )
        .unwrap();

        assert_eq!(
            result
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["glm-5.2", "gpt-a"]
        );
        assert_eq!(
            result.omitted,
            vec![
                CatalogOmission {
                    model_id: "gpt-b".into(),
                    reason: CatalogOmissionReason::Hidden,
                },
                CatalogOmission {
                    model_id: "deepseek".into(),
                    reason: CatalogOmissionReason::Capacity,
                }
            ]
        );
    }

    #[test]
    fn reports_unknown_policy_ids_without_publishing_phantoms() {
        let result = publish_catalog(
            &[official("gpt")],
            &[],
            &CatalogPolicy {
                order: vec!["missing-b".into(), "missing-a".into()],
                hidden: BTreeSet::from(["missing-a".into()]),
                capacity: None,
            },
        )
        .unwrap();

        assert_eq!(result.models, vec![official("gpt")]);
        assert_eq!(result.unknown_policy_ids, ["missing-a", "missing-b"]);
    }

    #[test]
    fn rejects_duplicate_ids_across_origins() {
        let error = publish_catalog(
            &[official("same")],
            &[external("same")],
            &CatalogPolicy::default(),
        )
        .unwrap_err();

        assert_eq!(error, CatalogError::DuplicateModelId("same".into()));
    }

    #[test]
    fn rejects_model_in_wrong_source_list() {
        let error =
            publish_catalog(&[external("wrong")], &[], &CatalogPolicy::default()).unwrap_err();

        assert_eq!(
            error,
            CatalogError::SourceMismatch {
                model_id: "wrong".into(),
                list_name: "official",
                actual_source: "external",
            }
        );
    }

    #[test]
    fn zero_capacity_publishes_nothing() {
        let result = publish_catalog(
            &[official("gpt")],
            &[external("glm")],
            &CatalogPolicy {
                capacity: Some(0),
                ..CatalogPolicy::default()
            },
        )
        .unwrap();

        assert!(result.models.is_empty());
        assert!(
            result
                .omitted
                .iter()
                .all(|item| item.reason == CatalogOmissionReason::Capacity)
        );
    }
}
