use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::Write,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

const MODEL_PROVIDER: &str = "openai";

/// Paths and operations for the user-level Codex config integration.
#[derive(Clone, Debug)]
pub struct CodexIntegration {
    config_path: PathBuf,
    sidecar_path: PathBuf,
    lock_path: PathBuf,
    installed: InstalledValues,
}

/// Current state of the three managed Codex values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationStatus {
    /// No sidecar exists, so this installation does not claim ownership of any keys.
    NotInstalled,
    /// The sidecar and all managed values match.
    Installed,
    /// A sidecar exists, but one or more managed values were changed or removed.
    Drifted,
}

/// Result of restoring managed values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UninstallReport {
    /// Number of installed values safely restored from the backup.
    pub restored: usize,
    /// Number of managed values left alone because the user changed them after install.
    pub preserved_user_changes: usize,
    /// Backup retained for manual recovery.
    pub backup_path: Option<PathBuf>,
}

/// Result of an unconditional restore to the pre-install Codex config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreReport {
    /// Whether a recorded integration was found and reverted.
    pub restored: bool,
    /// Backup retained for manual recovery.
    pub backup_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallState {
    schema_version: u32,
    config_path: PathBuf,
    backup_path: PathBuf,
    config_existed: bool,
    installed: InstalledValues,
    originals: OriginalValues,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledValues {
    model_provider: String,
    openai_base_url: String,
    remote_control: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginalValues {
    model_provider: SavedItem,
    openai_base_url: SavedItem,
    remote_control: SavedItem,
    features_present: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedItem {
    changed_by_install: bool,
    /// A self-contained TOML document with the original item stored under `value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    item_toml: Option<String>,
}

impl InstalledValues {
    fn new(openai_base_url: String) -> Self {
        Self {
            model_provider: MODEL_PROVIDER.to_owned(),
            openai_base_url,
            remote_control: true,
        }
    }
}

impl CodexIntegration {
    /// Creates an integration for the current user's standard Codex config.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system does not expose a current
    /// user home directory or when the resulting managed paths are unsafe.
    pub fn for_current_user(router_host: &str, router_port: u16) -> Result<Self> {
        let user_dirs = UserDirs::new().context("resolve the current user's home directory")?;
        Self::new(
            user_dirs.home_dir().join(".codex").join("config.toml"),
            None,
            router_host,
            router_port,
        )
    }

    /// Creates an integration using explicit, injectable paths.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint or managed filesystem paths are unsafe.
    pub fn new(
        config_path: impl Into<PathBuf>,
        sidecar_path: Option<PathBuf>,
        router_host: &str,
        router_port: u16,
    ) -> Result<Self> {
        let config_path = absolute(config_path.into())?;
        let sidecar_path =
            absolute(sidecar_path.unwrap_or_else(|| default_sidecar_path(&config_path)))?;
        let config_name = config_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Codex config path has no UTF-8 filename")?;
        let lock_path = config_path.with_file_name(format!("{config_name}.cmr.lock"));
        validate_distinct_managed_paths(&config_path, &sidecar_path, &lock_path)?;
        let openai_base_url = router_base_url(router_host, router_port)?;
        Ok(Self {
            config_path,
            sidecar_path,
            lock_path,
            installed: InstalledValues::new(openai_base_url),
        })
    }

    /// Returns the Codex config path.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Returns the state sidecar path.
    #[must_use]
    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    /// Backs up and merges only the three user-level settings required by the router.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, parsing, backup, or atomic writes fail.
    pub fn install(&self) -> Result<PathBuf> {
        self.create_parent_directories()?;
        let _lock = OperationLock::acquire(&self.lock_path)?;
        self.validate_managed_paths()?;

        let config_existed = self.config_path.exists();
        let original = if config_existed {
            fs::read(&self.config_path)
                .with_context(|| format!("read Codex config {}", self.config_path.display()))?
        } else {
            Vec::new()
        };
        let source = std::str::from_utf8(&original).context("Codex config must be UTF-8 TOML")?;
        let mut document = parse_document(source, &self.config_path)?;
        ensure_features_table(&document)?;

        let previous_sidecar = read_optional(&self.sidecar_path)?;
        let previous = if let Some(bytes) = previous_sidecar.as_deref() {
            let state = Self::parse_state(bytes)?;
            self.validate_state_paths(&state)?;
            Some(state)
        } else {
            None
        };
        let originals = self.capture_originals(&document, previous.as_ref());

        let backup_path = self.create_backup(&original)?;
        document["model_provider"] = value(self.installed.model_provider.clone());
        document["openai_base_url"] = value(self.installed.openai_base_url.clone());
        set_remote_control(&mut document, self.installed.remote_control)?;

        let state = InstallState {
            schema_version: 1,
            config_path: self.config_path.clone(),
            backup_path: backup_path.clone(),
            config_existed,
            installed: self.installed.clone(),
            originals,
        };
        let state_bytes =
            serde_json::to_vec_pretty(&state).context("serialize Codex integration state")?;
        let config_bytes = document.to_string().into_bytes();
        let sidecar_permissions = metadata_if_exists(&self.sidecar_path)?;
        let config_permissions = metadata_if_exists(&self.config_path)?;

        // Write recovery metadata first. If the following config write fails, an
        // uninstall remains safe because it only restores exact installed values.
        atomic_write_cas(
            &self.sidecar_path,
            previous_sidecar.as_deref(),
            &state_bytes,
            sidecar_permissions.as_ref(),
        )?;
        if let Err(config_error) = atomic_write_cas(
            &self.config_path,
            config_existed.then_some(original.as_slice()),
            &config_bytes,
            config_permissions.as_ref(),
        ) {
            let rollback = match previous_sidecar.as_deref() {
                Some(previous) => atomic_write_cas(
                    &self.sidecar_path,
                    Some(&state_bytes),
                    previous,
                    sidecar_permissions.as_ref(),
                ),
                None => remove_file_cas(&self.sidecar_path, &state_bytes),
            };
            if let Err(rollback_error) = rollback {
                bail!(
                    "Codex config update failed ({config_error:#}); sidecar rollback also failed ({rollback_error:#}); recovery backup is {}",
                    backup_path.display()
                );
            }
            return Err(config_error).context(format!(
                "Codex config update rolled back; recovery backup is {}",
                backup_path.display()
            ));
        }

        Ok(backup_path)
    }

    fn capture_originals(
        &self,
        document: &DocumentMut,
        previous: Option<&InstallState>,
    ) -> OriginalValues {
        let model_provider_current = document.get("model_provider").and_then(Item::as_str);
        let base_url_current = document.get("openai_base_url").and_then(Item::as_str);
        let remote_current = remote_control(document).and_then(Item::as_bool);
        let model_provider_matches = model_provider_current
            == Some(self.installed.model_provider.as_str())
            || previous.is_some_and(|state| {
                model_provider_current == Some(state.installed.model_provider.as_str())
            });
        let base_url_matches = base_url_current == Some(self.installed.openai_base_url.as_str())
            || previous.is_some_and(|state| {
                base_url_current == Some(state.installed.openai_base_url.as_str())
            });
        let remote_matches = remote_current == Some(self.installed.remote_control)
            || previous.is_some_and(|state| remote_current == Some(state.installed.remote_control));
        OriginalValues {
            model_provider: select_original(
                document.get("model_provider"),
                previous.map(|state| &state.originals.model_provider),
                model_provider_matches,
            ),
            openai_base_url: select_original(
                document.get("openai_base_url"),
                previous.map(|state| &state.originals.openai_base_url),
                base_url_matches,
            ),
            remote_control: select_original(
                remote_control(document),
                previous.map(|state| &state.originals.remote_control),
                remote_matches,
            ),
            features_present: if remote_matches {
                previous.map_or_else(
                    || document.get("features").is_some(),
                    |state| state.originals.features_present,
                )
            } else {
                document.get("features").is_some()
            },
        }
    }

    /// Restores only values that still exactly match what this tool installed.
    ///
    /// # Errors
    ///
    /// Returns an error when recovery metadata is invalid or restoration fails.
    pub fn uninstall(&self) -> Result<UninstallReport> {
        self.create_parent_directories()?;
        let _lock = OperationLock::acquire(&self.lock_path)?;
        self.validate_managed_paths()?;
        let Some(sidecar_bytes) = read_optional(&self.sidecar_path)? else {
            return Ok(UninstallReport {
                restored: 0,
                preserved_user_changes: 0,
                backup_path: None,
            });
        };

        let state = Self::parse_state(&sidecar_bytes)?;
        self.validate_state_paths(&state)?;
        let current_bytes = read_optional(&self.config_path)?;
        let current_source = std::str::from_utf8(current_bytes.as_deref().unwrap_or_default())
            .context("Codex config must be UTF-8 TOML")?;
        let mut current = parse_document(current_source, &self.config_path)?;

        let mut restored = 0;
        let mut preserved_user_changes = 0;
        restore_root_string(
            &mut current,
            "model_provider",
            &state.installed.model_provider,
            &state.originals.model_provider,
            &mut restored,
            &mut preserved_user_changes,
        )?;
        restore_root_string(
            &mut current,
            "openai_base_url",
            &state.installed.openai_base_url,
            &state.originals.openai_base_url,
            &mut restored,
            &mut preserved_user_changes,
        )?;
        restore_remote_control(
            &mut current,
            state.installed.remote_control,
            &state.originals.remote_control,
            state.originals.features_present,
            &mut restored,
            &mut preserved_user_changes,
        )?;

        let config_permissions = metadata_if_exists(&self.config_path)?;
        atomic_write_cas(
            &self.config_path,
            current_bytes.as_deref(),
            current.to_string().as_bytes(),
            config_permissions.as_ref(),
        )?;
        remove_file_cas(&self.sidecar_path, &sidecar_bytes)?;

        Ok(UninstallReport {
            restored,
            preserved_user_changes,
            backup_path: Some(state.backup_path),
        })
    }

    /// Restores the Codex config to its exact pre-install state.
    ///
    /// Unlike [`CodexIntegration::uninstall`], this performs an unconditional
    /// byte-for-byte restoration from the recovery backup captured at install
    /// time. Values the user changed after install are overwritten, and the
    /// whole file is reverted to the original snapshot (so unrelated keys added
    /// after install are removed as well). When the config did not exist before
    /// install, the current config is deleted to reproduce that default state.
    /// The integration sidecar is removed and the recovery backup is retained.
    ///
    /// # Errors
    ///
    /// Returns an error when recovery metadata is invalid, the backup cannot be
    /// read, or restoration fails.
    pub fn restore(&self) -> Result<RestoreReport> {
        self.create_parent_directories()?;
        let _lock = OperationLock::acquire(&self.lock_path)?;
        self.validate_managed_paths()?;
        let Some(sidecar_bytes) = read_optional(&self.sidecar_path)? else {
            return Ok(RestoreReport {
                restored: false,
                backup_path: None,
            });
        };

        let state = Self::parse_state(&sidecar_bytes)?;
        self.validate_state_paths(&state)?;
        let backup_path = absolute(state.backup_path.clone())?;
        let backup_bytes = fs::read(&backup_path)
            .with_context(|| format!("read Codex recovery backup {}", backup_path.display()))?;

        let current_bytes = read_optional(&self.config_path)?;
        let config_permissions = metadata_if_exists(&self.config_path)?;

        if state.config_existed {
            // The backup is the exact pre-install file contents. Replay it
            // verbatim so even unrelated post-install edits are reverted.
            atomic_write_cas(
                &self.config_path,
                current_bytes.as_deref(),
                &backup_bytes,
                config_permissions.as_ref(),
            )?;
        } else {
            // The config did not exist before install: reproduce that default by
            // removing the file. An empty backup is the sentinel for this case.
            remove_file_cas(
                &self.config_path,
                current_bytes.as_deref().unwrap_or_default(),
            )?;
        }
        remove_file_cas(&self.sidecar_path, &sidecar_bytes)?;

        Ok(RestoreReport {
            restored: true,
            backup_path: Some(backup_path),
        })
    }

    /// Inspects the sidecar and all three managed values without changing files.
    ///
    /// # Errors
    ///
    /// Returns an error when recovery metadata or the Codex TOML cannot be validated.
    pub fn status(&self) -> Result<IntegrationStatus> {
        self.validate_managed_paths()?;
        if !self.sidecar_path.exists() {
            return Ok(IntegrationStatus::NotInstalled);
        }
        let state = self.read_state()?;
        self.validate_state_paths(&state)?;
        if !self.config_path.exists() {
            return Ok(IntegrationStatus::Drifted);
        }
        let source = fs::read_to_string(&self.config_path)
            .with_context(|| format!("read Codex config {}", self.config_path.display()))?;
        let document = parse_document(&source, &self.config_path)?;
        let matches = state.installed == self.installed
            && document.get("model_provider").and_then(Item::as_str)
                == Some(state.installed.model_provider.as_str())
            && document.get("openai_base_url").and_then(Item::as_str)
                == Some(state.installed.openai_base_url.as_str())
            && remote_control(&document).and_then(Item::as_bool)
                == Some(state.installed.remote_control);
        Ok(if matches {
            IntegrationStatus::Installed
        } else {
            IntegrationStatus::Drifted
        })
    }

    fn read_state(&self) -> Result<InstallState> {
        let bytes = fs::read(&self.sidecar_path)
            .with_context(|| format!("read {}", self.sidecar_path.display()))?;
        Self::parse_state(&bytes)
    }

    fn parse_state(bytes: &[u8]) -> Result<InstallState> {
        let state: InstallState =
            serde_json::from_slice(bytes).context("parse Codex integration state")?;
        if state.schema_version != 1 {
            bail!("unsupported Codex integration state schema");
        }
        Ok(state)
    }

    fn create_parent_directories(&self) -> Result<()> {
        for path in [&self.config_path, &self.sidecar_path] {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        Ok(())
    }

    fn validate_managed_paths(&self) -> Result<()> {
        validate_distinct_managed_paths(&self.config_path, &self.sidecar_path, &self.lock_path)
    }

    fn validate_state_paths(&self, state: &InstallState) -> Result<()> {
        if !paths_refer_to_same_file(&absolute(state.config_path.clone())?, &self.config_path)? {
            bail!("Codex integration sidecar belongs to a different config path");
        }
        let backup = absolute(state.backup_path.clone())?;
        let config_parent = self.config_path.parent().unwrap_or_else(|| Path::new(""));
        if !path_keys_equal(
            backup.parent().unwrap_or_else(|| Path::new("")),
            config_parent,
        ) {
            bail!("Codex integration backup is outside the config directory");
        }
        if paths_refer_to_same_file(&backup, &self.config_path)?
            || paths_refer_to_same_file(&backup, &self.sidecar_path)?
            || paths_refer_to_same_file(&backup, &self.lock_path)?
        {
            bail!("Codex integration backup aliases a managed file");
        }
        let config_name = self
            .config_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Codex config path has no UTF-8 filename")?;
        let backup_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .context("Codex backup path has no UTF-8 filename")?;
        if !backup_name.starts_with(&format!("{config_name}.cmr-backup-")) {
            bail!("Codex integration backup name is invalid");
        }
        Ok(())
    }

    fn create_backup(&self, contents: &[u8]) -> Result<PathBuf> {
        let config_name = self
            .config_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Codex config path has no UTF-8 filename")?;
        let parent = self.config_path.parent().unwrap_or_else(|| Path::new(""));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        for attempt in 0_u8..=10 {
            let path = parent.join(format!(
                "{config_name}.cmr-backup-{stamp}-{}-{attempt}",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    apply_target_permissions(
                        &file,
                        metadata_if_exists(&self.config_path)?.as_ref(),
                    )?;
                    file.write_all(contents)
                        .with_context(|| format!("write backup {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("flush backup {}", path.display()))?;
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("create backup {}", path.display()));
                }
            }
        }
        bail!("could not allocate a unique Codex config backup name")
    }
}

fn validate_distinct_managed_paths(config: &Path, sidecar: &Path, lock: &Path) -> Result<()> {
    if paths_refer_to_same_file(config, sidecar)? {
        bail!("Codex config and integration sidecar must be different files");
    }
    if paths_refer_to_same_file(config, lock)? || paths_refer_to_same_file(sidecar, lock)? {
        bail!("Codex integration lock must be a distinct file");
    }
    Ok(())
}

fn parse_document(source: &str, path: &Path) -> Result<DocumentMut> {
    if source.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    source
        .parse::<DocumentMut>()
        .with_context(|| format!("parse TOML {}", path.display()))
}

fn ensure_features_table(document: &DocumentMut) -> Result<()> {
    if let Some(features) = document.get("features") {
        if features.as_table_like().is_none() {
            bail!("Codex config `features` must be a table");
        }
    }
    Ok(())
}

fn set_remote_control(document: &mut DocumentMut, enabled: bool) -> Result<()> {
    if document.get("features").is_none() {
        document["features"] = Item::Table(Table::new());
    }
    let features = document
        .get_mut("features")
        .and_then(Item::as_table_like_mut)
        .context("Codex config `features` must be a table")?;
    features.insert("remote_control", value(enabled));
    Ok(())
}

fn remote_control(document: &DocumentMut) -> Option<&Item> {
    document
        .get("features")?
        .as_table_like()?
        .get("remote_control")
}

fn select_original(
    current: Option<&Item>,
    previous: Option<&SavedItem>,
    still_has_installed_value: bool,
) -> SavedItem {
    if still_has_installed_value {
        if let Some(previous) = previous {
            return previous.clone();
        }
        return SavedItem {
            changed_by_install: false,
            item_toml: current.map(serialize_item),
        };
    }
    SavedItem {
        changed_by_install: true,
        item_toml: current.map(serialize_item),
    }
}

fn serialize_item(item: &Item) -> String {
    let mut document = DocumentMut::new();
    document.insert("value", item.clone());
    document.to_string()
}

fn deserialize_item(saved: &SavedItem) -> Result<Option<Item>> {
    let Some(source) = &saved.item_toml else {
        return Ok(None);
    };
    let mut document = source
        .parse::<DocumentMut>()
        .context("parse an original Codex config value from integration state")?;
    Ok(document.remove("value"))
}

fn restore_root_string(
    current: &mut DocumentMut,
    key: &str,
    installed: &str,
    original: &SavedItem,
    restored: &mut usize,
    preserved_user_changes: &mut usize,
) -> Result<()> {
    if !original.changed_by_install {
        return Ok(());
    }
    if current.get(key).and_then(Item::as_str) != Some(installed) {
        *preserved_user_changes += 1;
        return Ok(());
    }
    if let Some(item) = deserialize_item(original)? {
        current.insert(key, item);
    } else {
        current.remove(key);
    }
    *restored += 1;
    Ok(())
}

fn restore_remote_control(
    current: &mut DocumentMut,
    installed: bool,
    original: &SavedItem,
    original_had_features: bool,
    restored: &mut usize,
    preserved_user_changes: &mut usize,
) -> Result<()> {
    if !original.changed_by_install {
        return Ok(());
    }
    if remote_control(current).and_then(Item::as_bool) != Some(installed) {
        *preserved_user_changes += 1;
        return Ok(());
    }
    let original_remote = deserialize_item(original)?;
    let features = current
        .get_mut("features")
        .and_then(Item::as_table_like_mut)
        .context("Codex config `features` changed from a table")?;
    if let Some(item) = original_remote {
        features.insert("remote_control", item);
    } else {
        features.remove("remote_control");
    }
    let now_empty = features.iter().next().is_none();
    if now_empty && !original_had_features {
        current.remove("features");
    }
    *restored += 1;
    Ok(())
}

fn default_sidecar_path(config_path: &Path) -> PathBuf {
    let filename = config_path
        .file_name()
        .map_or_else(|| "config.toml".into(), |name| name.to_string_lossy());
    config_path.with_file_name(format!("{filename}.cmr-state.json"))
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes its filesystem root: {}", path.display());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Ok(normalized)
}

fn router_base_url(host: &str, port: u16) -> Result<String> {
    if port == 0 {
        bail!("router port must be non-zero");
    }
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let address: IpAddr = unbracketed
        .parse()
        .with_context(|| format!("router host `{host}` must be an IP address"))?;
    if !address.is_loopback() {
        bail!("router host `{host}` must be loopback-only");
    }
    Ok(match address {
        IpAddr::V4(address) => format!("http://{address}:{port}/v1"),
        IpAddr::V6(address) => format!("http://[{address}]:{port}/v1"),
    })
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> Result<bool> {
    if path_keys_equal(left, right) {
        return Ok(true);
    }
    if left.exists() && right.exists() {
        return same_file::is_same_file(left, right).with_context(|| {
            format!(
                "compare file identities for {} and {}",
                left.display(),
                right.display()
            )
        });
    }
    let resolved_left = resolve_existing_prefix(left)?;
    let resolved_right = resolve_existing_prefix(right)?;
    Ok(path_keys_equal(&resolved_left, &resolved_right))
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut prefix = path.to_path_buf();
    let mut suffix = Vec::new();
    while !prefix.exists() {
        let name = prefix
            .file_name()
            .context("path has no existing filesystem prefix")?
            .to_os_string();
        suffix.push(name);
        if !prefix.pop() {
            bail!("path has no existing filesystem prefix: {}", path.display());
        }
    }
    let mut resolved = fs::canonicalize(&prefix)
        .with_context(|| format!("resolve existing path prefix {}", prefix.display()))?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(windows)]
fn path_keys_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn path_keys_equal(left: &Path, right: &Path) -> bool {
    left == right
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn metadata_if_exists(path: &Path) -> Result<Option<Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspect permissions for {}", path.display()))
        }
    }
}

fn atomic_write_cas(
    path: &Path,
    expected: Option<&[u8]>,
    contents: &[u8],
    previous_metadata: Option<&Metadata>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("atomic write target has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("write temporary file for {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("flush temporary file for {}", path.display()))?;
    apply_target_permissions(temporary.as_file(), previous_metadata)?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync temporary file for {}", path.display()))?;

    let current = read_optional(path)?;
    let unchanged = match (expected, current.as_deref()) {
        (None, None) => true,
        (Some(expected), Some(current)) => expected == current,
        _ => false,
    };
    if !unchanged {
        bail!(
            "concurrent modification detected while updating {}",
            path.display()
        );
    }

    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace {}", path.display()))?;
    sync_parent_directory(parent)?;
    Ok(())
}

fn remove_file_cas(path: &Path, expected: &[u8]) -> Result<()> {
    let current = read_optional(path)?;
    if current.as_deref() != Some(expected) {
        bail!(
            "concurrent modification detected while removing {}",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    if let Some(parent) = path.parent() {
        sync_parent_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_target_permissions(file: &File, previous: Option<&Metadata>) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = previous.map_or(0o600, |metadata| metadata.mode() & 0o7777);
    file.set_permissions(fs::Permissions::from_mode(mode))
        .context("apply file permissions")
}

#[cfg(not(unix))]
fn apply_target_permissions(file: &File, _previous: Option<&Metadata>) -> Result<()> {
    // A same-directory temporary file inherits the directory ACL on Windows.
    // `std` exposes only the read-only bit, so copying `Permissions` would not
    // preserve the ACL and can make the replacement unexpectedly immutable.
    file.metadata().context("verify temporary file metadata")?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)
        .with_context(|| format!("open directory {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    fs::metadata(parent)
        .with_context(|| format!("verify replacement directory {}", parent.display()))?;
    Ok(())
}

struct OperationLock {
    path: PathBuf,
    marker: Vec<u8>,
}

impl OperationLock {
    fn acquire(path: &Path) -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        let marker = format!("cmr:{}:{stamp}\n", std::process::id()).into_bytes();
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "another Codex config operation holds lock {}",
                    path.display()
                )
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create lock {}", path.display()));
            }
        };
        let result = (|| -> Result<()> {
            apply_target_permissions(&file, None)?;
            file.write_all(&marker)
                .with_context(|| format!("write lock {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync lock {}", path.display()))?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(Self {
            path: path.to_path_buf(),
            marker,
        })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        if fs::read(&self.path).is_ok_and(|contents| contents.as_slice() == self.marker.as_slice())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BASE_URL: &str = "http://127.0.0.1:15722/v1";

    fn fixture() -> (tempfile::TempDir, CodexIntegration) {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join(".codex").join("config.toml");
        let sidecar = directory.path().join("state").join("codex.json");
        let integration =
            CodexIntegration::new(&config, Some(sidecar), "127.0.0.1", 15722).expect("integration");
        (directory, integration)
    }

    #[test]
    fn install_preserves_unrelated_tables_and_creates_exact_backup() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        let original = r#"model = "gpt-existing"
sandbox_mode = "workspace-write"

[features]
web_search = true

[mcp_servers.files]
command = "server"

[projects.'C:\\work']
trust_level = "trusted"
"#;
        fs::write(integration.config_path(), original).expect("write original");

        let backup = integration.install().expect("install");
        assert_eq!(fs::read_to_string(&backup).expect("backup"), original);
        assert_eq!(
            integration.status().expect("status"),
            IntegrationStatus::Installed
        );

        let merged = fs::read_to_string(integration.config_path()).expect("merged");
        let document = merged.parse::<DocumentMut>().expect("parse merged");
        assert_eq!(document["model"].as_str(), Some("gpt-existing"));
        assert_eq!(document["sandbox_mode"].as_str(), Some("workspace-write"));
        assert_eq!(document["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(
            document["mcp_servers"]["files"]["command"].as_str(),
            Some("server")
        );
        assert_eq!(document["model_provider"].as_str(), Some(MODEL_PROVIDER));
        assert_eq!(document["openai_base_url"].as_str(), Some(TEST_BASE_URL));
        assert_eq!(document["features"]["remote_control"].as_bool(), Some(true));
    }

    #[test]
    fn uninstall_restores_only_values_that_have_not_drifted() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        let original = r#"model_provider = "legacy"
openai_base_url = "https://example.invalid/v1"

[features]
remote_control = false
other = true

[mcp_servers.keep]
command = "keep-me"
"#;
        fs::write(integration.config_path(), original).expect("write original");
        let backup = integration.install().expect("install");

        let merged = fs::read_to_string(integration.config_path()).expect("read merged");
        let mut document = merged.parse::<DocumentMut>().expect("parse merged");
        document["openai_base_url"] = value("https://user-changed.example/v1");
        document["projects"]["later"]["trust_level"] = value("trusted");
        fs::write(integration.config_path(), document.to_string()).expect("write drift");

        assert_eq!(
            integration.status().expect("status"),
            IntegrationStatus::Drifted
        );
        let report = integration.uninstall().expect("uninstall");
        assert_eq!(report.restored, 2);
        assert_eq!(report.preserved_user_changes, 1);
        assert_eq!(report.backup_path.as_deref(), Some(backup.as_path()));
        assert!(!integration.sidecar_path().exists());
        assert!(backup.exists(), "the recovery backup must be retained");

        let restored = fs::read_to_string(integration.config_path()).expect("restored");
        let document = restored.parse::<DocumentMut>().expect("parse restored");
        assert_eq!(document["model_provider"].as_str(), Some("legacy"));
        assert_eq!(
            document["openai_base_url"].as_str(),
            Some("https://user-changed.example/v1")
        );
        assert_eq!(
            document["features"]["remote_control"].as_bool(),
            Some(false)
        );
        assert_eq!(document["features"]["other"].as_bool(), Some(true));
        assert_eq!(
            document["projects"]["later"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            document["mcp_servers"]["keep"]["command"].as_str(),
            Some("keep-me")
        );
    }

    #[test]
    fn uninstall_of_new_config_keeps_later_unrelated_values() {
        let (_directory, integration) = fixture();
        integration.install().expect("install empty");
        let merged = fs::read_to_string(integration.config_path()).expect("read merged");
        let mut document = merged.parse::<DocumentMut>().expect("parse merged");
        document["features"]["web_search"] = value(true);
        document["approval_policy"] = value("on-request");
        fs::write(integration.config_path(), document.to_string()).expect("write additions");

        integration.uninstall().expect("uninstall");
        let restored = fs::read_to_string(integration.config_path()).expect("read restored");
        let document = restored.parse::<DocumentMut>().expect("parse restored");
        assert!(document.get("model_provider").is_none());
        assert!(document.get("openai_base_url").is_none());
        assert!(remote_control(&document).is_none());
        assert_eq!(document["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(document["approval_policy"].as_str(), Some("on-request"));
    }

    #[test]
    fn reinstall_rebases_restore_point_without_overwriting_old_backup() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        fs::write(integration.config_path(), "model_provider = \"first\"\n").expect("first");
        let first_backup = integration.install().expect("first install");

        let merged = fs::read_to_string(integration.config_path()).expect("read merged");
        let mut document = merged.parse::<DocumentMut>().expect("parse merged");
        document["model_provider"] = value("second");
        fs::write(integration.config_path(), document.to_string()).expect("second value");
        let second_backup = integration.install().expect("second install");

        assert_ne!(first_backup, second_backup);
        assert!(first_backup.exists());
        integration.uninstall().expect("uninstall");
        let restored = fs::read_to_string(integration.config_path()).expect("restored");
        let document = restored.parse::<DocumentMut>().expect("parse restored");
        assert_eq!(document["model_provider"].as_str(), Some("second"));
    }

    #[test]
    fn uninstall_does_not_claim_values_that_already_matched() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        let original = format!(
            "model_provider = \"{MODEL_PROVIDER}\"\nopenai_base_url = \"{TEST_BASE_URL}\"\n\n[features]\nremote_control = true\nother = true\n"
        );
        fs::write(integration.config_path(), &original).expect("write original");

        integration.install().expect("install");
        let report = integration.uninstall().expect("uninstall");
        assert_eq!(report.restored, 0);
        assert_eq!(report.preserved_user_changes, 0);
        let restored = fs::read_to_string(integration.config_path()).expect("restored");
        let document = restored.parse::<DocumentMut>().expect("parse restored");
        assert_eq!(document["model_provider"].as_str(), Some(MODEL_PROVIDER));
        assert_eq!(document["openai_base_url"].as_str(), Some(TEST_BASE_URL));
        assert_eq!(document["features"]["remote_control"].as_bool(), Some(true));
        assert_eq!(document["features"]["other"].as_bool(), Some(true));
    }

    #[test]
    fn router_base_url_formats_ipv4_and_ipv6_loopback() {
        assert_eq!(
            router_base_url("127.0.0.1", 15722).expect("IPv4 URL"),
            TEST_BASE_URL
        );
        assert_eq!(
            router_base_url("::1", 15722).expect("IPv6 URL"),
            "http://[::1]:15722/v1"
        );
        assert_eq!(
            router_base_url("[::1]", 15722).expect("bracketed IPv6 URL"),
            "http://[::1]:15722/v1"
        );
        assert!(router_base_url("0.0.0.0", 15722).is_err());
    }

    #[test]
    fn constructor_rejects_same_normalized_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("config.toml");
        let alias = directory
            .path()
            .join("nested")
            .join("..")
            .join("config.toml");
        let error = CodexIntegration::new(&config, Some(alias), "127.0.0.1", 15722)
            .expect_err("same path must be rejected");
        assert!(error.to_string().contains("must be different files"));
    }

    #[test]
    fn constructor_rejects_hard_link_identity() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("config.toml");
        let sidecar = directory.path().join("state.json");
        fs::write(&config, "model = \"test\"\n").expect("write config");
        fs::hard_link(&config, &sidecar).expect("create hard link");
        let error = CodexIntegration::new(&config, Some(sidecar), "127.0.0.1", 15722)
            .expect_err("same file identity must be rejected");
        assert!(error.to_string().contains("must be different files"));
    }

    #[test]
    fn install_rechecks_file_identity_created_after_construction() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("config.toml");
        let sidecar = directory.path().join("state").join("codex.json");
        let integration = CodexIntegration::new(&config, Some(sidecar.clone()), "127.0.0.1", 15722)
            .expect("initially distinct paths");
        fs::create_dir_all(sidecar.parent().unwrap()).expect("create sidecar directory");
        let original = b"model = \"unchanged\"\n";
        fs::write(&config, original).expect("write config");
        fs::hard_link(&config, &sidecar).expect("alias sidecar to config");

        let error = integration
            .install()
            .expect_err("late file-identity alias must be rejected");

        assert!(error.to_string().contains("must be different files"));
        assert_eq!(fs::read(&config).expect("read unchanged config"), original);
        assert_eq!(
            fs::read(&sidecar).expect("read unchanged sidecar"),
            original
        );
        assert!(!integration.lock_path.exists());
    }

    #[test]
    fn operation_lock_prevents_concurrent_install() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        fs::write(&integration.lock_path, b"another owner\n").expect("write lock");
        let error = integration.install().expect_err("install must honor lock");
        assert!(error.to_string().contains("another Codex config operation"));
        assert!(!integration.config_path().exists());
    }

    #[test]
    fn atomic_write_rejects_stale_compare_and_swap() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, b"newer").expect("write current");
        let error = atomic_write_cas(&path, Some(b"older"), b"replacement", None)
            .expect_err("stale CAS must fail");
        assert!(error.to_string().contains("concurrent modification"));
        assert_eq!(fs::read(&path).expect("read current"), b"newer");
    }

    #[test]
    fn endpoint_change_keeps_original_restore_point() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("config.toml");
        let sidecar = directory.path().join("state.json");
        fs::write(
            &config,
            "openai_base_url = \"https://original.invalid/v1\"\n",
        )
        .expect("write original");
        let first = CodexIntegration::new(&config, Some(sidecar.clone()), "127.0.0.1", 15722)
            .expect("first integration");
        first.install().expect("first install");
        let second = CodexIntegration::new(&config, Some(sidecar), "::1", 25722)
            .expect("second integration");
        second.install().expect("second install");
        second.uninstall().expect("uninstall");
        let restored = fs::read_to_string(config).expect("read restored");
        let document = restored.parse::<DocumentMut>().expect("parse restored");
        assert_eq!(
            document["openai_base_url"].as_str(),
            Some("https://original.invalid/v1")
        );
    }

    #[test]
    fn endpoint_change_is_drifted_until_reinstalled() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = directory.path().join("config.toml");
        let sidecar = directory.path().join("state.json");
        let first = CodexIntegration::new(&config, Some(sidecar.clone()), "127.0.0.1", 15722)
            .expect("first integration");
        first.install().expect("first install");
        let second = CodexIntegration::new(&config, Some(sidecar), "127.0.0.1", 15723)
            .expect("second integration");

        assert_eq!(
            second.status().expect("changed endpoint status"),
            IntegrationStatus::Drifted
        );
        second.install().expect("install changed endpoint");
        assert_eq!(
            second.status().expect("updated endpoint status"),
            IntegrationStatus::Installed
        );
        let updated = fs::read_to_string(config).expect("read updated config");
        assert!(updated.contains("openai_base_url = \"http://127.0.0.1:15723/v1\""));
    }

    #[test]
    fn restore_reverts_drifted_config_to_exact_pre_install_snapshot() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        let original = r#"model_provider = "legacy"
openai_base_url = "https://example.invalid/v1"

[features]
remote_control = false
other = true

[mcp_servers.keep]
command = "keep-me"
"#;
        fs::write(integration.config_path(), original).expect("write original");
        let backup = integration.install().expect("install");

        // Simulate heavy post-install user editing: a managed value is changed
        // and an unrelated table is added. `uninstall` would preserve the
        // changed base URL; `restore` must discard both edits.
        let merged = fs::read_to_string(integration.config_path()).expect("read merged");
        let mut document = merged.parse::<DocumentMut>().expect("parse merged");
        document["openai_base_url"] = value("https://user-changed.example/v1");
        document["projects"]["later"]["trust_level"] = value("trusted");
        fs::write(integration.config_path(), document.to_string()).expect("write drift");

        let report = integration.restore().expect("restore");
        assert!(report.restored);
        assert_eq!(report.backup_path.as_deref(), Some(backup.as_path()));
        assert!(!integration.sidecar_path().exists());
        assert!(backup.exists(), "the recovery backup must be retained");

        // Byte-for-byte equality with the pre-install file, including the loss
        // of the unrelated `[projects.later]` table the user added later.
        assert_eq!(
            fs::read(integration.config_path()).expect("read restored"),
            original.as_bytes()
        );
    }

    #[test]
    fn restore_removes_config_that_did_not_exist_before_install() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        // No pre-existing config: install creates it, restore must delete it.
        integration.install().expect("install");
        assert!(integration.config_path().exists());

        // Add unrelated content after install; restore still removes the file.
        fs::write(
            integration.config_path(),
            "model = \"x\"\n[features]\nweb_search = true\n",
        )
        .expect("write additions");

        let report = integration.restore().expect("restore");
        assert!(report.restored);
        assert!(!integration.config_path().exists());
        assert!(!integration.sidecar_path().exists());
        assert!(report.backup_path.is_some_and(|p| p.exists()));
    }

    #[test]
    fn restore_without_install_is_a_no_op() {
        let (_directory, integration) = fixture();
        fs::create_dir_all(integration.config_path().parent().unwrap()).expect("mkdir");
        fs::write(integration.config_path(), "model = \"untouched\"\n").expect("write config");

        let report = integration.restore().expect("restore");
        assert!(!report.restored);
        assert_eq!(report.backup_path, None);
        // Nothing changed: no sidecar was ever created.
        assert!(!integration.sidecar_path().exists());
        assert_eq!(
            fs::read_to_string(integration.config_path()).expect("read config"),
            "model = \"untouched\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_managed_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, integration) = fixture();
        let backup = integration.install().expect("install");
        for path in [
            integration.config_path(),
            integration.sidecar_path(),
            &backup,
        ] {
            let mode = fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} should be owner-only", path.display());
        }
    }
}
