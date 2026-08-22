//! Command-line entry point for Codex Model Router.
//!
//! The CLI edits only the router's non-secret configuration. API keys are read
//! from an invisible terminal prompt and passed directly to the operating-system
//! credential vault.

mod args;
/// Safe user-level Codex configuration integration.
pub mod codex;
/// Native per-user background-service management.
pub mod service;

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use args::{
    Cli, CodexCommand, Command, ConfigCommand, ModelAddArgs, ModelCommand, ProviderAddArgs,
    ProviderCommand, SecretCommand, ServiceCommand,
};
use clap::Parser;
use cmr_providers::{
    AuthStyle, ProviderPreset, built_in_presets, custom_compatible_preset, preset_by_id,
};
use cmr_router::AppState;
use cmr_storage::{
    AppPaths, CompatibilityPolicy, ConfigCommitOutcome, ConfigRevision, ConfigStore,
    CredentialStore, ModelConfig, OsCredentialStore, ProviderConfig, RouterConfig, SecretRef,
    StateStore, StorageError,
};
use codex::{CodexIntegration, IntegrationStatus};
use directories::UserDirs;
use serde_json::Value;
use service::{ServiceManager, ServiceStatus, SystemRunner};

/// Parses process arguments and executes one CLI command.
///
/// # Errors
///
/// Returns an error when arguments are invalid or the selected command cannot
/// read, validate, or update its required local state.
pub async fn main_entry() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("cmr=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    run(Cli::parse()).await
}

#[derive(Clone, Debug)]
struct RuntimePaths {
    config: PathBuf,
    state_db: PathBuf,
    codex_config: PathBuf,
    codex_sidecar: Option<PathBuf>,
}

impl RuntimePaths {
    fn resolve(cli: &Cli) -> Result<Self> {
        let discovered = AppPaths::discover().context("resolve router paths")?;
        let user_dirs = UserDirs::new().context("resolve the current user's home directory")?;
        Ok(Self {
            config: cli.config.clone().unwrap_or(discovered.config_file),
            state_db: cli.state_db.clone().unwrap_or(discovered.state_db),
            codex_config: cli
                .codex_config
                .clone()
                .unwrap_or_else(|| user_dirs.home_dir().join(".codex").join("config.toml")),
            codex_sidecar: cli.codex_sidecar.clone(),
        })
    }

    fn config_store(&self) -> ConfigStore {
        ConfigStore::new(&self.config)
    }

    fn codex(&self, config: &RouterConfig) -> Result<CodexIntegration> {
        CodexIntegration::new(
            &self.codex_config,
            self.codex_sidecar.clone(),
            &config.server.host,
            config.server.port,
        )
    }
}

async fn run(cli: Cli) -> Result<()> {
    let paths = RuntimePaths::resolve(&cli)?;
    match cli.command {
        Command::Serve => serve(&paths).await,
        Command::Doctor => doctor(&paths).await,
        Command::Presets { json } => list_presets(json),
        Command::Config { command } => config_command(&paths, &command),
        Command::Provider { command } => provider_command(&paths, command),
        Command::Model { command } => model_command(&paths, command),
        Command::Secret { command } => secret_command(&paths, command),
        Command::Codex { command } => codex_command(&paths, &command),
        Command::Service { command } => service_command(&paths, &command),
    }
}

async fn serve(paths: &RuntimePaths) -> Result<()> {
    let config_store = paths.config_store();
    let instance_id = config_store.instance_id()?;
    let config = config_store
        .load()
        .with_context(|| format!("load {}", paths.config.display()))?;
    let state = StateStore::open(&paths.state_db)
        .with_context(|| format!("open {}", paths.state_db.display()))?;
    let app = AppState::new_scoped(config, state, instance_id).context("initialize router")?;
    cmr_router::serve(app).await.context("serve router")
}

fn list_presets(json_output: bool) -> Result<()> {
    let presets = built_in_presets();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&presets)?);
        return Ok(());
    }
    println!("ID\tPROTOCOL\tAUTH\tDEFAULT MODEL\tBASE URL");
    for preset in presets {
        println!(
            "{}\t{:?}\t{:?}\t{}\t{}",
            preset.id,
            preset.protocol,
            preset.auth,
            preset.default_model.as_deref().unwrap_or("-"),
            preset.base_url
        );
    }
    Ok(())
}

fn config_command(paths: &RuntimePaths, command: &ConfigCommand) -> Result<()> {
    let store = paths.config_store();
    match command {
        ConfigCommand::Init => {
            let (config, revision) = store.load_with_revision()?;
            if store.path().exists() {
                bail!(
                    "refusing to overwrite existing router config {}",
                    store.path().display()
                );
            }
            debug_assert_eq!(config, RouterConfig::default());
            report_config_commit(&store.save_if_revision(&RouterConfig::default(), &revision)?);
            println!("Created {}", store.path().display());
        }
        ConfigCommand::Path => println!("{}", store.path().display()),
        ConfigCommand::Show => {
            let config = store.load()?;
            println!("{}", toml::to_string_pretty(&config)?);
        }
    }
    Ok(())
}

