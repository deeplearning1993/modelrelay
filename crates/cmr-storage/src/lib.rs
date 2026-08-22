//! Durable local state for Codex Model Router.
//!
//! API credentials are referenced by name and stored in the operating-system
//! credential vault. Conversation items and model transitions are stored in a
//! loopback-only `SQLite` database so a request can be replayed after switching
//! providers without sending provider-private reasoning to another vendor.

mod config;
mod credentials;
mod state;

pub use config::{
    AppPaths, CompatibilityPolicy, ConfigCommitOutcome, ConfigRevision, ConfigStore, ModelConfig,
    ProviderConfig, RouterConfig, ServerConfig,
};
pub use credentials::{
    ConfigInstanceId, CredentialStore, MemoryCredentialStore, OsCredentialStore, ProviderOwnerId,
    ScopedCredentialStore, SecretRef,
};
pub use state::{
    CompactionRecord, JournaledOutputItem, ResponseRecord, ResponseStatus, StateStore,
    SwitchRecord, compaction_key,
};

/// Errors returned by persistent storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The platform-specific application directories could not be resolved.
    #[error("could not resolve application directories")]
    Directories,
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// TOML serialization or parsing failed.
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
    /// TOML serialization failed.
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    /// JSON serialization or parsing failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// `SQLite` operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The operating-system credential vault failed.
    #[error("credential vault error: {0}")]
    Credential(String),
    /// A synchronized database connection was poisoned.
    #[error("state database lock was poisoned")]
    Poisoned,
    /// The configuration is unsafe or inconsistent.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// A compare-and-swap or idempotency check detected competing state.
    #[error("storage conflict: {0}")]
    Conflict(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, StorageError>;
