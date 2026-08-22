//! Portable context and deterministic replay planning across providers.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::responses::{ContentPart, MessageContent, MessageRole, ResponseItem, ToolOutput};

/// Provider-neutral message content safe to replay to another provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PortableContent {
    /// Plain text content.
    Text {
        /// Text safe to replay across providers.
        text: String,
    },
    /// A visual input referenced by file identifier or URI.
    ImageReference {
        /// Optional provider-neutral file identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        /// Optional image URI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        /// Requested image detail level, when supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A file input referenced by inline data, identifier, or URI.
    FileReference {
        /// Optional inline file data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        /// Optional provider-neutral file identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        /// Optional file URI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        /// Optional source filename.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// A model refusal retained as a typed content part.
    Refusal {
        /// Refusal text.
        text: String,
    },
}

/// Context that is semantically portable between provider protocols.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PortableContextItem {
    /// A provider-neutral conversation message.
    Message {
        /// Message author role.
        role: MessageRole,
        /// Portable message content.
        content: Vec<PortableContent>,
    },
    /// A provider-neutral tool invocation.
    ToolCall {
        /// Stable identifier correlating the call with its result.
        call_id: String,
        /// Invoked tool name.
        name: String,
        /// Complete JSON arguments encoded as a string.
        arguments: String,
    },
    /// Result of a prior tool invocation.
    ToolResult {
        /// Identifier of the corresponding tool call.
        call_id: String,
        /// Textual or structured tool output.
        output: ToolOutput,
    },
    /// A provider-neutral summary, never represented as assistant speech.
    NeutralSummary {
        /// Provider-neutral summary text.
        text: String,
        /// Source compaction identifier when the summary maps from compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_compaction_id: Option<String>,
    },
}

/// State that may only be returned to the provider that created it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueContextItem {
    /// Identifier of the provider that owns the encrypted state.
    pub provider_id: String,
    /// Upstream response associated with the state when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_response_id: Option<String>,
    /// Provider-encrypted payload that must never cross provider boundaries.
    pub encrypted_content: String,
}

/// A stored canonical context item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "portability", rename_all = "snake_case")]
pub enum ContextItem {
    /// Context safe to replay to any provider.
    Portable {
        /// Portable payload.
        item: PortableContextItem,
    },
    /// Encrypted context owned by exactly one provider.
    ProviderOpaque {
        /// Provider-owned payload.
        item: OpaqueContextItem,
    },
}

/// One monotonically ordered context record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEntry {
    /// Strictly increasing position in canonical history.
    pub ordinal: u64,
    /// Stored portable or provider-owned context.
    #[serde(flatten)]
    pub context: ContextItem,
}

/// Latest upstream state known for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCursor {
    /// Provider that owns the upstream continuation.
    pub provider_id: String,
    /// Upstream response identifier used to continue the conversation.
    pub previous_response_id: String,
    /// The upstream response already contains all history through this ordinal.
    pub through_ordinal: u64,
}

/// Upstream continuation selected by a replay plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamContinuation {
    /// Upstream response identifier used to continue the conversation.
    pub previous_response_id: String,
    /// Last canonical ordinal already represented upstream.
    pub through_ordinal: u64,
}

/// A provider-owned item deliberately excluded from portable replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedContextItem {
    /// Canonical history position of the omitted item.
    pub ordinal: u64,
    /// Provider that owns the omitted encrypted state.
    pub owner_provider_id: String,
    /// Reason the replay planner excluded the item.
    pub reason: OmittedContextReason,
}

/// Why an opaque item was excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmittedContextReason {
    /// The encrypted state belongs to another provider.
    ForeignProviderEncryptedState,
    /// The target owns the state, but no cursor proves it is upstream already.
    OwnerStateNotCoveredByCursor,
}

/// Complete instructions for submitting a turn to one target provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPlan {
    /// Provider selected for the next turn.
    pub target_provider_id: String,
    /// Existing upstream response to continue when one is valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<UpstreamContinuation>,
    /// Full portable history when no cursor exists, otherwise cursor deltas.
    pub items: Vec<PortableContextItem>,
    /// Provider-owned context deliberately withheld from replay.
    pub omitted: Vec<OmittedContextItem>,
}

/// Invalid history or cursor state.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReplayError {
    /// The requested target provider identifier is empty.
    #[error("target provider id must not be empty")]
    EmptyTargetProvider,
    /// Canonical context ordinals are not strictly increasing.
    #[error("context ordinals must be strictly increasing: {previous} then {current}")]
    NonIncreasingOrdinal {
        /// Ordinal of the preceding entry.
        previous: u64,
        /// Invalid current ordinal.
        current: u64,
    },
    /// More than one cursor was supplied for the same provider.
    #[error("duplicate cursor for provider {0}")]
    DuplicateProviderCursor(String),
    /// A cursor claims history beyond the last known entry.
    #[error(
        "cursor for provider {provider_id} ends at {through_ordinal}, beyond history ordinal {last_ordinal:?}"
    )]
    CursorBeyondHistory {
        /// Provider that owns the invalid cursor.
        provider_id: String,
        /// Last ordinal the cursor claims is already upstream.
        through_ordinal: u64,
        /// Last ordinal present in canonical history, if any.
        last_ordinal: Option<u64>,
    },
    /// A cursor has an empty provider or upstream response identifier.
    #[error("provider cursor fields must not be empty")]
    EmptyCursorField,
}