fn provider_command(paths: &RuntimePaths, command: ProviderCommand) -> Result<()> {
    let store = paths.config_store();
    match command {
        ProviderCommand::List => {
            let config = store.load()?;
            println!("ID\tPRESET\tENABLED\tCREDENTIAL\tBASE URL OVERRIDE");
            for provider in config.providers {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    provider.id,
                    provider.preset,
                    provider.enabled,
                    provider.secret_ref.as_deref().unwrap_or("none"),
                    provider.base_url.as_deref().unwrap_or("default")
                );
            }
        }
        ProviderCommand::Add(arguments) => {
            update_config_with_retry(&store, |config| add_provider(config, arguments.clone()))?;
            println!("Provider added.");
        }
        ProviderCommand::Remove { id } => {
            if id == "official" {
                bail!("the official ChatGPT provider cannot be removed");
            }
            update_config_with_retry(&store, |config| {
                if config.models.iter().any(|model| model.provider == id) {
                    bail!("provider {id} is still referenced by a model");
                }
                let before = config.providers.len();
                config.providers.retain(|provider| provider.id != id);
                if config.providers.len() == before {
                    bail!("unknown provider: {id}");
                }
                Ok(())
            })?;
            println!("Provider {id} removed; credentials were not deleted.");
        }
    }
    Ok(())
}

fn add_provider(config: &mut RouterConfig, arguments: ProviderAddArgs) -> Result<()> {
    if arguments.id == "official" {
        bail!("provider id `official` is reserved");
    }
    if config
        .providers
        .iter()
        .any(|provider| provider.id == arguments.id)
    {
        bail!("provider id already exists: {}", arguments.id);
    }

    let mut preset = if arguments.preset == "custom-compatible" {
        let base_url = arguments
            .base_url
            .as_deref()
            .context("--base-url is required for custom-compatible")?;
        custom_compatible_preset("custom-compatible", &arguments.id, base_url, false)?
    } else {
        preset_by_id(&arguments.preset)
            .with_context(|| format!("unknown provider preset: {}", arguments.preset))?
    };
    let base_url = if let Some(base_url) = arguments.base_url {
        let validated = custom_compatible_preset("endpoint-check", "Endpoint", base_url, false)?;
        preset.base_url.clone_from(&validated.base_url);
        Some(validated.base_url)
    } else {
        None
    };
    if preset.auth != AuthStyle::None {
        // Validate the requested profile now, but leave the provider unbound
        // until `secret set` stages a generation-specific scoped credential.
        SecretRef::new_generation(&arguments.id, &arguments.secret_profile)?;
    }
    config.providers.push(ProviderConfig {
        id: arguments.id,
        preset: arguments.preset,
        base_url,
        secret_ref: None,
        enabled: !arguments.disabled,
        allow_insecure_http: false,
    });
    config.validate()?;
    Ok(())
}

fn model_command(paths: &RuntimePaths, command: ModelCommand) -> Result<()> {
    let store = paths.config_store();
    match command {
        ModelCommand::List => {
            let config = store.load()?;
            println!("ID\tPROVIDER\tUPSTREAM\tENABLED\tORDER\tDISPLAY NAME");
            for model in &config.models {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    model.id,
                    model.provider,
                    model.upstream_model,
                    model.enabled,
                    model.order,
                    model.display_name
                );
            }
            if !config.hidden_models.is_empty() {
                println!("Hidden catalog ids: {}", config.hidden_models.join(", "));
            }
        }
        ModelCommand::Add(arguments) => {
            update_config_with_retry(&store, |config| add_model(config, arguments.clone()))?;
            println!("Model added.");
        }
        ModelCommand::Enable { id } => {
            set_model_enabled(&store, &id, true)?;
            println!("Model {id} enabled.");
        }
        ModelCommand::Disable { id } => {
            set_model_enabled(&store, &id, false)?;
            println!("Model {id} disabled.");
        }
        ModelCommand::Move { id, position } => {
            validate_catalog_id(&id)?;
            update_config_with_retry(&store, |config| {
                let mut order = Vec::new();
                let mut seen = HashSet::new();
                for entry in &config.catalog_order {
                    if seen.insert(entry.clone()) {
                        order.push(entry.clone());
                    }
                }
                let mut models = config.models.iter().collect::<Vec<_>>();
                models.sort_by_key(|model| model.order);
                for model in models {
                    if seen.insert(model.id.clone()) {
                        order.push(model.id.clone());
                    }
                }
                order.retain(|entry| entry != &id);
                order.insert(position.min(order.len()), id.clone());
                config.catalog_order = order;
                Ok(())
            })?;
            println!("Model {id} moved to picker position {position}.");
        }
        ModelCommand::Hide { id } => {
            validate_catalog_id(&id)?;
            update_config_with_retry(&store, |config| {
                if !config.hidden_models.iter().any(|entry| entry == &id) {
                    config.hidden_models.push(id.clone());
                }
                Ok(())
            })?;
            println!("Model {id} hidden.");
        }
        ModelCommand::Unhide { id } => {
            update_config_with_retry(&store, |config| {
                config.hidden_models.retain(|entry| entry != &id);
                Ok(())
            })?;
            println!("Model {id} is no longer hidden.");
        }
    }
    Ok(())
}

