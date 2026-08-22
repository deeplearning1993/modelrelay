//! Strict validation for the Responses compact endpoint.

use thiserror::Error;

use crate::responses::{CompactionItem, ResponseOutputItem};

/// A compact response that violates the exact-one-item contract.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompactionValidationError {
    /// The compact response did not contain exactly one output item.
    #[error("compact response must contain exactly one output item, got {actual}")]
    WrongOutputCount {
        /// Number of output items actually returned.
        actual: usize,
    },
    /// The sole output item was not a standard compaction item.
    #[error("compact response output item must have type=compaction, got {actual_type}")]
    WrongOutputType {
        /// Wire type of the item actually returned.
        actual_type: &'static str,
    },
    /// The standard compaction item had no encrypted payload.
    #[error("compaction encrypted_content must not be empty")]
    EmptyEncryptedContent,
}

/// Validate and return the sole standard compaction item.
///
/// This rejects assistant messages even if their text looks like a summary.
///
/// # Errors
///
/// Returns [`CompactionValidationError`] when the output count is not one, the
/// sole item is not `type=compaction`, or its encrypted payload is empty.
pub fn validate_exactly_one_compaction(
    output: &[ResponseOutputItem],
) -> Result<&CompactionItem, CompactionValidationError> {
    if output.len() != 1 {
        return Err(CompactionValidationError::WrongOutputCount {
            actual: output.len(),
        });
    }

    match &output[0] {
        ResponseOutputItem::Compaction(item) => {
            if item.encrypted_content.is_empty() {
                Err(CompactionValidationError::EmptyEncryptedContent)
            } else {
                Ok(item)
            }
        }
        item => Err(CompactionValidationError::WrongOutputType {
            actual_type: item_type(item),
        }),
    }
}

const fn item_type(item: &ResponseOutputItem) -> &'static str {
    match item {
        ResponseOutputItem::Message(_) => "message",
        ResponseOutputItem::FunctionCall(_) => "function_call",
        ResponseOutputItem::FunctionCallOutput(_) => "function_call_output",
        ResponseOutputItem::Reasoning(_) => "reasoning",
        ResponseOutputItem::Compaction(_) => "compaction",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::responses::{ContentPart, MessageContent, MessageItem, MessageRole, ResponseItem};

    fn compaction(content: &str) -> ResponseItem {
        ResponseItem::Compaction(CompactionItem {
            id: Some("cmp-1".into()),
            encrypted_content: content.into(),
            extra: BTreeMap::new(),
        })
    }

    fn assistant_summary() -> ResponseItem {
        ResponseItem::Message(MessageItem {
            id: Some("msg-1".into()),
            role: MessageRole::Assistant,
            content: MessageContent::Parts(vec![ContentPart::OutputText {
                text: "A summary pretending to be compaction".into(),
                annotations: Vec::new(),
                logprobs: Vec::new(),
            }]),
            status: None,
            extra: BTreeMap::new(),
        })
    }

    #[test]
    fn accepts_exactly_one_nonempty_compaction_item() {
        let output = [compaction("encrypted")];
        let item = validate_exactly_one_compaction(&output).unwrap();

        assert_eq!(item.encrypted_content, "encrypted");
    }

    #[test]
    fn rejects_zero_or_multiple_items() {
        assert_eq!(
            validate_exactly_one_compaction(&[]),
            Err(CompactionValidationError::WrongOutputCount { actual: 0 })
        );
        assert_eq!(
            validate_exactly_one_compaction(&[compaction("a"), compaction("b")]),
            Err(CompactionValidationError::WrongOutputCount { actual: 2 })
        );
    }

    #[test]
    fn rejects_assistant_message_impersonation() {
        assert_eq!(
            validate_exactly_one_compaction(&[assistant_summary()]),
            Err(CompactionValidationError::WrongOutputType {
                actual_type: "message"
            })
        );
    }

    #[test]
    fn rejects_empty_encrypted_content() {
        assert_eq!(
            validate_exactly_one_compaction(&[compaction("")]),
            Err(CompactionValidationError::EmptyEncryptedContent)
        );
    }
}