/// An item that cannot be represented in portable context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PortableContextError {
    /// Provider reasoning cannot be replayed as portable context.
    #[error("reasoning is provider-owned and cannot be converted to portable context")]
    ProviderReasoning,
    /// Encrypted compaction needs an explicit neutral-summary mapping.
    #[error("encrypted compaction is provider-owned; use a neutral summary mapping")]
    EncryptedCompaction,
}

impl TryFrom<&ResponseItem> for PortableContextItem {
    type Error = PortableContextError;

    fn try_from(item: &ResponseItem) -> Result<Self, Self::Error> {
        match item {
            ResponseItem::Message(message) => Ok(Self::Message {
                role: message.role,
                content: match &message.content {
                    MessageContent::Text(text) => {
                        vec![PortableContent::Text { text: text.clone() }]
                    }
                    MessageContent::Parts(parts) => {
                        parts.iter().map(PortableContent::from).collect()
                    }
                },
            }),
            ResponseItem::FunctionCall(call) => Ok(Self::ToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
            ResponseItem::FunctionCallOutput(output) => Ok(Self::ToolResult {
                call_id: output.call_id.clone(),
                output: output.output.clone(),
            }),
            ResponseItem::Reasoning(_) => Err(PortableContextError::ProviderReasoning),
            ResponseItem::Compaction(_) => Err(PortableContextError::EncryptedCompaction),
        }
    }
}

impl From<&ContentPart> for PortableContent {
    fn from(part: &ContentPart) -> Self {
        match part {
            ContentPart::InputText { text } | ContentPart::OutputText { text, .. } => {
                Self::Text { text: text.clone() }
            }
            ContentPart::InputImage {
                file_id,
                image_url,
                detail,
            } => Self::ImageReference {
                file_id: file_id.clone(),
                uri: image_url.clone(),
                detail: detail.clone(),
            },
            ContentPart::InputFile {
                file_data,
                file_id,
                file_url,
                filename,
            } => Self::FileReference {
                data: file_data.clone(),
                file_id: file_id.clone(),
                uri: file_url.clone(),
                filename: filename.clone(),
            },
            ContentPart::Refusal { refusal } => Self::Refusal {
                text: refusal.clone(),
            },
        }
    }
}

/// Plan a turn without leaking encrypted reasoning across providers.
///
/// If the target provider has a cursor, the plan reuses its upstream response
/// and sends only portable items after that cursor. This also handles switching
/// away and later switching back: portable turns produced while away become
/// deltas on top of the target provider's last known state.
///
/// # Errors
///
/// Returns [`ReplayError`] when the target identifier or cursor fields are
/// empty, history ordinals are not strictly increasing, provider cursors are
/// duplicated, or a cursor claims context beyond known history.
pub fn plan_replay(
    entries: &[ContextEntry],
    cursors: &[ProviderCursor],
    target_provider_id: &str,
) -> Result<ReplayPlan, ReplayError> {
    if target_provider_id.trim().is_empty() {
        return Err(ReplayError::EmptyTargetProvider);
    }

    let mut previous_ordinal = None;
    for entry in entries {
        if let Some(previous) = previous_ordinal
            && entry.ordinal <= previous
        {
            return Err(ReplayError::NonIncreasingOrdinal {
                previous,
                current: entry.ordinal,
            });
        }
        previous_ordinal = Some(entry.ordinal);
    }

    let mut providers = BTreeSet::new();
    let mut target_cursor = None;
    for cursor in cursors {
        if cursor.provider_id.trim().is_empty() || cursor.previous_response_id.trim().is_empty() {
            return Err(ReplayError::EmptyCursorField);
        }
        if !providers.insert(cursor.provider_id.as_str()) {
            return Err(ReplayError::DuplicateProviderCursor(
                cursor.provider_id.clone(),
            ));
        }
        if cursor.through_ordinal > previous_ordinal.unwrap_or(0)
            || (entries.is_empty() && cursor.through_ordinal > 0)
        {
            return Err(ReplayError::CursorBeyondHistory {
                provider_id: cursor.provider_id.clone(),
                through_ordinal: cursor.through_ordinal,
                last_ordinal: previous_ordinal,
            });
        }
        if cursor.provider_id == target_provider_id {
            target_cursor = Some(cursor);
        }
    }

    let continuation = target_cursor.map(|cursor| UpstreamContinuation {
        previous_response_id: cursor.previous_response_id.clone(),
        through_ordinal: cursor.through_ordinal,
    });
    let after_ordinal = target_cursor.map(|cursor| cursor.through_ordinal);
    let mut items = Vec::new();
    let mut omitted = Vec::new();

    for entry in entries {
        if after_ordinal.is_some_and(|ordinal| entry.ordinal <= ordinal) {
            continue;
        }
        match &entry.context {
            ContextItem::Portable { item } => items.push(item.clone()),
            ContextItem::ProviderOpaque { item } => omitted.push(OmittedContextItem {
                ordinal: entry.ordinal,
                owner_provider_id: item.provider_id.clone(),
                reason: if item.provider_id == target_provider_id {
                    OmittedContextReason::OwnerStateNotCoveredByCursor
                } else {
                    OmittedContextReason::ForeignProviderEncryptedState
                },
            }),
        }
    }

    Ok(ReplayPlan {
        target_provider_id: target_provider_id.to_owned(),
        continuation,
        items,
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::responses::{FunctionCallItem, ReasoningItem};

    fn portable(ordinal: u64, text: &str) -> ContextEntry {
        ContextEntry {
            ordinal,
            context: ContextItem::Portable {
                item: PortableContextItem::Message {
                    role: MessageRole::User,
                    content: vec![PortableContent::Text { text: text.into() }],
                },
            },
        }
    }

    fn opaque(ordinal: u64, provider: &str) -> ContextEntry {
        ContextEntry {
            ordinal,
            context: ContextItem::ProviderOpaque {
                item: OpaqueContextItem {
                    provider_id: provider.into(),
                    upstream_response_id: Some(format!("{provider}-response")),
                    encrypted_content: "secret-state".into(),
                },
            },
        }
    }

    #[test]
    fn new_provider_receives_full_portable_history_only() {
        let plan = plan_replay(
            &[
                portable(1, "hello"),
                opaque(2, "openai"),
                portable(3, "next"),
            ],
            &[],
            "zhipu",
        )
        .unwrap();

        assert!(plan.continuation.is_none());
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.omitted.len(), 1);
        assert_eq!(
            plan.omitted[0].reason,
            OmittedContextReason::ForeignProviderEncryptedState
        );
    }

    #[test]
    fn switching_back_uses_old_cursor_and_replays_only_new_portable_items() {
        let entries = [
            portable(1, "first"),
            opaque(2, "openai"),
            portable(3, "asked while on glm"),
            opaque(4, "zhipu"),
            portable(5, "glm answer"),
        ];
        let cursors = [ProviderCursor {
            provider_id: "openai".into(),
            previous_response_id: "resp_openai_1".into(),
            through_ordinal: 2,
        }];

        let plan = plan_replay(&entries, &cursors, "openai").unwrap();

        assert_eq!(
            plan.continuation,
            Some(UpstreamContinuation {
                previous_response_id: "resp_openai_1".into(),
                through_ordinal: 2,
            })
        );
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.omitted[0].owner_provider_id, "zhipu");
    }

    #[test]
    fn rejects_non_monotonic_history() {
        let error = plan_replay(&[portable(2, "a"), portable(2, "b")], &[], "openai").unwrap_err();

        assert_eq!(
            error,
            ReplayError::NonIncreasingOrdinal {
                previous: 2,
                current: 2,
            }
        );
    }

    #[test]
    fn rejects_duplicate_provider_cursors() {
        let cursors = [
            ProviderCursor {
                provider_id: "openai".into(),
                previous_response_id: "resp_1".into(),
                through_ordinal: 1,
            },
            ProviderCursor {
                provider_id: "openai".into(),
                previous_response_id: "resp_2".into(),
                through_ordinal: 1,
            },
        ];

        assert_eq!(
            plan_replay(&[portable(1, "a")], &cursors, "openai"),
            Err(ReplayError::DuplicateProviderCursor("openai".into()))
        );
    }

    #[test]
    fn rejects_cursor_beyond_known_history() {
        let cursors = [ProviderCursor {
            provider_id: "openai".into(),
            previous_response_id: "resp_1".into(),
            through_ordinal: 3,
        }];

        assert_eq!(
            plan_replay(&[portable(1, "a")], &cursors, "openai"),
            Err(ReplayError::CursorBeyondHistory {
                provider_id: "openai".into(),
                through_ordinal: 3,
                last_ordinal: Some(1),
            })
        );
    }

    #[test]
    fn response_items_convert_without_promoting_reasoning_to_messages() {
        let call = ResponseItem::FunctionCall(FunctionCallItem {
            id: None,
            call_id: "call-1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
            status: None,
            extra: BTreeMap::new(),
        });
        let reasoning = ResponseItem::Reasoning(ReasoningItem {
            id: None,
            summary: Vec::new(),
            encrypted_content: Some("opaque".into()),
            status: None,
            extra: BTreeMap::new(),
        });

        assert!(matches!(
            PortableContextItem::try_from(&call).unwrap(),
            PortableContextItem::ToolCall { .. }
        ));
        assert_eq!(
            PortableContextItem::try_from(&reasoning),
            Err(PortableContextError::ProviderReasoning)
        );
    }

    #[test]
    fn neutral_summary_is_a_distinct_context_type() {
        let item = PortableContextItem::NeutralSummary {
            text: "Summary".into(),
            source_compaction_id: Some("cmp-1".into()),
        };
        let json = serde_json::to_value(item).unwrap();

        assert_eq!(json["type"], "neutral_summary");
        assert!(json.get("role").is_none());
    }
}
