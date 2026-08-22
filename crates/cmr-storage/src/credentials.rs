use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Result, StorageError};

const LEGACY_SERVICE: &str = "codex-model-router";
const SCOPED_SERVICE_PREFIX: &str = "codex-model-router.instance.";
const MAX_REFERENCE_PART_LEN: usize = 64;

/// Stable, non-secret identity for one configuration file.
///
/// The value is a SHA-256 digest of the normalized absolute config path. Moving a
/// portable config intentionally creates a new vault namespace instead of sharing
/// credentials with the old location by accident.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ConfigInstanceId(String);

impl ConfigInstanceId {
    /// Derives an instance id without reading the config contents.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be made absolute or canonicalized, or
    /// if it does not identify a file name.
    pub fn for_path(path: impl AsRef<Path>) -> Result<Self> {
        let normalized = normalize_config_path(path.as_ref())?;
        let mut identity = normalized.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            identity.make_ascii_lowercase();
        }
        let mut digest = Sha256::new();
        digest.update(b"cmr-config-instance-v1\0");
        digest.update(identity.as_bytes());
        Ok(Self(format!("{:x}", digest.finalize())))
    }

    /// Parses a previously derived instance id.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` is exactly one lowercase, 64-character
    /// hexadecimal SHA-256 digest.
    pub fn parse(value: &str) -> Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(StorageError::InvalidConfig(
                "config instance id must be a lowercase SHA-256 digest".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the non-secret digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn normalize_config_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute.exists() {
        return Ok(absolute.canonicalize()?);
    }
    let Some(file_name) = absolute.file_name() else {
        return Err(StorageError::InvalidConfig(
            "config path must name a file".into(),
        ));
    };
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let normalized_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    Ok(normalized_parent.join(file_name))
}

/// Immutable owner id for provider-private response material.
///
/// It binds the provider definition to a config instance and the exact
/// generation-specific credential reference. It never hashes credential values.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderOwnerId(String);

impl ProviderOwnerId {
    /// Constructs an opaque provenance owner id.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider id is invalid, the endpoint identity is
    /// empty, or the credential reference belongs to another provider or is not
    /// generation-specific.
    pub fn new(
        instance: &ConfigInstanceId,
        provider_id: &str,
        endpoint_identity: &str,
        secret_ref: Option<&SecretRef>,
    ) -> Result<Self> {
        if let Some(reference) = secret_ref {
            reference.validate_provider(provider_id)?;
            if reference.generation().is_none() {
                return Err(StorageError::InvalidConfig(
                    "provider owner requires a generation-specific credential reference; legacy mutable references have no safe private replay owner"
                        .into(),
                ));
            }
        }
        Self::for_credential_generation(
            instance,
            provider_id,
            endpoint_identity,
            secret_ref.map_or("none", SecretRef::account),
        )
    }

    /// Constructs an owner for a credential generation that is not represented
    /// by a vault reference, such as the bound official `ChatGPT` account.
    ///
    /// `credential_generation` must be a non-secret stable identifier (for
    /// example, a local generation UUID or an already-hashed account binding).
    ///
    /// # Errors
    ///
    /// Returns an error if the provider id is not a short safe identifier, the
    /// endpoint identity is empty, or the credential-generation identity is empty
    /// or longer than 256 bytes.
    pub fn for_credential_generation(
        instance: &ConfigInstanceId,
        provider_id: &str,
        endpoint_identity: &str,
        credential_generation: &str,
    ) -> Result<Self> {
        validate_reference_part("provider", provider_id)?;
        if endpoint_identity.is_empty() {
            return Err(StorageError::InvalidConfig(
                "provider endpoint identity cannot be empty".into(),
            ));
        }
        if credential_generation.is_empty() || credential_generation.len() > 256 {
            return Err(StorageError::InvalidConfig(
                "credential generation identity must be non-empty and at most 256 bytes".into(),
            ));
        }
        let mut digest = Sha256::new();
        for part in [
            "cmr-provider-owner-v1",
            instance.as_str(),
            provider_id,
            endpoint_identity,
            credential_generation,
        ] {
            digest.update(part.as_bytes());
            digest.update([0]);
        }
        Ok(Self(format!("pvo_sha256_{:x}", digest.finalize())))
    }

    /// Parses an owner id previously returned by [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns an error if `value` lacks the supported prefix or does not contain
    /// a lowercase, 64-character hexadecimal digest.
    pub fn parse(value: &str) -> Result<Self> {
        let Some(digest) = value.strip_prefix("pvo_sha256_") else {
            return Err(StorageError::InvalidConfig(
                "provider owner id uses an unsupported format".into(),
            ));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(StorageError::InvalidConfig(
                "provider owner id contains an invalid digest".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the opaque non-secret id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderOwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ProviderOwnerId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderOwnerId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

/// A non-secret reference to one credential-vault entry.
///
/// Legacy references use `provider/profile`. New writes should use
/// `provider/profile/g/<uuid>` so rotating a secret never overwrites the entry a
/// running router is using.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SecretRef {
    value: String,
    provider: String,
    profile: String,
    generation: Option<Uuid>,
}

impl SecretRef {
    /// Creates a legacy provider/profile reference.
    ///
    /// # Errors
    ///
    /// Returns an error if either component is empty, too long, contains unsafe
    /// characters, or resembles credential material.
    pub fn new(provider: &str, profile: &str) -> Result<Self> {
        Self::from_parts(provider, profile, None)
    }

    /// Creates a new generation-specific reference for an atomic credential rotation.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider or profile is empty, too long, contains
    /// unsafe characters, or resembles credential material.
    pub fn new_generation(provider: &str, profile: &str) -> Result<Self> {
        Self::with_generation(provider, profile, Uuid::new_v4())
    }

    /// Creates a reference for an explicit generation id.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider or profile is empty, too long, contains
    /// unsafe characters, or resembles credential material.
    pub fn with_generation(provider: &str, profile: &str, generation: Uuid) -> Result<Self> {
        Self::from_parts(provider, profile, Some(generation))
    }

    fn from_parts(provider: &str, profile: &str, generation: Option<Uuid>) -> Result<Self> {
        validate_reference_part("provider", provider)?;
        validate_reference_part("profile", profile)?;
        if looks_like_secret(provider) || looks_like_secret(profile) {
            return Err(StorageError::InvalidConfig(
                "credential profile resembles secret material; store only a short profile name"
                    .into(),
            ));
        }
        let value = generation.map_or_else(
            || format!("{provider}/{profile}"),
            |generation| format!("{provider}/{profile}/g/{generation}"),
        );
        Ok(Self {
            value,
            provider: provider.to_owned(),
            profile: profile.to_owned(),
            generation,
        })
    }

    /// Parses a persisted reference without accepting credential material.
    ///
    /// # Errors
    ///
    /// Returns an error if the reference has an unsupported shape, contains an
    /// invalid UUID, has unsafe provider/profile components, or resembles a secret.
    pub fn parse(value: &str) -> Result<Self> {
        let parts = value.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            [provider, profile] => Self::new(provider, profile),
            [provider, profile, "g", generation] => {
                let generation = Uuid::parse_str(generation).map_err(|_| {
                    StorageError::InvalidConfig(
                        "credential reference generation must be a UUID".into(),
                    )
                })?;
                Self::with_generation(provider, profile, generation)
            }
            _ => Err(StorageError::InvalidConfig(
                "credential reference must be provider/profile or provider/profile/g/uuid".into(),
            )),
        }
    }

    /// Validates that this reference belongs to the configured provider.
    ///
    /// # Errors
    ///
    /// Returns an error when `provider_id` differs from the provider encoded in
    /// this reference.
    pub fn validate_provider(&self, provider_id: &str) -> Result<()> {
        if self.provider != provider_id {
            return Err(StorageError::InvalidConfig(format!(
                "credential reference belongs to provider {}, not {provider_id}",
                self.provider
            )));
        }
        Ok(())
    }

    /// Returns the provider portion.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the user-facing profile portion.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Returns the immutable credential generation, if present.
    #[must_use]
    pub fn generation(&self) -> Option<Uuid> {
        self.generation
    }

    /// Returns the operating-system vault account name.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

impl Serialize for SecretRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.account())
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

fn validate_reference_part(kind: &str, value: &str) -> Result<()> {
    let safe = !value.is_empty()
        && value.len() <= MAX_REFERENCE_PART_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !safe {
        return Err(StorageError::InvalidConfig(format!(
            "credential reference {kind} must be a short alphanumeric id"
        )));
    }
    Ok(())
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let has_letter = value.bytes().any(|byte| byte.is_ascii_alphabetic());
    let has_digit = value.bytes().any(|byte| byte.is_ascii_digit());
    let long_random_token = value.len() >= 32 && has_letter && has_digit;
    let long_hex = value.len() >= 24 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    let dotted_token = value
        .split_once('.')
        .is_some_and(|(left, right)| left.len() >= 12 && right.len() >= 12);
    lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("api-key")
        || lower.starts_with("apikey")
        || lower.starts_with("bearer")
        || long_random_token
        || long_hex
        || dotted_token
}

/// Secret storage abstraction used by CLI and router.
pub trait CredentialStore: Send + Sync {
    /// Stores or replaces a credential.
    ///
    /// # Errors
    ///
    /// Returns an error if the secret is rejected or the backing credential store
    /// cannot write it.
    fn set(&self, reference: &SecretRef, secret: &str) -> Result<()>;
    /// Retrieves a credential for request assembly.
    ///
    /// # Errors
    ///
    /// Returns an error if the backing credential store cannot read the entry.
    fn get(&self, reference: &SecretRef) -> Result<Option<String>>;
    /// Removes a credential.
    ///
    /// # Errors
    ///
    /// Returns an error if the backing credential store cannot remove the entry.
    fn delete(&self, reference: &SecretRef) -> Result<()>;

    /// Stages a new, non-overwriting credential generation.
    ///
    /// The caller commits by atomically saving the returned reference in config,
    /// or rolls back by deleting it when that save fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider/profile cannot form a safe reference or
    /// if the backing credential store cannot persist the new secret.
    fn stage_generation(&self, provider: &str, profile: &str, secret: &str) -> Result<SecretRef> {
        let reference = SecretRef::new_generation(provider, profile)?;
        self.set(&reference, secret)?;
        Ok(reference)
    }
}

/// Legacy global vault backend retained only for explicit migration.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsCredentialStore;

impl OsCredentialStore {
    fn entry(reference: &SecretRef) -> Result<keyring::Entry> {
        vault_entry(LEGACY_SERVICE, reference)
    }

    /// Creates the production per-config backend.
    #[must_use]
    pub fn scoped(instance: ConfigInstanceId) -> ScopedCredentialStore {
        ScopedCredentialStore::new(instance)
    }
}

impl CredentialStore for OsCredentialStore {
    fn set(&self, reference: &SecretRef, secret: &str) -> Result<()> {
        set_entry(&Self::entry(reference)?, secret)
    }

    fn get(&self, reference: &SecretRef) -> Result<Option<String>> {
        get_entry(&Self::entry(reference)?)
    }

    fn delete(&self, reference: &SecretRef) -> Result<()> {
        delete_entry(&Self::entry(reference)?)
    }
}

/// Operating-system credential backend isolated to one config instance.
#[derive(Clone, Debug)]
pub struct ScopedCredentialStore {
    instance: ConfigInstanceId,
    service: String,
}

impl ScopedCredentialStore {
    /// Creates a vault namespace for one config instance.
    #[must_use]
    pub fn new(instance: ConfigInstanceId) -> Self {
        Self {
            service: format!("{SCOPED_SERVICE_PREFIX}{}", instance.as_str()),
            instance,
        }
    }

    /// Returns the non-secret namespace id.
    #[must_use]
    pub fn instance_id(&self) -> &ConfigInstanceId {
        &self.instance
    }

    fn entry(&self, reference: &SecretRef) -> Result<keyring::Entry> {
        vault_entry(&self.service, reference)
    }

    /// Explicitly copies one legacy entry into this namespace when no scoped entry exists.
    ///
    /// The legacy entry is retained because another old config may still reference it.
    ///
    /// # Errors
    ///
    /// Returns an error if either vault entry cannot be opened or read, or if the
    /// legacy secret cannot be written to the scoped vault.
    pub fn migrate_legacy(&self, reference: &SecretRef) -> Result<bool> {
        if self.get(reference)?.is_some() {
            return Ok(false);
        }
        let legacy = OsCredentialStore;
        let Some(secret) = legacy.get(reference)? else {
            return Ok(false);
        };
        self.set(reference, &secret)?;
        Ok(true)
    }
}

impl CredentialStore for ScopedCredentialStore {
    fn set(&self, reference: &SecretRef, secret: &str) -> Result<()> {
        set_entry(&self.entry(reference)?, secret)
    }

    fn get(&self, reference: &SecretRef) -> Result<Option<String>> {
        get_entry(&self.entry(reference)?)
    }

    fn delete(&self, reference: &SecretRef) -> Result<()> {
        delete_entry(&self.entry(reference)?)
    }
}

fn vault_entry(service: &str, reference: &SecretRef) -> Result<keyring::Entry> {
    keyring::Entry::new(service, reference.account())
        .map_err(|error| StorageError::Credential(error.to_string()))
}

fn set_entry(entry: &keyring::Entry, secret: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(StorageError::InvalidConfig(
            "credential cannot be empty".into(),
        ));
    }
    entry
        .set_password(secret)
        .map_err(|error| StorageError::Credential(error.to_string()))
}

fn get_entry(entry: &keyring::Entry) -> Result<Option<String>> {
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(StorageError::Credential(error.to_string())),
    }
}

fn delete_entry(entry: &keyring::Entry) -> Result<()> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(StorageError::Credential(error.to_string())),
    }
}

