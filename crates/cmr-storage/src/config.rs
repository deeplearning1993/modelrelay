use std::{
    fs,
    io::Write,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use directories::ProjectDirs;
use rusqlite::{Connection, ErrorCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ConfigInstanceId, Result, SecretRef, StorageError};

const OFFICIAL_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Platform-specific filesystem paths used by the router.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    /// Human-editable router configuration.
    pub config_file: PathBuf,
    /// `SQLite` session database.
    pub state_db: PathBuf,
}

impl AppPaths {
    /// Resolves per-user directories without touching the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Directories`] when the operating system does not
    /// expose a per-user configuration directory.
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("org", "codex-model-router", "Codex Model Router")
            .ok_or(StorageError::Directories)?;
        Ok(Self {
            config_file: dirs.config_dir().join("config.toml"),
            state_db: dirs.data_local_dir().join("state.sqlite3"),
        })
    }
}

/// Loopback HTTP service settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address. Only a loopback address is accepted.
    pub host: String,
    /// Listening port.
    pub port: u16,
    /// Maximum accepted request body size in bytes.
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 15_722,
            max_body_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Behavior when the installed Codex/ChatGPT version is not in the tested matrix.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityPolicy {
    /// Keep working but expose a prominent diagnostic warning.
    #[default]
    Warn,
    /// Make diagnostics fail while the current release has no published
    /// Desktop/mobile Remote acceptance evidence.
    Strict,
}

/// One upstream provider. Secret values never appear here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Stable provider id referenced by models.
    pub id: String,
    /// Built-in preset id or `custom-compatible`.
    pub preset: String,
    /// Optional endpoint override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Credential-vault account name, never a credential value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Whether requests may be routed to this provider.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit opt-in for a plain-HTTP endpoint on a self-hosted server.
    /// Credentials then transit unencrypted; never enable for third parties.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_insecure_http: bool,
}

/// One picker-visible model mapping.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// Stable model id exposed to Codex.
    pub id: String,
    /// Human-readable picker label.
    pub display_name: String,
    /// Provider id.
    pub provider: String,
    /// Model name sent upstream.
    pub upstream_model: String,
    /// User-controlled ordering; lower values appear first.
    #[serde(default)]
    pub order: i32,
    /// Whether the model appears in the merged catalog.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional context window override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Optional maximum output token override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// Complete non-secret router configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Config schema version.
    pub version: u32,
    /// Local listener settings.
    pub server: ServerConfig,
    /// Official `ChatGPT` Codex backend used for official models and compaction.
    pub official_base_url: String,
    /// Maximum catalog size when all official models fit. Official models are never truncated.
    pub picker_capacity: usize,
    /// Behavior for untested Codex versions.
    pub compatibility_policy: CompatibilityPolicy,
    /// Full user-defined picker order. Unlisted models follow in stable order.
    pub catalog_order: Vec<String>,
    /// Model ids hidden from the merged catalog, including official models.
    pub hidden_models: Vec<String>,
    /// Official subscription model used to obtain encrypted compaction items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_compaction_model: Option<String>,
    /// Upstream provider definitions.
    pub providers: Vec<ProviderConfig>,
    /// External model mappings. Official entries are dynamically fetched.
    pub models: Vec<ModelConfig>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            version: 1,
            server: ServerConfig::default(),
            official_base_url: OFFICIAL_CODEX_BASE_URL.into(),
            picker_capacity: 24,
            compatibility_policy: CompatibilityPolicy::Warn,
            catalog_order: Vec::new(),
            hidden_models: Vec::new(),
            official_compaction_model: None,
            providers: vec![ProviderConfig {
                id: "official".into(),
                preset: "openai-responses".into(),
                base_url: None,
                secret_ref: None,
                enabled: true,
                allow_insecure_http: false,
            }],
            models: Vec::new(),
        }
    }
}