fn add_model(config: &mut RouterConfig, arguments: ModelAddArgs) -> Result<()> {
    if config.models.iter().any(|model| model.id == arguments.id) {
        bail!("model id already exists: {}", arguments.id);
    }
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == arguments.provider)
        .with_context(|| format!("unknown provider: {}", arguments.provider))?;
    if provider.id == "official" {
        bail!("official models are fetched dynamically and cannot be added here");
    }
    let preset = configured_preset(provider)?;
    let upstream_model = arguments
        .upstream_model
        .or_else(|| preset.default_model.clone())
        .unwrap_or_else(|| arguments.id.clone());
    let order = arguments.order.unwrap_or_else(|| {
        config
            .models
            .iter()
            .map(|model| model.order)
            .max()
            .unwrap_or(-10)
            .saturating_add(10)
    });
    config.models.push(ModelConfig {
        id: arguments.id.clone(),
        display_name: arguments
            .display_name
            .unwrap_or_else(|| arguments.id.clone()),
        provider: arguments.provider,
        upstream_model,
        order,
        enabled: !arguments.disabled,
        context_window: arguments
            .context_window
            .or(preset.capabilities.context_window),
        max_output_tokens: arguments
            .max_output_tokens
            .or(preset.capabilities.max_output_tokens),
    });
    if !config.catalog_order.iter().any(|id| id == &arguments.id) {
        config.catalog_order.push(arguments.id);
    }
    config.validate()?;
    Ok(())
}

fn set_model_enabled(store: &ConfigStore, id: &str, enabled: bool) -> Result<()> {
    update_config_with_retry(store, |config| {
        let model = config
            .models
            .iter_mut()
            .find(|model| model.id == id)
            .with_context(|| format!("unknown external model: {id}"))?;
        model.enabled = enabled;
        Ok(())
    })
}

const CONFIG_WRITE_ATTEMPTS: usize = 4;