/// In-memory backend for tests and embedding. It never persists values.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore(Mutex<HashMap<SecretRef, String>>);

impl CredentialStore for MemoryCredentialStore {
    fn set(&self, reference: &SecretRef, secret: &str) -> Result<()> {
        if secret.is_empty() {
            return Err(StorageError::InvalidConfig(
                "credential cannot be empty".into(),
            ));
        }
        self.0
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .insert(reference.clone(), secret.to_owned());
        Ok(())
    }

    fn get(&self, reference: &SecretRef) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .get(reference)
            .cloned())
    }

    fn delete(&self, reference: &SecretRef) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .remove(reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_uses_reference_not_value() {
        let store = MemoryCredentialStore::default();
        let reference = SecretRef::new("zhipu", "default").expect("reference");
        store.set(&reference, "test-secret").expect("set");
        assert_eq!(
            store.get(&reference).expect("get").as_deref(),
            Some("test-secret")
        );
        store.delete(&reference).expect("delete");
        assert_eq!(store.get(&reference).expect("get"), None);
    }

    #[test]
    fn generation_references_never_overwrite_one_another() {
        let store = MemoryCredentialStore::default();
        let first = store
            .stage_generation("zhipu", "default", "first")
            .expect("first generation");
        let second = store
            .stage_generation("zhipu", "default", "second")
            .expect("second generation");
        assert_ne!(first, second);
        assert!(first.generation().is_some());
        assert_eq!(store.get(&first).expect("first"), Some("first".into()));
        assert_eq!(store.get(&second).expect("second"), Some("second".into()));
    }

    #[test]
    fn strict_reference_parser_rejects_secret_like_and_cross_provider_values() {
        for rejected in [
            "zhipu/not_a_real_credential_000000000000",
            "zhipu/default/extra",
            "zhipu/default/g/not-a-uuid",
            "zhipu//default",
        ] {
            assert!(SecretRef::parse(rejected).is_err(), "accepted {rejected}");
        }
        let reference = SecretRef::new("zhipu", "default").expect("reference");
        assert!(reference.validate_provider("zhipu").is_ok());
        assert!(reference.validate_provider("other").is_err());
    }

    #[test]
    fn provider_owner_changes_with_credential_generation_and_config_instance() {
        let directory = tempfile::tempdir().expect("temp dir");
        let first_instance =
            ConfigInstanceId::for_path(directory.path().join("one.toml")).expect("first instance");
        let second_instance =
            ConfigInstanceId::for_path(directory.path().join("two.toml")).expect("second instance");
        let first_ref = SecretRef::new_generation("zhipu", "default").expect("first ref");
        let second_ref = SecretRef::new_generation("zhipu", "default").expect("second ref");
        let first = ProviderOwnerId::new(
            &first_instance,
            "zhipu",
            "https://example.test/v1",
            Some(&first_ref),
        )
        .expect("first owner");
        let rotated = ProviderOwnerId::new(
            &first_instance,
            "zhipu",
            "https://example.test/v1",
            Some(&second_ref),
        )
        .expect("rotated owner");
        let other_config = ProviderOwnerId::new(
            &second_instance,
            "zhipu",
            "https://example.test/v1",
            Some(&first_ref),
        )
        .expect("other config owner");
        assert_ne!(first, rotated);
        assert_ne!(first, other_config);
        assert!(ProviderOwnerId::parse(first.as_str()).is_ok());
    }

    #[test]
    fn provider_owner_rejects_mutable_legacy_or_foreign_credential_references() {
        let directory = tempfile::tempdir().expect("temp dir");
        let instance =
            ConfigInstanceId::for_path(directory.path().join("config.toml")).expect("instance");
        let legacy = SecretRef::new("zhipu", "default").expect("legacy reference");
        let foreign = SecretRef::new_generation("other", "default").expect("foreign reference");

        assert!(
            ProviderOwnerId::new(&instance, "zhipu", "https://example.test/v1", Some(&legacy))
                .is_err()
        );
        assert!(
            ProviderOwnerId::new(
                &instance,
                "zhipu",
                "https://example.test/v1",
                Some(&foreign)
            )
            .is_err()
        );
    }

    #[test]
    fn secret_ref_serde_always_validates() {
        let reference = SecretRef::new_generation("zhipu", "default").expect("reference");
        let encoded = serde_json::to_string(&reference).expect("serialize");
        assert_eq!(
            serde_json::from_str::<SecretRef>(&encoded).expect("deserialize"),
            reference
        );
        assert!(serde_json::from_str::<SecretRef>("\"plaintext-key\"").is_err());
    }
}