impl RouterConfig {
    /// Checks safety and referential integrity before the service starts.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidConfig`] when a listener, endpoint,
    /// provider, model, or credential reference violates the router's safety or
    /// referential-integrity rules.
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_SCHEMA_VERSION {
            return Err(StorageError::InvalidConfig(format!(
                "unsupported configuration schema version {}; expected {CONFIG_SCHEMA_VERSION}",
                self.version
            )));
        }
        let address: IpAddr = self.server.host.parse().map_err(|_| {
            StorageError::InvalidConfig("server.host must be an IP loopback address".into())
        })?;
        if !address.is_loopback() {
            return Err(StorageError::InvalidConfig(
                "server.host must be loopback; LAN/public listeners are forbidden".into(),
            ));
        }
        if !official_base_url_is_allowed(&self.official_base_url) {
            return Err(StorageError::InvalidConfig(format!(
                "official_base_url must be exactly {OFFICIAL_CODEX_BASE_URL} (an optional trailing slash is allowed)"
            )));
        }
        if self.picker_capacity == 0 {
            return Err(StorageError::InvalidConfig(
                "picker_capacity must be greater than zero".into(),
            ));
        }
        let mut provider_ids = std::collections::HashSet::new();
        let mut official_provider_count = 0usize;
        for provider in &self.providers {
            validate_id("provider", &provider.id)?;
            if let Some(base_url) = &provider.base_url {
                validate_provider_base_url(base_url, provider.allow_insecure_http)?;
            }
            if let Some(reference) = &provider.secret_ref {
                SecretRef::parse(reference)?.validate_provider(&provider.id)?;
            }
            if !provider_ids.insert(provider.id.as_str()) {
                return Err(StorageError::InvalidConfig(format!(
                    "duplicate provider id: {}",
                    provider.id
                )));
            }
            if provider.id == "official" {
                official_provider_count += 1;
                if provider.preset != "openai-responses"
                    || provider.base_url.is_some()
                    || provider.secret_ref.is_some()
                    || !provider.enabled
                {
                    return Err(StorageError::InvalidConfig(
                        "the reserved official provider must use preset openai-responses, the built-in endpoint and ChatGPT authentication"
                            .into(),
                    ));
                }
            }
        }
        if official_provider_count != 1 {
            return Err(StorageError::InvalidConfig(
                "configuration must contain exactly one reserved official provider".into(),
            ));
        }
        let mut model_ids = std::collections::HashSet::new();
        for model in &self.models {
            validate_id("model", &model.id)?;
            if !model_ids.insert(model.id.as_str()) {
                return Err(StorageError::InvalidConfig(format!(
                    "duplicate model id: {}",
                    model.id
                )));
            }
            if !provider_ids.contains(model.provider.as_str()) {
                return Err(StorageError::InvalidConfig(format!(
                    "model {} references unknown provider {}",
                    model.id, model.provider
                )));
            }
            if model.provider == "official" {
                return Err(StorageError::InvalidConfig(format!(
                    "model {} cannot use the reserved official provider; official models are fetched dynamically",
                    model.id
                )));
            }
        }
        Ok(())
    }
}

fn validate_provider_base_url(value: &str, allow_insecure_http: bool) -> Result<()> {
    let url = url::Url::parse(value)
        .map_err(|_| StorageError::InvalidConfig("provider base_url must be a valid URL".into()))?;
    let loopback = url.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain == "localhost",
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && (loopback || allow_insecure_http)) {
        return Err(StorageError::InvalidConfig(
            "provider base_url must use HTTPS, except for loopback HTTP or an explicit allow_insecure_http opt-in"
                .into(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(StorageError::InvalidConfig(
            "provider base_url cannot contain credentials, a query string, or a fragment".into(),
        ));
    }
    Ok(())
}

fn official_base_url_is_allowed(value: &str) -> bool {
    if value == OFFICIAL_CODEX_BASE_URL || value.strip_suffix('/') == Some(OFFICIAL_CODEX_BASE_URL)
    {
        return true;
    }

    #[cfg(all(feature = "e2e-loopback-upstream", debug_assertions))]
    {
        return e2e_loopback_official_base_url(value);
    }

    #[cfg(not(all(feature = "e2e-loopback-upstream", debug_assertions)))]
    false
}

#[cfg(all(feature = "e2e-loopback-upstream", debug_assertions))]
fn e2e_loopback_official_base_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let loopback_host = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    matches!(url.scheme(), "http" | "https")
        && loopback_host
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
}

fn validate_id(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(StorageError::InvalidConfig(format!(
            "{kind} id contains unsupported characters: {value}"
        )));
    }
    Ok(())
}

const fn default_true() -> bool {
    true
}

/// Opaque content revision used for compare-and-swap configuration updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRevision(String);

impl ConfigRevision {
    fn for_bytes(bytes: Option<&[u8]>) -> Self {
        let mut digest = Sha256::new();
        match bytes {
            Some(bytes) => {
                digest.update(b"cmr-config-present-v1\0");
                digest.update(bytes);
            }
            None => digest.update(b"cmr-config-missing-v1"),
        }
        Self(format!("{:x}", digest.finalize()))
    }

    /// Returns the non-secret content digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingConfigIntent {
    protocol: u32,
    expected_revision: String,
    intended_revision: String,
    config_toml: String,
}

impl PendingConfigIntent {
    const PROTOCOL: u32 = 1;

