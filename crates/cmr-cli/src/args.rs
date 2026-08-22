use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Configure and run the local Codex Model Router.
#[derive(Clone, Debug, Parser)]
#[command(name = "cmr", version, about)]
pub struct Cli {
    /// Router configuration file. Defaults to the platform user config directory.
    #[arg(long, env = "CMR_CONFIG")]
    pub config: Option<PathBuf>,

    /// Session database. Defaults to the platform user data directory.
    #[arg(long, env = "CMR_STATE_DB")]
    pub state_db: Option<PathBuf>,

    /// User-level Codex config. Intended for portable installs and tests.
    #[arg(long, env = "CMR_CODEX_CONFIG")]
    pub codex_config: Option<PathBuf>,

    /// Integration state sidecar. Defaults beside the Codex config.
    #[arg(long, env = "CMR_CODEX_SIDECAR")]
    pub codex_sidecar: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Start the loopback Responses router.
    Serve,
    /// Validate configuration, credentials, integration, and router health.
    Doctor,
    /// List built-in provider presets without secret values.
    Presets {
        /// Emit JSON instead of a compact table.
        #[arg(long)]
        json: bool,
    },
    /// Manage the non-secret router configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage provider endpoints and credential references.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Manage picker-visible models, ordering, and visibility.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Manage API keys in the operating-system credential vault.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Safely merge or remove the user-level Codex integration.
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
    /// Install and manage the router as a per-user background service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a default config, refusing to overwrite an existing file.
    Init,
    /// Print the resolved router config path.
    Path,
    /// Print the non-secret router configuration.
    Show,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProviderCommand {
    /// List configured providers.
    List,
    /// Add a provider from a built-in preset or a compatible endpoint.
    Add(ProviderAddArgs),
    /// Remove an unused provider.
    Remove {
        /// Configured provider id.
        id: String,
    },
}

#[derive(Clone, Debug, Args)]
pub struct ProviderAddArgs {
    /// Stable id used by model mappings.
    pub id: String,

    /// Built-in preset id, or `custom-compatible`.
    #[arg(long)]
    pub preset: String,

    /// HTTPS endpoint override; loopback HTTP is also accepted.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Vault profile name used to construct the non-secret provider/profile reference.
    #[arg(long, default_value = "default")]
    pub secret_profile: String,

    /// Add the provider disabled.
    #[arg(long)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ModelCommand {
    /// List configured external models and hidden catalog ids.
    List,
    /// Add one external model mapping.
    Add(ModelAddArgs),
    /// Enable one configured external model.
    Enable { id: String },
    /// Disable one configured external model.
    Disable { id: String },
    /// Move any external or known official model id to a zero-based picker position.
    Move { id: String, position: usize },
    /// Hide an external or official model id from the merged picker.
    Hide { id: String },
    /// Remove a model id from the hidden set.
    Unhide { id: String },
}

#[derive(Clone, Debug, Args)]
pub struct ModelAddArgs {
    /// Stable model id exposed to Codex.
    pub id: String,

    /// Configured provider id.
    #[arg(long)]
    pub provider: String,

    /// Model name sent upstream. Defaults to the preset model, then the public id.
    #[arg(long)]
    pub upstream_model: Option<String>,

    /// Picker label. Defaults to the public model id.
    #[arg(long)]
    pub display_name: Option<String>,

    /// Fallback sort rank for entries absent from `catalog_order`.
    #[arg(long)]
    pub order: Option<i32>,

    /// Context-window override.
    #[arg(long)]
    pub context_window: Option<u64>,

    /// Maximum output-token override.
    #[arg(long)]
    pub max_output_tokens: Option<u64>,

    /// Add the model disabled.
    #[arg(long)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum SecretCommand {
    /// Prompt invisibly for a key and store it in the OS vault.
    Set {
        /// Configured provider id.
        provider: String,
        /// Vault profile name.
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Delete a provider/profile key from the OS vault.
    Delete {
        /// Configured provider id.
        provider: String,
        /// Vault profile name.
        #[arg(long, default_value = "default")]
        profile: String,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum CodexCommand {
    /// Back up and merge the three user-level router settings.
    Install,
    /// Restore only managed keys that still have values installed by this tool.
    Uninstall,
    /// Reset the Codex config to its exact pre-install state from the backup.
    Restore,
    /// Report whether the integration is installed or has drifted.
    Status,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install and start the per-user router service.
    Install,
    /// Stop and remove the per-user router service.
    Uninstall,
    /// Report whether the per-user router service is registered.
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_exposes_required_model_operations() {
        let parsed = Cli::try_parse_from([
            "cmr",
            "--config",
            "router.toml",
            "model",
            "move",
            "glm-5.2",
            "0",
        ])
        .expect("parse model move");
        assert!(matches!(
            parsed.command,
            Command::Model {
                command: ModelCommand::Move { position: 0, .. }
            }
        ));
    }

    #[test]
    fn secret_set_has_no_value_argument() {
        let parsed =
            Cli::try_parse_from(["cmr", "secret", "set", "zhipu"]).expect("parse secret set");
        assert!(matches!(
            parsed.command,
            Command::Secret {
                command: SecretCommand::Set { .. }
            }
        ));
    }

    #[test]
    fn parser_exposes_service_lifecycle_commands() {
        for (name, expected) in [
            ("install", "install"),
            ("uninstall", "uninstall"),
            ("status", "status"),
        ] {
            let parsed = Cli::try_parse_from(["cmr", "service", name]).expect("parse service");
            let Command::Service { command } = parsed.command else {
                panic!("expected service command");
            };
            let actual = match command {
                ServiceCommand::Install => "install",
                ServiceCommand::Uninstall => "uninstall",
                ServiceCommand::Status => "status",
            };
            assert_eq!(actual, expected);
        }
    }
}