fn update_config_with_retry<F>(store: &ConfigStore, mut update: F) -> Result<()>
where
    F: FnMut(&mut RouterConfig) -> Result<()>,
{
    for attempt in 0..CONFIG_WRITE_ATTEMPTS {
        let (mut config, revision) = store.load_with_revision()?;
        update(&mut config)?;
        match store.save_if_revision(&config, &revision) {
            Ok(outcome) => {
                report_config_commit(&outcome);
                return Ok(());
            }
            Err(StorageError::Conflict(_)) if attempt + 1 < CONFIG_WRITE_ATTEMPTS => {}
            Err(StorageError::Conflict(message)) => {
                bail!(
                    "configuration kept changing concurrently after {CONFIG_WRITE_ATTEMPTS} attempts: {message}"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("bounded config update loop always returns")
}

fn validate_catalog_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        bail!("model id contains unsupported characters: {id}");
    }
    Ok(())
}

fn configured_preset(provider: &ProviderConfig) -> Result<ProviderPreset> {
    let mut preset = if provider.preset == "custom-compatible" {
        let base_url = provider
            .base_url
            .as_deref()
            .context("custom-compatible provider has no base_url")?;
        custom_compatible_preset(
            "custom-compatible",
            &provider.id,
            base_url,
            provider.allow_insecure_http,
        )?
    } else {
        preset_by_id(&provider.preset)
            .with_context(|| format!("unknown provider preset: {}", provider.preset))?
    };
    if let Some(base_url) = &provider.base_url {
        let validated = custom_compatible_preset(
            "endpoint-check",
            "Endpoint",
            base_url,
            provider.allow_insecure_http,
        )?;
        preset.base_url = validated.base_url;
    }
    Ok(preset)
}

fn secret_command(paths: &RuntimePaths, command: SecretCommand) -> Result<()> {
    let store = paths.config_store();
    let credentials = OsCredentialStore::scoped(store.instance_id()?);
    match command {
        SecretCommand::Set { provider, profile } => {
            let config = store.load()?;
            config
                .providers
                .iter()
                .find(|entry| entry.id == provider)
                .with_context(|| format!("unknown provider: {provider}"))?;
            // Validate both components before asking for a secret without ever
            // manufacturing a legacy, generation-less production reference.
            SecretRef::new_generation(&provider, &profile)?;
            let first = rpassword::prompt_password("API key (input hidden): ")?;
            if first.is_empty() {
                bail!("credential cannot be empty");
            }
            let second = rpassword::prompt_password("Confirm API key (input hidden): ")?;
            if first != second {
                bail!("credentials did not match");
            }
            let reference =
                rotate_configured_secret(&store, &credentials, &provider, &profile, &first)?;
            drop(first);
            drop(second);
            println!(
                "Credential stored for {}; no credential value was written to config or output. Superseded vault generations are retained so an already-running router can keep using its startup snapshot.",
                reference.account()
            );
        }
        SecretCommand::Delete { provider, profile } => {
            let reference = remove_configured_secret(&store, &provider, &profile)?;
            println!(
                "Credential reference {} was removed from config. Its vault entry was retained so an already-running router can keep using its startup snapshot; stop or restart router processes before any future explicit garbage collection.",
                reference.account()
            );
        }
    }
    Ok(())
}

trait RevisionedConfigStore {
    fn load_with_revision(&self) -> cmr_storage::Result<(RouterConfig, ConfigRevision)>;

    fn save_if_revision(
        &self,
        config: &RouterConfig,
        expected: &ConfigRevision,
    ) -> cmr_storage::Result<ConfigCommitOutcome>;
}

impl RevisionedConfigStore for ConfigStore {
    fn load_with_revision(&self) -> cmr_storage::Result<(RouterConfig, ConfigRevision)> {
        ConfigStore::load_with_revision(self)
    }

    fn save_if_revision(
        &self,
        config: &RouterConfig,
        expected: &ConfigRevision,
    ) -> cmr_storage::Result<ConfigCommitOutcome> {
        ConfigStore::save_if_revision(self, config, expected)
    }
}

fn report_config_commit(outcome: &ConfigCommitOutcome) {
    if let Some(warning) = outcome.maintenance_warning() {
        eprintln!("Configuration was committed, but local maintenance is pending: {warning}");
    }
}

fn rotate_configured_secret<S: RevisionedConfigStore + ?Sized>(
    store: &S,
    credentials: &dyn CredentialStore,
    provider: &str,
    profile: &str,
    secret: &str,
) -> Result<SecretRef> {
    let (mut config, revision) = store.load_with_revision()?;
    let provider_index = config
        .providers
        .iter()
        .position(|entry| entry.id == provider)
        .with_context(|| format!("unknown provider: {provider}"))?;
    let old_reference = config.providers[provider_index]
        .secret_ref
        .as_deref()
        .map(SecretRef::parse)
        .transpose()?;
    if let Some(reference) = &old_reference {
        reference.validate_provider(provider)?;
    }

    let staged = credentials.stage_generation(provider, profile, secret)?;
    config.providers[provider_index].secret_ref = Some(staged.to_string());
    match store.save_if_revision(&config, &revision) {
        Ok(outcome) => report_config_commit(&outcome),
        Err(commit_error) => {
            // ConfigStore's commit contract returns Err only before publishing the
            // live replacement. Post-commit maintenance issues are carried by a
            // successful ConfigCommitOutcome, so this generation is safe to undo.
            if let Err(cleanup_error) = credentials.delete(&staged) {
                bail!(
                    "configuration commit failed ({commit_error}); staged credential cleanup also failed ({cleanup_error})"
                );
            }
            return Err(commit_error.into());
        }
    }

    // Router processes intentionally hold an immutable startup snapshot of the
    // configured secret reference. Keep the superseded generation readable
    // until a future explicit GC can prove every old process has stopped.
    Ok(staged)
}

fn remove_configured_secret(
    store: &ConfigStore,
    provider: &str,
    profile: &str,
) -> Result<SecretRef> {
    let (mut config, revision) = store.load_with_revision()?;
    let provider_index = config
        .providers
        .iter()
        .position(|entry| entry.id == provider)
        .with_context(|| format!("unknown provider: {provider}"))?;
    let reference = config.providers[provider_index]
        .secret_ref
        .as_deref()
        .context("provider has no configured credential reference")
        .and_then(|value| SecretRef::parse(value).map_err(Into::into))?;
    reference.validate_provider(provider)?;
    if reference.profile() != profile {
        bail!(
            "provider {provider} currently references profile {}; requested profile was {profile}",
            reference.profile()
        );
    }

    config.providers[provider_index].secret_ref = None;
    report_config_commit(&store.save_if_revision(&config, &revision)?);
    // Do not remove the vault entry here. A currently running router may still
    // hold this exact reference in its immutable startup configuration.
    Ok(reference)
}

fn codex_command(paths: &RuntimePaths, command: &CodexCommand) -> Result<()> {
    let config = paths
        .config_store()
        .load()
        .with_context(|| format!("load {}", paths.config.display()))?;
    let integration = paths.codex(&config)?;
    match command {
        CodexCommand::Install => {
            let backup = integration.install()?;
            println!(
                "Codex user config updated: {}",
                integration.config_path().display()
            );
            println!("Recovery backup retained: {}", backup.display());
        }
        CodexCommand::Uninstall => {
            let report = integration.uninstall()?;
            if let Some(backup) = report.backup_path {
                println!(
                    "Restored {} managed values; preserved {} later user changes.",
                    report.restored, report.preserved_user_changes
                );
                println!("Recovery backup retained: {}", backup.display());
            } else {
                println!("Codex integration is not installed.");
            }
        }
        CodexCommand::Restore => {
            let report = integration.restore()?;
            if report.restored {
                println!(
                    "Codex config reset to its pre-install state: {}",
                    integration.config_path().display()
                );
                if let Some(backup) = report.backup_path {
                    println!("Recovery backup retained: {}", backup.display());
                }
            } else {
                println!("Codex integration is not installed.");
            }
        }
        CodexCommand::Status => {
            let status = integration.status()?;
            println!(
                "{}",
                match status {
                    IntegrationStatus::NotInstalled => "not-installed",
                    IntegrationStatus::Installed => "installed",
                    IntegrationStatus::Drifted => "drifted",
                }
            );
            println!("Config: {}", integration.config_path().display());
            println!("State: {}", integration.sidecar_path().display());
        }
    }
    Ok(())
}

fn service_command(paths: &RuntimePaths, command: &ServiceCommand) -> Result<()> {
    let mut manager = ServiceManager::discover(&paths.config, &paths.state_db, SystemRunner)?;
    match command {
        ServiceCommand::Install => {
            manager.install()?;
            println!("installed");
            println!("Definition: {}", manager.definition_path().display());
        }
        ServiceCommand::Uninstall => {
            if manager.uninstall()? {
                println!("uninstalled");
            } else {
                println!("not-installed");
            }
            println!("Definition: {}", manager.definition_path().display());
        }
        ServiceCommand::Status => {
            let status = manager.status()?;
            println!(
                "{}",
                match status {
                    ServiceStatus::Installed => "installed",
                    ServiceStatus::NotInstalled => "not-installed",
                }
            );
            println!("Definition: {}", manager.definition_path().display());
        }
    }
    Ok(())
}

async fn doctor(paths: &RuntimePaths) -> Result<()> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if !paths.config.exists() {
        warnings.push(format!(
            "router config does not exist; defaults would be used: {}",
            paths.config.display()
        ));
    }
    let config_store = paths.config_store();
    let config = config_store
        .load()
        .with_context(|| format!("load {}", paths.config.display()))?;
    if let Err(error) = config.validate() {
        errors.push(error.to_string());
    } else {
        println!("OK   configuration is valid and loopback-only");
    }

    let compatibility_message = "Desktop and phone Remote picker support has no published paired-device acceptance result for this release";
    match config.compatibility_policy {
        CompatibilityPolicy::Warn => warnings.push(compatibility_message.into()),
        CompatibilityPolicy::Strict => errors.push(format!(
            "strict compatibility policy: {compatibility_message}"
        )),
    }

    let credentials = OsCredentialStore::scoped(config_store.instance_id()?);
    check_credentials(&config, &credentials, &mut errors)?;
    if errors.is_empty() {
        println!("OK   credentials referenced by enabled external models are available");
    }

    match paths.codex(&config)?.status() {
        Ok(IntegrationStatus::Installed) => {
            println!("OK   Codex user config integration is installed");
        }
        Ok(IntegrationStatus::NotInstalled) => {
            warnings.push("Codex user config integration is not installed".into());
        }
        Ok(IntegrationStatus::Drifted) => {
            warnings.push("Codex user config integration has drifted".into());
        }
        Err(error) => errors.push(format!("Codex integration: {error:#}")),
    }

    let expected_models = enabled_external_models(&config);
    match fetch_health(&config).await {
        Ok(health) => check_health_catalog(&health, &expected_models, &mut errors, &mut warnings),
        Err(error) => warnings.push(format!("router is not reachable: {error:#}")),
    }

    for warning in warnings {
        eprintln!("WARN {warning}");
    }
    if errors.is_empty() {
        println!("Doctor completed without errors.");
        Ok(())
    } else {
        for error in &errors {
            eprintln!("FAIL {error}");
        }
        bail!("doctor found {} error(s)", errors.len())
    }
}

fn check_credentials(
    config: &RouterConfig,
    credentials: &dyn CredentialStore,
    errors: &mut Vec<String>,
) -> Result<()> {
    let hidden = config
        .hidden_models
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let active_provider_ids = config
        .models
        .iter()
        .filter(|model| model.enabled && !hidden.contains(model.id.as_str()))
        .map(|model| model.provider.as_str())
        .collect::<HashSet<_>>();
    for provider in config
        .providers
        .iter()
        .filter(|provider| provider.enabled && active_provider_ids.contains(provider.id.as_str()))
    {
        let preset = match configured_preset(provider) {
            Ok(preset) => preset,
            Err(error) => {
                errors.push(format!("provider {}: {error:#}", provider.id));
                continue;
            }
        };
        if preset.auth == AuthStyle::None {
            continue;
        }
        let Some(reference) = provider.secret_ref.as_deref() else {
            errors.push(format!(
                "provider {} has no credential reference",
                provider.id
            ));
            continue;
        };
        let reference = match SecretRef::parse(reference) {
            Ok(reference) => reference,
            Err(error) => {
                errors.push(format!("provider {}: {error}", provider.id));
                continue;
            }
        };
        if credentials.get(&reference)?.is_none() {
            errors.push(format!(
                "provider {} credential {} is missing from the OS vault",
                provider.id,
                reference.account()
            ));
        }
    }
    Ok(())
}

fn enabled_external_models(config: &RouterConfig) -> Vec<String> {
    let enabled_providers = config
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.id.as_str())
        .collect::<HashSet<_>>();
    let hidden = config
        .hidden_models
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut models = config
        .models
        .iter()
        .filter(|model| {
            model.enabled
                && enabled_providers.contains(model.provider.as_str())
                && !hidden.contains(model.id.as_str())
        })
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    models.sort();
    models
}