    fn new(
        expected_revision: &ConfigRevision,
        intended_revision: &ConfigRevision,
        config_toml: String,
    ) -> Self {
        Self {
            protocol: Self::PROTOCOL,
            expected_revision: expected_revision.as_str().to_owned(),
            intended_revision: intended_revision.as_str().to_owned(),
            config_toml,
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let intent: Self = serde_json::from_slice(bytes)?;
        if intent.protocol != Self::PROTOCOL
            || !valid_revision_digest(&intent.expected_revision)
            || !valid_revision_digest(&intent.intended_revision)
        {
            return Err(StorageError::InvalidConfig(
                "pending configuration intent has invalid metadata".into(),
            ));
        }
        parse_config_bytes(intent.config_toml.as_bytes())?;
        if ConfigRevision::for_bytes(Some(intent.config_toml.as_bytes())).as_str()
            != intent.intended_revision
        {
            return Err(StorageError::InvalidConfig(
                "pending configuration intent content does not match its revision".into(),
            ));
        }
        Ok(intent)
    }
}

fn valid_revision_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Result of durably accepting a configuration update.
///
/// Receiving this value always means callers must retain any credential
/// generation referenced by the accepted configuration. Normally the live file
/// is already visible. If [`Self::requires_recovery`] is true, a unique durable
/// recovery intent is authoritative and the next load will finish publishing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigCommitOutcome {
    revision: ConfigRevision,
    live: bool,
    maintenance_warning: Option<String>,
}

impl ConfigCommitOutcome {
    fn new(revision: ConfigRevision, live: bool, warnings: &[String]) -> Self {
        Self {
            revision,
            live,
            maintenance_warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        }
    }

    /// Returns the revision of the accepted bytes.
    ///
    /// When recovery is pending, callers should load again before using this as
    /// the expected revision for another compare-and-swap update.
    #[must_use]
    pub fn revision(&self) -> &ConfigRevision {
        &self.revision
    }

    /// Returns whether the accepted bytes are already the live configuration.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.live
    }

    /// Returns whether the next load must finish a durable recovery intent.
    #[must_use]
    pub const fn requires_recovery(&self) -> bool {
        !self.live
    }

    /// Returns non-fatal post-commit maintenance diagnostics, if any.
    #[must_use]
    pub fn maintenance_warning(&self) -> Option<&str> {
        self.maintenance_warning.as_deref()
    }

    /// Consumes the outcome and returns the accepted revision.
    #[must_use]
    pub fn into_revision(self) -> ConfigRevision {
        self.revision
    }
}

