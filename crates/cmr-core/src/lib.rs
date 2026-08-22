//! Canonical, provider-neutral types used by Codex Model Router.
//!
//! The crate deliberately contains no transport, credential, or persistence
//! code. It defines the contracts shared by those layers and keeps
//! provider-encrypted state out of cross-provider replay.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod compaction;
pub mod context;
pub mod responses;

pub use catalog::{
    CatalogError, CatalogModel, CatalogOmission, CatalogOmissionReason, CatalogPolicy,
    ModelCapabilities, ModelSource, PublishedCatalog, publish_catalog,
};
pub use compaction::{CompactionValidationError, validate_exactly_one_compaction};
pub use context::{
    ContextEntry, ContextItem, OmittedContextItem, OmittedContextReason, OpaqueContextItem,
    PortableContent, PortableContextError, PortableContextItem, ProviderCursor, ReplayError,
    ReplayPlan, UpstreamContinuation, plan_replay,
};
pub use responses::{
    CompactionItem, ContentPart, ContentPartEvent, ErrorEvent, FunctionArgumentsDeltaEvent,
    FunctionArgumentsDoneEvent, FunctionCallItem, FunctionCallOutputItem, ItemStatus,
    MessageContent, MessageItem, MessageRole, NamedToolChoice, OutputItemEvent,
    OutputTextAnnotationAddedEvent, ReasoningItem, ReasoningSummaryPart, ReasoningSummaryPartEvent,
    ReasoningSummaryPartType, ReasoningSummaryTextDeltaEvent, ReasoningSummaryTextDoneEvent,
    ReasoningTextDeltaEvent, ReasoningTextDoneEvent, RefusalDeltaEvent, RefusalDoneEvent,
    ResponseError, ResponseInput, ResponseInputItem, ResponseInstructions, ResponseItem,
    ResponseLifecycleEvent, ResponseObject, ResponseOutputItem, ResponseRequest, ResponseStatus,
    ResponseStreamEvent, ResponseUsage, TextDeltaEvent, TextDoneEvent, ToolChoice, ToolChoiceMode,
    ToolDefinition, ToolOutput,
};