fn health_model_ids(health: &Value, field: &str) -> Option<Vec<String>> {
    let mut models = health
        .get(field)?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    models.sort();
    models.dedup();
    Some(models)
}

fn check_health_catalog(
    health: &Value,
    expected_models: &[String],
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let routable = health_model_ids(health, "routable_external_models");
    let injected = health_model_ids(health, "external_models");
    let catalog_cached = health
        .get("official_catalog_cached")
        .and_then(Value::as_bool);
    if routable.is_none() {
        errors.push("router health is missing a valid routable_external_models array".into());
    } else if routable.as_deref() != Some(expected_models) {
        errors.push(
            "router health routable_external_models does not match config; restart the router"
                .into(),
        );
    } else if catalog_cached.is_none() {
        errors.push("router health is missing a valid official_catalog_cached boolean".into());
    } else if catalog_cached != Some(true) {
        warnings.push(
            "router is healthy, but no authorized official catalog is cached; open the model picker before validating external model injection"
                .into(),
        );
    } else if injected.is_none() {
        errors.push("router health is missing a valid external_models array".into());
    } else if !expected_models.is_empty() && injected.as_ref().is_some_and(Vec::is_empty) {
        errors.push(
            "configured external models are routable, but none were injected into the authorized picker (check picker capacity and account authorization)"
                .into(),
        );
    } else if injected.as_ref().is_some_and(|models| {
        models
            .iter()
            .any(|model| expected_models.binary_search(model).is_err())
    }) {
        errors.push(
            "router health external_models contains an ID that is not routable from config".into(),
        );
    } else {
        println!(
            "OK   loopback router health matches routable config and reports the authorized picker injection"
        );
    }
}