/// Reads and crash-safely writes router configuration.
#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Uses an explicit path, primarily for portable installs and tests.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Uses the platform default per-user configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system does not expose a per-user
    /// configuration directory.
    pub fn discover() -> Result<Self> {
        Ok(Self::new(AppPaths::discover()?.config_file))
    }

    /// Returns the underlying configuration path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the stable vault namespace for this config path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be made absolute or normalized, or
    /// when it does not name a configuration file.
    pub fn instance_id(&self) -> Result<ConfigInstanceId> {
        ConfigInstanceId::for_path(&self.path)
    }

    /// Loads a config or returns defaults only when no live or recoverable file exists.
    ///
    /// # Errors
    ///
    /// Returns an error when locking, crash recovery, file I/O, parsing, or
    /// configuration validation fails.
    pub fn load(&self) -> Result<RouterConfig> {
        self.load_with_revision().map(|(config, _)| config)
    }

    /// Loads a config and the exact content revision used for a later CAS save.
    ///
    /// # Errors
    ///
    /// Returns an error when locking, crash recovery, file I/O, parsing, or
    /// configuration validation fails.
    pub fn load_with_revision(&self) -> Result<(RouterConfig, ConfigRevision)> {
        self.ensure_parent()?;
        let _lock = ConfigLock::acquire(&self.lock_path())?;
        self.recover_locked()?;
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let config = if let Some(bytes) = bytes.as_deref() {
            parse_config_bytes(bytes)?
        } else {
            RouterConfig::default()
        };
        Ok((config, ConfigRevision::for_bytes(bytes.as_deref())))
    }

    /// Saves with an atomic, write-through same-directory replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, locking, serialization, or durable file
    /// publication fails before the update is safely accepted.
    pub fn save(&self, config: &RouterConfig) -> Result<ConfigCommitOutcome> {
        self.save_inner(config, None, SaveFailpoint::None)
    }

    /// Saves only when the live file still has `expected_revision`.
    ///
    /// A sibling cross-process lock closes the compare/replace race. A mismatch
    /// returns [`StorageError::Conflict`] without changing either file.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Conflict`] when `expected_revision` is stale, or
    /// another storage error when validation, locking, serialization, or durable
    /// file publication fails before the update is safely accepted.
    pub fn save_if_revision(
        &self,
        config: &RouterConfig,
        expected_revision: &ConfigRevision,
    ) -> Result<ConfigCommitOutcome> {
        self.save_inner(config, Some(expected_revision), SaveFailpoint::None)
    }

    fn save_inner(
        &self,
        config: &RouterConfig,
        expected_revision: Option<&ConfigRevision>,
        failpoint: SaveFailpoint,
    ) -> Result<ConfigCommitOutcome> {
        config.validate()?;
        self.ensure_parent()?;
        let _lock = ConfigLock::acquire(&self.lock_path())?;
        self.recover_locked()?;
        let current = match fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(expected) = expected_revision {
            let actual = ConfigRevision::for_bytes(current.as_deref());
            if &actual != expected {
                return Err(StorageError::Conflict(
                    "configuration changed since it was loaded".into(),
                ));
            }
        }

        let encoded = toml::to_string_pretty(config)?;
        let current_revision = ConfigRevision::for_bytes(current.as_deref());
        let intended_revision = ConfigRevision::for_bytes(Some(encoded.as_bytes()));
        let intent =
            PendingConfigIntent::new(&current_revision, &intended_revision, encoded.clone());
        let intent_bytes = serde_json::to_vec(&intent)?;
        let pending = self.new_pending_path()?;
        let mut warnings = Vec::new();
        if let Some(warning) = atomic_replace(&pending, &intent_bytes, false)? {
            warnings.push(format!("pending configuration: {warning}"));
        }
        if failpoint == SaveFailpoint::AfterPending {
            warnings
                .push("live configuration publication was deferred after durable intent".into());
            return Ok(ConfigCommitOutcome::new(
                intended_revision,
                false,
                &warnings,
            ));
        }

        // Publishing the unique pending intent is the deterministic commit
        // point. Any later live-file failure is an accepted recovery-pending
        // outcome, never `Err`, so callers cannot delete its staged credential.
        let live_result = if failpoint == SaveFailpoint::LiveReplaceFailure {
            Err(StorageError::Io(std::io::Error::other(
                "injected live configuration replacement failure",
            )))
        } else {
            atomic_replace(
                &self.path,
                encoded.as_bytes(),
                failpoint == SaveFailpoint::LiveSyncFailure,
            )
        };
        match live_result {
            Ok(Some(warning)) => warnings.push(format!("live configuration: {warning}")),
            Ok(None) => {}
            Err(error) => {
                warnings.push(format!(
                    "live configuration publication deferred to recovery: {error}"
                ));
                return Ok(ConfigCommitOutcome::new(
                    intended_revision,
                    false,
                    &warnings,
                ));
            }
        }

        let cleanup = if failpoint == SaveFailpoint::PendingCleanupFailure {
            Err(StorageError::Io(std::io::Error::other(
                "injected pending cleanup failure",
            )))
        } else {
            remove_if_exists(&pending)
        };
        if let Err(error) = cleanup {
            warnings.push(format!("pending configuration cleanup failed: {error}"));
        } else {
            let cleanup_sync = if failpoint == SaveFailpoint::CleanupSyncFailure {
                Err(StorageError::Io(std::io::Error::other(
                    "injected cleanup directory sync failure",
                )))
            } else {
                sync_parent(&self.path)
            };
            if let Err(error) = cleanup_sync {
                warnings.push(format!(
                    "pending configuration cleanup directory sync failed: {error}"
                ));
            }
        }
        Ok(ConfigCommitOutcome::new(intended_revision, true, &warnings))
    }

    fn ensure_parent(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn pending_path(&self) -> PathBuf {
        let extension = self.path.extension().map_or_else(
            || "pending".into(),
            |value| format!("{}.pending", value.to_string_lossy()),
        );
        self.path.with_extension(extension)
    }

    fn new_pending_path(&self) -> Result<PathBuf> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = self
            .path
            .file_name()
            .ok_or_else(|| StorageError::InvalidConfig("config path has no file name".into()))?;
        Ok(parent.join(format!(
            "{}.pending.{}",
            file_name.to_string_lossy(),
            uuid::Uuid::new_v4()
        )))
    }

    fn generation_pending_paths(&self) -> Result<Vec<PathBuf>> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let Some(file_name) = self.path.file_name().and_then(|value| value.to_str()) else {
            return Ok(Vec::new());
        };
        let prefix = format!("{file_name}.pending.");
        let mut paths = Vec::new();
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(generation) = name.strip_prefix(&prefix) else {
                continue;
            };
            if uuid::Uuid::parse_str(generation).is_ok() {
                paths.push(entry.path());
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn lock_path(&self) -> PathBuf {
        let extension = self.path.extension().map_or_else(
            || "lock.sqlite3".into(),
            |value| format!("{}.lock.sqlite3", value.to_string_lossy()),
        );
        self.path.with_extension(extension)
    }

    fn recover_locked(&self) -> Result<()> {
        let live = match fs::read(&self.path) {
            Ok(bytes) => {
                parse_config_bytes(&bytes)?;
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        // New writes use unique generation intents containing their expected and
        // intended revisions. Only an intent whose expected revision is live may
        // advance the config. Applied or superseded residue can therefore never
        // roll a later save backward even when Windows cannot delete it.
        let generation_paths = self.generation_pending_paths()?;
        let live_revision = ConfigRevision::for_bytes(live.as_deref());
        let mut distinct_intents = Vec::<(String, Vec<u8>)>::new();
        for path in &generation_paths {
            let bytes = fs::read(path)?;
            let intent = PendingConfigIntent::parse(&bytes)?;
            if intent.expected_revision == live_revision.as_str()
                && intent.intended_revision != live_revision.as_str()
                && !distinct_intents
                    .iter()
                    .any(|(revision, _)| revision == &intent.intended_revision)
            {
                distinct_intents.push((intent.intended_revision, intent.config_toml.into_bytes()));
            }
        }
        if distinct_intents.len() > 1 {
            return Err(StorageError::InvalidConfig(
                "multiple distinct pending configuration generations exist; refusing to guess commit order"
                    .into(),
            ));
        }
        if let Some((_, config_bytes)) = distinct_intents.first() {
            let _ = atomic_replace(&self.path, config_bytes, false)?;
        }
        for path in &generation_paths {
            let _ = remove_if_exists(path);
        }
        if !distinct_intents.is_empty() {
            return Ok(());
        }

        // The fixed legacy pending name predates deterministic outcomes. If a
        // valid live file exists it must never be promoted: an older caller may
        // have received `Err` and deleted the staged credential. Non-file objects
        // at this legacy path are ignored so they cannot permanently block saves.
        let legacy_pending = self.pending_path();
        let legacy_pending_is_file = fs::metadata(&legacy_pending).is_ok_and(|meta| meta.is_file());
        if live.is_some() {
            if legacy_pending_is_file {
                let _ = remove_if_exists(&legacy_pending);
            }
            return Ok(());
        }
        if legacy_pending_is_file {
            let bytes = fs::read(&legacy_pending)?;
            parse_config_bytes(&bytes)?;
            let _ = atomic_replace(&self.path, &bytes, false)?;
            let _ = remove_if_exists(&legacy_pending);
            return Ok(());
        }

        let backups = self.legacy_backups()?;
        match backups.as_slice() {
            [] => Ok(()),
            [backup] => {
                let bytes = fs::read(backup)?;
                parse_config_bytes(&bytes)?;
                let _ = atomic_replace(&self.path, &bytes, false)?;
                let _ = remove_if_exists(backup);
                let _ = sync_parent(&self.path);
                Ok(())
            }
            _ => Err(StorageError::InvalidConfig(
                "multiple recovery backups exist; refusing to guess which config is authoritative"
                    .into(),
            )),
        }
    }

    fn legacy_backups(&self) -> Result<Vec<PathBuf>> {
        let Some(parent) = self.path.parent() else {
            return Ok(Vec::new());
        };
        let Some(file_name) = self.path.file_name().and_then(|value| value.to_str()) else {
            return Ok(Vec::new());
        };
        let prefix = format!("{file_name}.replace-backup.");
        let mut backups = Vec::new();
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                backups.push(entry.path());
            }
        }
        backups.sort();
        Ok(backups)
    }
}