async fn fetch_health(config: &RouterConfig) -> Result<Value> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()?;
    let url = health_url(config)?;
    let response = client.get(url).send().await?.error_for_status()?;
    response.json().await.context("parse health response")
}

fn health_url(config: &RouterConfig) -> Result<String> {
    let ip: IpAddr = config
        .server
        .host
        .parse()
        .with_context(|| format!("invalid router host `{}`", config.server.host))?;
    Ok(format!(
        "http://{}/health",
        SocketAddr::new(ip, config.server.port)
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use cmr_storage::{MemoryCredentialStore, StorageError};
    use tempfile::{TempDir, tempdir};

    struct ConflictingCredentialStore {
        inner: MemoryCredentialStore,
        config: ConfigStore,
        staged: Mutex<Option<SecretRef>>,
    }

    impl CredentialStore for ConflictingCredentialStore {
        fn set(&self, reference: &SecretRef, secret: &str) -> cmr_storage::Result<()> {
            self.inner.set(reference, secret)
        }

        fn get(&self, reference: &SecretRef) -> cmr_storage::Result<Option<String>> {
            self.inner.get(reference)
        }

        fn delete(&self, reference: &SecretRef) -> cmr_storage::Result<()> {
            self.inner.delete(reference)
        }

        fn stage_generation(
            &self,
            provider: &str,
            profile: &str,
            secret: &str,
        ) -> cmr_storage::Result<SecretRef> {
            let reference = SecretRef::new_generation(provider, profile)?;
            self.inner.set(&reference, secret)?;
            *self.staged.lock().map_err(|_| StorageError::Poisoned)? = Some(reference.clone());
            let mut concurrent = self.config.load()?;
            concurrent
                .hidden_models
                .push("concurrent-update".to_owned());
            self.config.save(&concurrent)?;
            Ok(reference)
        }
    }

    fn zhipu_config_store() -> (TempDir, ConfigStore) {
        let directory = tempdir().expect("temporary directory");
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let mut config = RouterConfig::default();
        add_provider(
            &mut config,
            ProviderAddArgs {
                id: "zhipu".into(),
                preset: "zhipu".into(),
                base_url: None,
                secret_profile: "default".into(),
                disabled: false,
            },
        )
        .expect("provider");
        store.save(&config).expect("seed config");
        (directory, store)
    }

    #[test]
    fn zhipu_model_inherits_preset_limits() {
        let mut config = RouterConfig::default();
        add_provider(
            &mut config,
            ProviderAddArgs {
                id: "zhipu".into(),
                preset: "zhipu".into(),
                base_url: None,
                secret_profile: "default".into(),
                disabled: false,
            },
        )
        .expect("provider");
        add_model(
            &mut config,
            ModelAddArgs {
                id: "glm-5.2".into(),
                provider: "zhipu".into(),
                upstream_model: None,
                display_name: None,
                order: None,
                context_window: None,
                max_output_tokens: None,
                disabled: false,
            },
        )
        .expect("model");
        let model = &config.models[0];
        assert_eq!(model.upstream_model, "glm-5.2");
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(131_072));
        assert_eq!(config.catalog_order, ["glm-5.2"]);
    }

    #[test]
    fn credential_check_never_requires_value_in_config() {
        let mut config = RouterConfig::default();
        add_provider(
            &mut config,
            ProviderAddArgs {
                id: "zhipu".into(),
                preset: "zhipu".into(),
                base_url: None,
                secret_profile: "default".into(),
                disabled: false,
            },
        )
        .expect("provider");
        add_model(
            &mut config,
            ModelAddArgs {
                id: "glm-5.2".into(),
                provider: "zhipu".into(),
                upstream_model: None,
                display_name: None,
                order: None,
                context_window: None,
                max_output_tokens: None,
                disabled: false,
            },
        )
        .expect("model");
        let credentials = MemoryCredentialStore::default();
        let reference = SecretRef::new_generation("zhipu", "default").expect("reference");
        config.providers[1].secret_ref = Some(reference.to_string());
        credentials
            .set(&reference, "never-serialize-this")
            .expect("set");
        let mut errors = Vec::new();
        check_credentials(&config, &credentials, &mut errors).expect("check");
        assert!(errors.is_empty());
        let encoded = toml::to_string(&config).expect("serialize");
        assert!(!encoded.contains("never-serialize-this"));
    }

    #[test]
    fn published_external_models_exclude_hidden_and_disabled_entries() {
        let mut config = RouterConfig::default();
        for (provider, disabled) in [("published", false), ("off", true)] {
            add_provider(
                &mut config,
                ProviderAddArgs {
                    id: provider.into(),
                    preset: "zhipu".into(),
                    base_url: None,
                    secret_profile: "default".into(),
                    disabled,
                },
            )
            .expect("provider");
        }
        for (id, provider, disabled) in [
            ("visible", "published", false),
            ("hidden", "published", false),
            ("model-off", "published", true),
            ("provider-off", "off", false),
        ] {
            add_model(
                &mut config,
                ModelAddArgs {
                    id: id.into(),
                    provider: provider.into(),
                    upstream_model: None,
                    display_name: None,
                    order: None,
                    context_window: None,
                    max_output_tokens: None,
                    disabled,
                },
            )
            .expect("model");
        }
        config.hidden_models.push("hidden".into());

        assert_eq!(enabled_external_models(&config), ["visible"]);
    }

    #[test]
    fn health_catalog_fields_keep_routable_and_injected_models_distinct() {
        let health = serde_json::json!({
            "external_models": ["visible-b"],
            "routable_external_models": ["visible-b", "visible-a"],
            "official_catalog_cached": true
        });
        assert_eq!(
            health_model_ids(&health, "routable_external_models"),
            Some(vec!["visible-a".to_owned(), "visible-b".to_owned()])
        );
        assert_eq!(
            health_model_ids(&health, "external_models"),
            Some(vec!["visible-b".to_owned()])
        );
    }

    #[test]
    fn health_url_brackets_ipv6() {
        let mut config = RouterConfig::default();
        config.server.host = "::1".into();
        config.server.port = 15722;
        assert_eq!(
            health_url(&config).expect("health URL"),
            "http://[::1]:15722/health"
        );
    }

    #[test]
    fn credential_rotation_keeps_old_generation_for_running_router() {
        let (_directory, store) = zhipu_config_store();
        let credentials = MemoryCredentialStore::default();
        let old = SecretRef::new_generation("zhipu", "default").expect("old generation");
        let mut config = store.load().expect("load config");
        config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "zhipu")
            .unwrap()
            .secret_ref = Some(old.to_string());
        store.save(&config).expect("save old reference");
        credentials
            .set(&old, "old-secret")
            .expect("store old secret");

        let new = rotate_configured_secret(&store, &credentials, "zhipu", "default", "new-secret")
            .expect("rotate credential");

        assert!(new.generation().is_some());
        assert_ne!(new, old);
        assert_eq!(
            credentials.get(&new).expect("read new"),
            Some("new-secret".into())
        );
        assert_eq!(
            credentials.get(&old).expect("read old"),
            Some("old-secret".into())
        );
        let saved = store.load().expect("reload config");
        assert_eq!(
            saved
                .providers
                .iter()
                .find(|provider| provider.id == "zhipu")
                .unwrap()
                .secret_ref
                .as_deref(),
            Some(new.account())
        );
        assert!(!toml::to_string(&saved).unwrap().contains("new-secret"));
    }

    #[test]
    fn credential_rotation_rolls_back_staged_generation_on_config_conflict() {
        let (_directory, store) = zhipu_config_store();
        let credentials = ConflictingCredentialStore {
            inner: MemoryCredentialStore::default(),
            config: store.clone(),
            staged: Mutex::new(None),
        };

        let error =
            rotate_configured_secret(&store, &credentials, "zhipu", "default", "staged-secret")
                .expect_err("CAS conflict must fail rotation");

        assert!(error.to_string().contains("conflict"));
        let staged = credentials
            .staged
            .lock()
            .unwrap()
            .clone()
            .expect("staged generation");
        assert_eq!(credentials.get(&staged).expect("read staged"), None);
        let saved = store.load().expect("reload concurrent config");
        assert!(
            saved
                .hidden_models
                .contains(&"concurrent-update".to_owned())
        );
        assert_eq!(
            saved
                .providers
                .iter()
                .find(|provider| provider.id == "zhipu")
                .unwrap()
                .secret_ref
                .as_deref(),
            None
        );
    }

    #[test]
    fn config_update_retries_a_concurrent_writer_without_losing_either_change() {
        let directory = tempdir().expect("temporary directory");
        let store = ConfigStore::new(directory.path().join("router.toml"));
        store.save(&RouterConfig::default()).expect("seed config");
        let mut inject_conflict = true;

        update_config_with_retry(&store, |config| {
            if inject_conflict {
                inject_conflict = false;
                let mut concurrent = store.load()?;
                concurrent.hidden_models.push("concurrent".into());
                store.save(&concurrent)?;
            }
            config.hidden_models.push("requested".into());
            Ok(())
        })
        .expect("retry update");

        let saved = store.load().expect("load merged config");
        assert!(saved.hidden_models.contains(&"concurrent".to_owned()));
        assert!(saved.hidden_models.contains(&"requested".to_owned()));
    }

    #[test]
    fn credential_delete_unbinds_config_but_retains_vault_entries() {
        let (_directory, store) = zhipu_config_store();
        let credentials = MemoryCredentialStore::default();
        let generated = SecretRef::new_generation("zhipu", "default").expect("generated reference");
        let mut config = store.load().expect("load config");
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "zhipu")
            .unwrap();
        provider.secret_ref = Some(generated.to_string());
        store.save(&config).expect("save generated reference");
        credentials
            .set(&generated, "generated-secret")
            .expect("store generated secret");

        let removed = remove_configured_secret(&store, "zhipu", "default")
            .expect("remove generated credential");
        assert_eq!(removed, generated);
        assert_eq!(
            credentials.get(&generated).expect("read generated"),
            Some("generated-secret".into())
        );

        let legacy = SecretRef::new("zhipu", "default").expect("legacy reference");
        let mut config = store.load().expect("reload config");
        config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "zhipu")
            .unwrap()
            .secret_ref = Some(legacy.to_string());
        store.save(&config).expect("save legacy reference");
        credentials
            .set(&legacy, "legacy-secret")
            .expect("store legacy secret");

        let removed =
            remove_configured_secret(&store, "zhipu", "default").expect("remove legacy reference");
        assert_eq!(removed, legacy);
        assert_eq!(
            credentials.get(&legacy).expect("read retained legacy"),
            Some("legacy-secret".into())
        );
        assert!(
            store
                .load()
                .unwrap()
                .providers
                .iter()
                .find(|provider| provider.id == "zhipu")
                .unwrap()
                .secret_ref
                .is_none()
        );
    }
}