fn parse_config_bytes(bytes: &[u8]) -> Result<RouterConfig> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        StorageError::InvalidConfig(format!("configuration is not UTF-8: {error}"))
    })?;
    let config: RouterConfig = toml::from_str(text)?;
    config.validate()?;
    Ok(config)
}

fn atomic_replace(path: &Path, bytes: &[u8], inject_sync_failure: bool) -> Result<Option<String>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| StorageError::Io(error.error))?;
    let sync_result = if inject_sync_failure {
        Err(StorageError::Io(std::io::Error::other(
            "injected directory sync failure after atomic publication",
        )))
    } else {
        sync_parent(path)
    };
    Ok(sync_result.err().map(|error| error.to_string()))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(path: &Path) -> Result<()> {
    // tempfile uses MoveFileExW with WRITE_THROUGH on Windows. Opening a
    // directory for sync through std is unsupported there. Still surface a
    // filesystem error if the just-published path cannot be queried.
    let _ = fs::metadata(path)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SaveFailpoint {
    None,
    AfterPending,
    LiveReplaceFailure,
    LiveSyncFailure,
    PendingCleanupFailure,
    CleanupSyncFailure,
}

struct ConfigLock {
    _connection: Connection,
}

impl ConfigLock {
    fn acquire(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::ZERO)?;
        match connection.execute_batch("PRAGMA locking_mode=NORMAL; BEGIN EXCLUSIVE;") {
            Ok(()) => Ok(Self {
                _connection: connection,
            }),
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) =>
            {
                Err(StorageError::Conflict(
                    "another process is updating the configuration".into(),
                ))
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_external_provider() -> RouterConfig {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: Some("zhipu/default".into()),
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-5.2".into(),
            display_name: "GLM-5.2".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.2".into(),
            order: 10,
            enabled: true,
            context_window: Some(1_000_000),
            max_output_tokens: Some(131_072),
        });
        config
    }

    #[test]
    fn rejects_non_loopback_listener() {
        let mut config = RouterConfig::default();
        config.server.host = "0.0.0.0".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unknown_schema_versions() {
        for version in [0, CONFIG_SCHEMA_VERSION + 1, u32::MAX] {
            let config = RouterConfig {
                version,
                ..RouterConfig::default()
            };
            assert!(config.validate().is_err(), "accepted schema {version}");
        }
        assert!(RouterConfig::default().validate().is_ok());
    }

    #[test]
    fn accepts_only_the_canonical_official_codex_base_url() {
        for allowed in [
            "https://chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/codex/",
        ] {
            let config = RouterConfig {
                official_base_url: allowed.into(),
                ..RouterConfig::default()
            };
            assert!(
                config.validate().is_ok(),
                "expected {allowed} to be allowed"
            );
        }
    }

    #[test]
    fn rejects_noncanonical_official_codex_base_urls() {
        for rejected in [
            "http://chatgpt.com/backend-api/codex",
            "https://example.com/backend-api/codex",
            "https://chatgpt.com:443/backend-api/codex",
            "https://user@chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/codex?target=external",
            "https://chatgpt.com/backend-api/codex#fragment",
            "https://chatgpt.com/backend-api/codex//",
            "https://chatgpt.com/backend-api/codex/models",
        ] {
            let config = RouterConfig {
                official_base_url: rejected.into(),
                ..RouterConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "expected {rejected} to be rejected"
            );
        }
    }

    #[test]
    fn provider_overrides_reject_embedded_credentials_query_and_fragment() {
        for rejected in [
            "https://user@example.com/v1",
            "https://user:password@example.com/v1",
            "https://example.com/v1?api_key=plaintext",
            "https://example.com/v1#credential",
        ] {
            let mut config = RouterConfig::default();
            config.providers.push(ProviderConfig {
                id: "custom".into(),
                preset: "openai-chat-compatible".into(),
                base_url: Some(rejected.into()),
                secret_ref: Some("custom/default".into()),
                enabled: true,
                allow_insecure_http: false,
            });
            assert!(
                config.validate().is_err(),
                "expected provider override {rejected} to be rejected"
            );
        }
    }

    #[test]
    fn plain_http_remote_endpoint_requires_the_explicit_opt_in() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "selfhost".into(),
            preset: "openai-chat-compatible".into(),
            base_url: Some("http://203.0.113.10:7000/v1".into()),
            secret_ref: Some("selfhost/default".into()),
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "self-model".into(),
            display_name: "Self Model".into(),
            provider: "selfhost".into(),
            upstream_model: "self-model".into(),
            order: 0,
            enabled: true,
            context_window: None,
            max_output_tokens: None,
        });
        assert!(
            config.validate().is_err(),
            "plain HTTP stays rejected by default"
        );
        config.providers[1].allow_insecure_http = true;
        config
            .validate()
            .expect("self-hosted plain HTTP is allowed with the explicit opt-in");
    }

    #[test]
    fn reserved_official_provider_cannot_be_reconfigured_or_reused_by_models() {
        let mut missing = RouterConfig::default();
        missing.providers.clear();
        assert!(missing.validate().is_err());

        let mut wrong_preset = RouterConfig::default();
        wrong_preset.providers[0].preset = "custom-compatible".into();
        assert!(wrong_preset.validate().is_err());

        let mut overridden = RouterConfig::default();
        overridden.providers[0].base_url = Some("https://example.com/v1".into());
        assert!(overridden.validate().is_err());

        let mut credentialed = RouterConfig::default();
        credentialed.providers[0].secret_ref = Some("official/forbidden".into());
        assert!(credentialed.validate().is_err());

        let mut disabled = RouterConfig::default();
        disabled.providers[0].enabled = false;
        assert!(disabled.validate().is_err());

        let mut model_reuse = RouterConfig::default();
        model_reuse.models.push(ModelConfig {
            id: "shadow-official".into(),
            display_name: "Shadow official".into(),
            provider: "official".into(),
            upstream_model: "external-model".into(),
            order: 0,
            enabled: true,
            context_window: None,
            max_output_tokens: None,
        });
        assert!(model_reuse.validate().is_err());
    }

    #[cfg(all(feature = "e2e-loopback-upstream", debug_assertions))]
    #[test]
    fn e2e_feature_accepts_only_explicit_loopback_origins() {
        for allowed in ["http://127.0.0.1:43123", "https://[::1]:43123/"] {
            let mut config = RouterConfig::default();
            config.official_base_url = allowed.into();
            assert!(
                config.validate().is_ok(),
                "expected {allowed} to be allowed"
            );
        }
        for rejected in [
            "http://127.0.0.1",
            "http://127.0.0.1:43123/models",
            "http://127.0.0.1:43123?redirect=1",
            "http://localhost:43123",
            "http://192.168.1.2:43123",
        ] {
            let mut config = RouterConfig::default();
            config.official_base_url = rejected.into();
            assert!(
                config.validate().is_err(),
                "expected {rejected} to be rejected"
            );
        }
    }

    #[test]
    fn round_trips_without_secret_values() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let config = config_with_external_provider();
        store.save(&config).expect("save");
        assert_eq!(store.load().expect("load"), config);
    }

    #[test]
    fn consecutive_saves_replace_existing_file_on_windows() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let first = RouterConfig::default();
        store.save(&first).expect("first save");

        let mut second = first;
        second.server.port = 15_723;
        second.catalog_order = vec!["glm-5.2".into(), "gpt-official".into()];
        store.save(&second).expect("replace existing config");

        assert_eq!(store.load().expect("load second config"), second);
        let files = fs::read_dir(directory.path())
            .expect("read temp dir")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            files.len(),
            2,
            "only the config and persistent SQLite-backed lock file may remain: {files:?}"
        );
        assert!(!store.pending_path().exists());
    }

    #[test]
    fn secret_references_are_strict_and_provider_bound() {
        for reference in [
            "other/default",
            "zhipu/not_a_real_credential_000000000000",
            "zhipu/0123456789abcdef0123456789abcdef",
            "zhipu/default/g/not-a-uuid",
            "zhipu/default/extra",
        ] {
            let mut config = config_with_external_provider();
            config.providers[1].secret_ref = Some(reference.into());
            assert!(
                config.validate().is_err(),
                "expected secret reference {reference} to be rejected"
            );
        }
    }

    #[test]
    fn compare_and_swap_rejects_a_stale_revision() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let (_, initial_revision) = store.load_with_revision().expect("initial load");

        let mut winner = RouterConfig::default();
        winner.server.port = 15_723;
        let winner_revision = store
            .save_if_revision(&winner, &initial_revision)
            .expect("first CAS save")
            .into_revision();
        assert_ne!(winner_revision, initial_revision);

        let mut stale = RouterConfig::default();
        stale.server.port = 15_724;
        assert!(matches!(
            store.save_if_revision(&stale, &initial_revision),
            Err(StorageError::Conflict(_))
        ));
        assert_eq!(store.load().expect("winner remains live"), winner);
    }

    #[test]
    fn recovers_pending_config_when_no_live_file_exists() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut expected = RouterConfig::default();
        expected.server.port = 15_723;

        let outcome = store
            .save_inner(&expected, None, SaveFailpoint::AfterPending)
            .expect("durable intent outcome");
        assert!(outcome.requires_recovery());
        assert!(!store.path().exists());
        assert_eq!(store.generation_pending_paths().expect("pending").len(), 1);

        assert_eq!(store.load().expect("recover pending"), expected);
        assert!(store.path().exists());
        assert!(
            store
                .generation_pending_paths()
                .expect("pending")
                .is_empty()
        );
    }

    #[test]
    fn committed_live_file_wins_over_an_ambiguous_legacy_pending_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut committed = RouterConfig::default();
        committed.server.port = 15_723;
        store.save(&committed).expect("committed config");

        let mut uncommitted = committed.clone();
        uncommitted.server.port = 15_724;
        fs::write(
            store.pending_path(),
            toml::to_string_pretty(&uncommitted).expect("serialize legacy pending"),
        )
        .expect("write legacy pending");

        assert_eq!(store.load().expect("load committed"), committed);
        assert!(!store.pending_path().exists());
    }

    #[test]
    fn unique_durable_intent_is_recovered_over_an_older_live_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut committed = RouterConfig::default();
        committed.server.port = 15_723;
        store.save(&committed).expect("committed config");

        let mut accepted = committed.clone();
        accepted.server.port = 15_724;
        let outcome = store
            .save_inner(&accepted, None, SaveFailpoint::LiveReplaceFailure)
            .expect("accepted recovery intent");
        assert!(outcome.requires_recovery());
        assert_eq!(
            parse_config_bytes(&fs::read(store.path()).expect("old live")).expect("parse old live"),
            committed
        );

        assert_eq!(store.load().expect("promote accepted intent"), accepted);
        assert!(
            store
                .generation_pending_paths()
                .expect("pending")
                .is_empty()
        );
    }

    #[test]
    fn superseded_stale_intent_never_rolls_back_a_later_save() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut first = RouterConfig::default();
        first.server.port = 15_723;
        store
            .save_inner(&first, None, SaveFailpoint::PendingCleanupFailure)
            .expect("first live commit");
        let stale_path = store
            .generation_pending_paths()
            .expect("first pending")
            .pop()
            .expect("stale intent");
        let stale_bytes = fs::read(&stale_path).expect("capture stale intent");

        let mut second = first;
        second.server.port = 15_724;
        store.save(&second).expect("later save");
        fs::write(&stale_path, stale_bytes).expect("simulate undeletable stale residue");

        assert_eq!(store.load().expect("load later config"), second);
        assert_eq!(
            parse_config_bytes(&fs::read(store.path()).expect("live bytes")).expect("live config"),
            second
        );
    }

    #[test]
    fn postcommit_cleanup_failure_is_an_outcome_and_live_remains_authoritative() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut expected = RouterConfig::default();
        expected.server.port = 15_723;

        let outcome = store
            .save_inner(&expected, None, SaveFailpoint::PendingCleanupFailure)
            .expect("live commit is successful despite cleanup warning");
        assert!(outcome.maintenance_warning().is_some());
        assert!(outcome.is_live());
        assert!(store.path().exists());
        assert_eq!(store.generation_pending_paths().expect("pending").len(), 1);

        assert_eq!(store.load().expect("recover live"), expected);
        assert!(
            store
                .generation_pending_paths()
                .expect("pending")
                .is_empty()
        );
    }

    #[test]
    fn postcommit_sync_failure_is_an_outcome_not_a_rollback_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut expected = RouterConfig::default();
        expected.server.port = 15_723;

        let outcome = store
            .save_inner(&expected, None, SaveFailpoint::LiveSyncFailure)
            .expect("published live config must not return Err");
        assert!(
            outcome
                .maintenance_warning()
                .is_some_and(|warning| warning.contains("live configuration"))
        );
        assert_eq!(store.load().expect("committed config"), expected);
    }

    #[test]
    fn postcleanup_sync_failure_is_an_outcome_not_a_rollback_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let expected = RouterConfig::default();

        let outcome = store
            .save_inner(&expected, None, SaveFailpoint::CleanupSyncFailure)
            .expect("published live config must not return Err");
        assert!(outcome.maintenance_warning().is_some());
        assert_eq!(store.load().expect("committed config"), expected);
    }

    #[test]
    fn valid_live_config_load_ignores_pending_cleanup_failure() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let expected = RouterConfig::default();
        store.save(&expected).expect("save live");
        fs::create_dir(store.pending_path()).expect("unremovable pending directory");

        assert_eq!(store.load().expect("valid live must load"), expected);
        assert!(store.pending_path().is_dir());
    }

    #[test]
    fn blocked_legacy_pending_object_does_not_prevent_consecutive_saves() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        fs::create_dir(store.pending_path()).expect("blocked legacy pending object");

        let first = RouterConfig::default();
        assert!(store.save(&first).expect("first save").is_live());
        let mut second = first;
        second.server.port = 15_723;
        assert!(store.save(&second).expect("second save").is_live());
        assert_eq!(store.load().expect("latest config"), second);
        assert!(store.pending_path().is_dir());
        assert!(
            store
                .generation_pending_paths()
                .expect("pending")
                .is_empty()
        );
    }

    #[test]
    fn invalid_recovery_file_is_never_silently_replaced_with_defaults() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        fs::write(store.pending_path(), b"this is not valid toml").expect("write pending");

        assert!(store.load().is_err());
        assert!(!store.path().exists());
        assert!(store.pending_path().exists());
    }

    #[test]
    fn restores_a_single_legacy_replace_backup() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let mut expected = RouterConfig::default();
        expected.server.port = 15_723;
        let backup = directory.path().join("config.toml.replace-backup.test");
        fs::write(
            &backup,
            toml::to_string_pretty(&expected).expect("serialize backup"),
        )
        .expect("write backup");

        assert_eq!(store.load().expect("recover backup"), expected);
        assert!(store.path().exists());
        assert!(!backup.exists());
    }

    #[test]
    fn an_active_cross_process_lock_blocks_updates() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ConfigStore::new(directory.path().join("config.toml"));
        let active = ConfigLock::acquire(&store.lock_path()).expect("first OS lock");

        assert!(matches!(
            store.save(&RouterConfig::default()),
            Err(StorageError::Conflict(_))
        ));
        drop(active);
        ConfigLock::acquire(&store.lock_path()).expect("OS releases lock on close");
    }

    #[test]
    fn config_instance_id_is_stable_and_path_scoped() {
        let directory = tempfile::tempdir().expect("temp dir");
        let first = ConfigStore::new(directory.path().join("a").join("config.toml"));
        let same = ConfigStore::new(directory.path().join("a").join("config.toml"));
        let second = ConfigStore::new(directory.path().join("b").join("config.toml"));

        assert_eq!(
            first.instance_id().expect("first instance"),
            same.instance_id().expect("same instance")
        );
        assert_ne!(
            first.instance_id().expect("first instance"),
            second.instance_id().expect("second instance")
        );
    }
}
