//! Native desktop manager for `ModelRelay`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cmr_cli::{
    codex::{CodexIntegration, IntegrationStatus},
    service::{
        CommandRunner, LegacyWindowsTaskBackup, ServiceManager, ServiceStatus, SystemRunner,
    },
};
use cmr_providers::{
    AuthStyle, ProtocolFamily, ProviderCapabilities, ProviderPreset, built_in_presets,
    custom_compatible_preset, preset_by_id,
};
use cmr_storage::{
    AppPaths, ConfigCommitOutcome, ConfigRevision, ConfigStore, CredentialStore, ModelConfig,
    OsCredentialStore, ProviderConfig, RouterConfig, SecretRef, StorageError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use sysinfo::{Pid, System};
use tauri::{Manager, RunEvent, State};
use zeroize::Zeroize;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(450);
const START_ATTEMPTS: usize = 30;
const START_RETRY_DELAY: Duration = Duration::from_millis(100);
const HEALTH_RESPONSE_LIMIT: u64 = 64 * 1024;
const CONFIG_UPDATE_ATTEMPTS: usize = 4;
const MAX_API_KEY_BYTES: usize = 16 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_LABEL_BYTES: usize = 256;
const MAX_BASE_URL_BYTES: usize = 2_048;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceState {
    status: &'static str,
    management: &'static str,
    manageable: bool,
    bind_address: String,
    pid: Option<u32>,
    uptime_seconds: Option<u64>,
    detail: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSummary {
    id: String,
    label: String,
    protocol: String,
    enabled: bool,
    credential_status: &'static str,
    model_count: usize,
    official: bool,
    base_url: Option<String>,
    secret_profile: Option<String>,
    allow_insecure_http: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSummary {
    id: String,
    label: String,
    provider_id: String,
    provider_label: String,
    official: bool,
    visible: bool,
    capabilities: Vec<String>,
    upstream_model: String,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    enabled: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCompatibility {
    state: &'static str,
    ios: &'static str,
    android: &'static str,
    message: String,
    last_checked_at: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardState {
    service: ServiceState,
    providers: Vec<ProviderSummary>,
    models: Vec<ModelSummary>,
    remote: RemoteCompatibility,
    catalog_version: String,
    config_path: String,
    codex_integration: CodexIntegrationState,
}

/// Codex integration status surfaced to the UI so the "restore default Codex
/// config" button can be enabled only when an integration is actually present.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexIntegrationState {
    installed: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSetupPreset {
    id: String,
    label: String,
    default_base_url: String,
    default_model: String,
    requires_api_key: bool,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSetupOptions {
    presets: Vec<ProviderSetupPreset>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddProviderWithModelInput {
    provider_id: String,
    preset_id: String,
    base_url: String,
    api_key: String,
    secret_profile: String,
    model_id: String,
    upstream_model: String,
    display_name: String,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    enabled: bool,
    #[serde(default)]
    allow_insecure_http: bool,
}

impl AddProviderWithModelInput {
    fn clear_secret(&mut self) {
        self.api_key.zeroize();
    }
}

impl Drop for AddProviderWithModelInput {
    fn drop(&mut self) {
        self.clear_secret();
    }
}

/// Editable fields of an existing external model. `None` fields are left
/// unchanged. The model `id` and its `provider` binding are intentionally not
/// editable here: `id` is the stable key, and reassigning a model across
/// providers is a structural change outside this lightweight edit flow.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateModelInput {
    model_id: String,
    display_name: Option<String>,
    upstream_model: Option<String>,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    enabled: Option<bool>,
}

/// Editable fields of an existing provider. `api_key`, when provided, rotates
/// the credential through the same stage/commit/rollback contract as the
/// add flow; an empty string is treated as "do not rotate".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateProviderInput {
    provider_id: String,
    base_url: Option<String>,
    enabled: Option<bool>,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_secret_profile")]
    secret_profile: String,
    #[serde(default)]
    allow_insecure_http: Option<bool>,
}

fn default_secret_profile() -> String {
    "default".to_owned()
}

/// An empty credential profile means "the default slot"; the credential vault
/// itself rejects empty reference parts, so blank form input must never reach
/// `stage_generation` unchanged.
fn normalize_secret_profile(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default_secret_profile()
    } else {
        trimmed.to_owned()
    }
}

impl UpdateProviderInput {
    fn rotating_secret(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn clear_secret(&mut self) {
        self.api_key.zeroize();
    }
}

impl Drop for UpdateProviderInput {
    fn drop(&mut self) {
        self.clear_secret();
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddProviderWithModelResult {
    providers: Vec<ProviderSummary>,
    models: Vec<ModelSummary>,
    requires_restart: bool,
}

/// Outcome of resetting the Codex config to its pre-router-install state.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RestoreCodexResult {
    restored: bool,
    config_path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct LocalSetupResult {
    codex_config_path: String,
    recovery_backup_path: Option<String>,
    service_definition_path: String,
    bind_address: String,
    external_models: Vec<String>,
    integration_installed: bool,
    service_installed: bool,
    healthy: bool,
    restart_chatgpt_required: bool,
    picker_pending: bool,
    partial: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct LocalSetupFailure {
    stage: &'static str,
    message: String,
    integration_installed: bool,
    service_installed: bool,
    healthy: bool,
    restart_chatgpt_required: bool,
    partial: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HealthDocument {
    version: String,
    external_models: Vec<String>,
    routable_external_models: Vec<String>,
    official_catalog_cached: bool,
}

enum HealthProbe {
    Healthy(HealthDocument),
    Stopped,
    Occupied,
}

struct ServiceSnapshot {
    state: ServiceState,
    health: Option<HealthDocument>,
}

#[derive(Default)]
struct ManagedService {
    child: Option<Child>,
    started_at: Option<Instant>,
}

impl ManagedService {
    fn reap(&mut self) {
        let exited = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .flatten()
            .is_some();
        if exited {
            self.child = None;
            self.started_at = None;
        }
    }

    fn stop_owned(&mut self) -> Result<bool, String> {
        let Some(mut child) = self.child.take() else {
            self.started_at = None;
            return Ok(false);
        };
        if child.kill().is_err() {
            if let Ok(Some(_)) = child.try_wait() {
                let _ = child.wait();
                self.started_at = None;
                return Ok(true);
            }
            self.child = Some(child);
            return Err("无法停止由此窗口启动的 cmr 进程。".to_owned());
        }
        let wait_result = child.wait();
        self.started_at = None;
        wait_result.map_err(|_| "等待 cmr 进程退出失败。".to_owned())?;
        Ok(true)
    }
}

impl Drop for ManagedService {
    fn drop(&mut self) {
        let _ = self.stop_owned();
    }
}

#[derive(Clone)]
struct DesktopState {
    config: ConfigStore,
    service: Arc<Mutex<ManagedService>>,
    remote: Arc<Mutex<Option<RemoteCompatibility>>>,
    setup: Arc<Mutex<()>>,
}

impl DesktopState {
    fn new(config: ConfigStore) -> Self {
        Self {
            config,
            service: Arc::new(Mutex::new(ManagedService::default())),
            remote: Arc::new(Mutex::new(None)),
            setup: Arc::new(Mutex::new(())),
        }
    }

    fn load_config(&self) -> Result<RouterConfig, String> {
        self.config.load().map_err(|_| {
            format!(
                "无法读取路由配置 {}；请运行 cmr doctor 检查非密钥配置。",
                self.config.path().display()
            )
        })
    }

    fn update_config<F>(&self, mut update: F) -> Result<RouterConfig, String>
    where
        F: FnMut(&mut RouterConfig) -> Result<(), String>,
    {
        for attempt in 0..CONFIG_UPDATE_ATTEMPTS {
            let (mut config, revision) = self.config.load_with_revision().map_err(|_| {
                format!(
                    "无法读取路由配置 {}；请运行 cmr doctor 检查非密钥配置。",
                    self.config.path().display()
                )
            })?;
            update(&mut config)?;
            match self.config.save_if_revision(&config, &revision) {
                Ok(outcome) => {
                    if let Some(warning) = outcome.maintenance_warning() {
                        eprintln!("路由配置已提交，但本地维护仍待完成：{warning}");
                    }
                    return Ok(config);
                }
                Err(StorageError::Conflict(_)) if attempt + 1 < CONFIG_UPDATE_ATTEMPTS => {}
                Err(StorageError::Conflict(_)) => {
                    return Err(
                        "路由配置持续被另一进程修改；未覆盖并发更改，请稍后重试。".to_owned()
                    );
                }
                Err(_) => {
                    return Err(format!(
                        "无法保存路由配置 {}；请检查当前用户的文件权限。",
                        self.config.path().display()
                    ));
                }
            }
        }
        Err("路由配置更新重试次数已耗尽。".to_owned())
    }

    fn add_provider_with_model(
        &self,
        input: &mut AddProviderWithModelInput,
    ) -> Result<AddProviderWithModelResult, String> {
        let before = self.load_config()?;
        let requires_restart = matches!(probe_health(&before), HealthProbe::Healthy(_));
        let credentials = OsCredentialStore::scoped(
            self.config
                .instance_id()
                .map_err(|_| "无法确定此配置专用的凭据库。".to_owned())?,
        );
        let config = add_provider_and_model(&self.config, &credentials, input)?;
        if let Ok(mut remote) = self.remote.lock() {
            *remote = None;
        }
        Ok(AddProviderWithModelResult {
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            requires_restart,
        })
    }

    fn complete_local_setup(&self) -> Result<LocalSetupResult, LocalSetupFailure> {
        let _setup_guard = self.setup.try_lock().map_err(|_| {
            local_setup_failure(
                "setup-lock",
                "一键接入正在另一个窗口操作中，请等待完成后再试。".to_owned(),
                false,
                false,
                false,
                false,
            )
        })?;
        let config = self.load_config().map_err(|message| {
            local_setup_failure("load-config", message, false, false, false, false)
        })?;
        let router_executable = find_service_binary().ok_or_else(|| {
            local_setup_failure(
                "find-router",
                "未找到随软件安装的 cmr 路由程序，无法自动接入。".to_owned(),
                false,
                false,
                false,
                false,
            )
        })?;
        let state_db = desktop_state_db_path(&self.config).map_err(|message| {
            local_setup_failure("resolve-paths", message, false, false, false, false)
        })?;
        let integration =
            CodexIntegration::for_current_user(&config.server.host, config.server.port).map_err(
                |_| {
                    local_setup_failure(
                        "resolve-codex-config",
                        "无法解析当前 Windows 用户的 Codex 配置路径。".to_owned(),
                        false,
                        false,
                        false,
                        false,
                    )
                },
            )?;
        let legacy = migrate_known_legacy_windows_router(&config, &router_executable, &state_db)?;
        let result = complete_local_setup_core(
            self,
            &config,
            &router_executable,
            &state_db,
            &integration,
            SystemRunner,
            probe_health,
        );
        if let (Err(failure), Some(legacy)) = (&result, legacy) {
            if restore_known_legacy_windows_router(&config, &router_executable, &state_db, &legacy)
                .is_err()
            {
                return Err(local_setup_failure(
                    "restore-legacy-service",
                    format!(
                        "{}；同时未能自动恢复旧版后台任务，请保留恢复 XML 并停止重试。",
                        failure.message
                    ),
                    failure.integration_installed,
                    failure.service_installed,
                    failure.healthy,
                    failure.restart_chatgpt_required,
                ));
            }
        }
        result
    }

    fn stop_owned_on_exit(&self) {
        if let Ok(mut managed) = self.service.lock() {
            let _ = managed.stop_owned();
        }
    }

    fn dashboard(&self) -> Result<DashboardState, String> {
        let config = self.load_config()?;
        let snapshot = self.service_snapshot(&config)?;
        let remote = self
            .remote
            .lock()
            .map_err(|_| "Remote 状态锁不可用。".to_owned())?
            .clone()
            .unwrap_or_else(remote_not_checked);
        let catalog_version = snapshot.health.as_ref().map_or_else(
            || format!("配置 v{}", config.version),
            |health| health.version.clone(),
        );
        let codex_integration = codex_integration_state(&config);

        Ok(DashboardState {
            service: snapshot.state,
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            remote,
            catalog_version,
            config_path: self.config.path().to_string_lossy().into_owned(),
            codex_integration,
        })
    }

    fn service_snapshot(&self, config: &RouterConfig) -> Result<ServiceSnapshot, String> {
        let mut managed = self
            .service
            .lock()
            .map_err(|_| "服务状态锁不可用。".to_owned())?;
        managed.reap();
        Ok(snapshot_with_guard(config, &managed))
    }

    fn set_service_running(&self, running: bool) -> Result<ServiceState, String> {
        let config = self.load_config()?;
        if running {
            self.start_service(&config)
        } else {
            self.stop_service(&config)
        }
    }

    fn start_service(&self, config: &RouterConfig) -> Result<ServiceState, String> {
        let mut managed = self
            .service
            .lock()
            .map_err(|_| "服务状态锁不可用。".to_owned())?;
        managed.reap();

        match probe_health(config) {
            HealthProbe::Healthy(_) => return Ok(snapshot_with_guard(config, &managed).state),
            HealthProbe::Occupied => {
                return Err(format!(
                    "{}:{} 已被其他程序占用，且响应不是 ModelRelay。",
                    config.server.host, config.server.port
                ));
            }
            HealthProbe::Stopped => {}
        }

        let executable = find_router_binary().ok_or_else(|| {
            "未找到 cmr 可执行文件。请安装 CLI，或设置 CMR_DESKTOP_ROUTER_BIN 指向本机 cmr。"
                .to_owned()
        })?;
        let mut command = Command::new(&executable);
        command
            .arg("--config")
            .arg(self.config.path())
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_background_process(&mut command);
        let child = command
            .spawn()
            .map_err(|_| format!("无法启动本机路由可执行文件 {}。", executable.display()))?;
        managed.child = Some(child);
        managed.started_at = Some(Instant::now());

        for _ in 0..START_ATTEMPTS {
            let exited = managed
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok())
                .flatten()
                .is_some();
            if exited {
                managed.child = None;
                managed.started_at = None;
                return Err("cmr 进程在健康检查通过前退出；请运行 cmr doctor。".to_owned());
            }
            if matches!(probe_health(config), HealthProbe::Healthy(_)) {
                return Ok(snapshot_with_guard(config, &managed).state);
            }
            thread::sleep(START_RETRY_DELAY);
        }

        managed
            .stop_owned()
            .map_err(|error| format!("cmr 未通过 /health，且清理子进程失败：{error}"))?;
        Err("cmr 已启动但未在三秒内通过 /health 检查，进程已安全停止。".to_owned())
    }

    fn stop_service(&self, config: &RouterConfig) -> Result<ServiceState, String> {
        let mut managed = self
            .service
            .lock()
            .map_err(|_| "服务状态锁不可用。".to_owned())?;
        managed.reap();
        if managed.stop_owned()? {
            return Ok(snapshot_with_guard(config, &managed).state);
        }

        if matches!(probe_health(config), HealthProbe::Healthy(_)) {
            return Err(
                "路由由此窗口之外的进程或系统服务管理；为避免终止未知进程，桌面端不会停止它。"
                    .to_owned(),
            );
        }
        Ok(snapshot_with_guard(config, &managed).state)
    }

    fn set_model_visibility(
        &self,
        model_id: &str,
        visible: bool,
    ) -> Result<Vec<ModelSummary>, String> {
        let config = self.update_config(|config| {
            let model = config
                .models
                .iter_mut()
                .find(|model| model.id == model_id)
                .ok_or_else(|| format!("未知的已配置外部模型：{model_id}"))?;
            if visible {
                model.enabled = true;
                config.hidden_models.retain(|id| id != model_id);
            } else if !config.hidden_models.iter().any(|id| id == model_id) {
                config.hidden_models.push(model_id.to_owned());
            }
            Ok(())
        })?;
        Ok(model_summaries(&config))
    }

    fn reorder_models(&self, ordered_ids: &[String]) -> Result<Vec<ModelSummary>, String> {
        let config = self.update_config(|config| {
            let requested: HashSet<&str> = ordered_ids.iter().map(String::as_str).collect();
            let configured: HashSet<&str> = config
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect();
            if ordered_ids.len() != config.models.len() || requested != configured {
                return Err("orderedIds 必须恰好包含每个已配置外部模型一次。".to_owned());
            }

            config.catalog_order =
                merge_external_order(&config.catalog_order, &configured, ordered_ids);
            let positions: HashMap<&str, i32> = ordered_ids
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    let position = i32::try_from(index).unwrap_or(i32::MAX);
                    (id.as_str(), position)
                })
                .collect();
            for model in &mut config.models {
                if let Some(position) = positions.get(model.id.as_str()) {
                    model.order = *position;
                }
            }
            Ok(())
        })?;
        Ok(model_summaries(&config))
    }

    fn remove_provider(&self, provider_id: &str) -> Result<AddProviderWithModelResult, String> {
        let before = self.load_config()?;
        let requires_restart = matches!(probe_health(&before), HealthProbe::Healthy(_));
        let config = self.update_config(|config| {
            if provider_id == "official" {
                return Err("无法移除官方 ChatGPT 供应商。".to_owned());
            }
            if config
                .models
                .iter()
                .any(|model| model.provider == provider_id)
            {
                return Err(format!(
                    "供应商 {provider_id} 仍被模型引用，请先删除该供应商下的所有模型。"
                ));
            }
            let before_len = config.providers.len();
            config
                .providers
                .retain(|provider| provider.id != provider_id);
            if config.providers.len() == before_len {
                return Err(format!("未知的供应商：{provider_id}"));
            }
            Ok(())
        })?;
        if let Ok(mut remote) = self.remote.lock() {
            *remote = None;
        }
        Ok(AddProviderWithModelResult {
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            requires_restart,
        })
    }

    fn delete_model(&self, model_id: &str) -> Result<AddProviderWithModelResult, String> {
        let before = self.load_config()?;
        let requires_restart = matches!(probe_health(&before), HealthProbe::Healthy(_));
        let config = self.update_config(|config| {
            let before_len = config.models.len();
            config.models.retain(|model| model.id != model_id);
            if config.models.len() == before_len {
                return Err(format!("未知的已配置外部模型：{model_id}"));
            }
            // Keep catalog order and hidden lists free of stale references to
            // the removed model id.
            config.catalog_order.retain(|id| id != model_id);
            config.hidden_models.retain(|id| id != model_id);
            Ok(())
        })?;
        if let Ok(mut remote) = self.remote.lock() {
            *remote = None;
        }
        Ok(AddProviderWithModelResult {
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            requires_restart,
        })
    }

    fn update_model(&self, input: &UpdateModelInput) -> Result<AddProviderWithModelResult, String> {
        let before = self.load_config()?;
        let requires_restart = matches!(probe_health(&before), HealthProbe::Healthy(_));
        let model_id = input.model_id.clone();
        let config = self.update_config(|config| {
            let model = config
                .models
                .iter_mut()
                .find(|model| model.id == model_id)
                .ok_or_else(|| format!("未知的已配置外部模型：{model_id}"))?;
            if let Some(display_name) = input.display_name.as_ref() {
                if display_name.trim().is_empty() {
                    return Err("显示名称不能为空。".to_owned());
                }
                display_name.trim().clone_into(&mut model.display_name);
            }
            if let Some(upstream_model) = input.upstream_model.as_ref() {
                if upstream_model.trim().is_empty() {
                    return Err("上游模型 ID 不能为空。".to_owned());
                }
                upstream_model.trim().clone_into(&mut model.upstream_model);
            }
            if let Some(context_window) = input.context_window {
                model.context_window = (context_window > 0).then_some(context_window);
            }
            if let Some(max_output_tokens) = input.max_output_tokens {
                model.max_output_tokens = (max_output_tokens > 0).then_some(max_output_tokens);
            }
            if let Some(enabled) = input.enabled {
                model.enabled = enabled;
            }
            Ok(())
        })?;
        if let Ok(mut remote) = self.remote.lock() {
            *remote = None;
        }
        Ok(AddProviderWithModelResult {
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            requires_restart,
        })
    }

    fn update_provider(
        &self,
        input: &mut UpdateProviderInput,
    ) -> Result<AddProviderWithModelResult, String> {
        let before = self.load_config()?;
        let requires_restart = matches!(probe_health(&before), HealthProbe::Healthy(_));
        let config = if input.rotating_secret() {
            self.update_provider_with_secret(input)?
        } else {
            self.update_config(|config| apply_provider_edit(config, input))?
        };
        if let Ok(mut remote) = self.remote.lock() {
            *remote = None;
        }
        Ok(AddProviderWithModelResult {
            providers: provider_summaries(&config),
            models: model_summaries(&config),
            requires_restart,
        })
    }

    /// Rotates a provider credential with the same stage/commit/rollback
    /// contract as the add flow: the new secret is staged first, and rolled
    /// back if the config commit fails or a concurrent edit wins the race.
    fn update_provider_with_secret(
        &self,
        input: &UpdateProviderInput,
    ) -> Result<RouterConfig, String> {
        let credentials = OsCredentialStore::scoped(
            self.config
                .instance_id()
                .map_err(|_| "无法确定此配置专用的凭据库。".to_owned())?,
        );
        for attempt in 0..CONFIG_UPDATE_ATTEMPTS {
            let (mut config, revision) = self
                .config
                .load_with_revision()
                .map_err(|_| "无法读取路由配置。".to_owned())?;
            let provider = config
                .providers
                .iter()
                .find(|provider| provider.id == input.provider_id)
                .ok_or_else(|| format!("未知的供应商：{}", input.provider_id))?;
            let profile = if provider.secret_ref.is_some() {
                SecretRef::parse(provider.secret_ref.as_deref().unwrap_or_default()).map_or_else(
                    |_| normalize_secret_profile(&input.secret_profile),
                    |reference| reference.profile().to_owned(),
                )
            } else {
                normalize_secret_profile(&input.secret_profile)
            };
            let staged = credentials
                .stage_generation(&input.provider_id, &profile, &input.api_key)
                .map_err(|_| "无法将 API Key 保存到操作系统凭据库。".to_owned())?;
            if let Err(edit_error) = apply_provider_edit(&mut config, input) {
                let _ = credentials.delete(&staged);
                return Err(edit_error);
            }
            if let Err(edit_error) = config.validate() {
                let _ = credentials.delete(&staged);
                return Err(format!("供应商配置无效：{edit_error}"));
            }
            config.providers.iter_mut().for_each(|provider| {
                if provider.id == input.provider_id {
                    provider.secret_ref = Some(staged.to_string());
                }
            });
            match self.config.save_if_revision(&config, &revision) {
                Ok(outcome) => {
                    if let Some(warning) = outcome.maintenance_warning() {
                        eprintln!("供应商配置已提交，但本地维护仍待完成：{warning}");
                    }
                    return Ok(config);
                }
                Err(StorageError::Conflict(_)) => {
                    let _ = credentials.delete(&staged);
                    if attempt + 1 >= CONFIG_UPDATE_ATTEMPTS {
                        return Err("配置持续被其他程序修改，请稍后重试。".to_owned());
                    }
                }
                Err(_) => {
                    let _ = credentials.delete(&staged);
                    return Err("无法保存供应商配置。".to_owned());
                }
            }
        }
        Err("路由配置更新重试次数已耗尽。".to_owned())
    }

    fn restore_codex_config(&self) -> Result<RestoreCodexResult, String> {
        let config = self.load_config()?;
        let integration =
            CodexIntegration::for_current_user(&config.server.host, config.server.port).map_err(
                |_| "无法定位 Codex 本机配置，请运行 cmr doctor 检查集成状态。".to_owned(),
            )?;
        let report = integration.restore().map_err(|_| {
            "无法还原 Codex 配置；恢复备份已保留，请通过 cmr codex status 检查。".to_owned()
        })?;
        Ok(RestoreCodexResult {
            restored: report.restored,
            config_path: integration.config_path().to_string_lossy().into_owned(),
        })
    }

    fn check_remote(&self) -> Result<RemoteCompatibility, String> {
        let config = self.load_config()?;
        let snapshot = self.service_snapshot(&config)?;
        let checked_at = unix_milliseconds()?;
        let expected = enabled_external_models(&config);

        let result = match snapshot.health {
            None => RemoteCompatibility {
                state: "blocked",
                ios: "未就绪",
                android: "未就绪",
                message: "本机路由未通过 /health，无法开始 ChatGPT Remote 真机验收。".to_owned(),
                last_checked_at: Some(checked_at),
            },
            Some(_) if expected.is_empty() => RemoteCompatibility {
                state: "blocked",
                ios: "未配置",
                android: "未配置",
                message: "没有已启用的外部模型；请先用 cmr CLI 配置供应商、凭据引用和模型。"
                    .to_owned(),
                last_checked_at: Some(checked_at),
            },
            Some(health) if health.routable_external_models != expected => RemoteCompatibility {
                state: "blocked",
                ios: "配置未同步",
                android: "配置未同步",
                message: "运行中服务的 routable_external_models 与磁盘配置不同；请重启路由后再验收。"
                    .to_owned(),
                last_checked_at: Some(checked_at),
            },
            Some(health) if !health.official_catalog_cached => RemoteCompatibility {
                state: "needs-validation",
                ios: "等待目录",
                android: "等待目录",
                message: "本机路由尚未缓存经过授权的官方模型目录；请先打开模型选择器，再检查外部模型是否实际注入。"
                    .to_owned(),
                last_checked_at: Some(checked_at),
            },
            Some(health) if health.external_models.is_empty() => RemoteCompatibility {
                state: "blocked",
                ios: "未注入",
                android: "未注入",
                message: "外部模型可路由，但授权模型目录没有注入任何外部模型；请检查 picker 容量与账号授权。"
                    .to_owned(),
                last_checked_at: Some(checked_at),
            },
            Some(health)
                if health
                    .external_models
                    .iter()
                    .any(|model| expected.binary_search(model).is_err()) =>
            {
                RemoteCompatibility {
                    state: "blocked",
                    ios: "目录异常",
                    android: "目录异常",
                    message: "授权模型目录报告了不在当前可路由配置中的外部模型；请重启路由后再验收。"
                        .to_owned(),
                    last_checked_at: Some(checked_at),
                }
            }
            Some(health) => {
                debug_assert!(picker_is_locally_ready(&health, &expected));
                RemoteCompatibility {
                    state: "local-ready",
                    ios: "需要真机 E2E",
                    android: "需要真机 E2E",
                    message: "本机路由和实际 picker 注入目录已满足本地条件。此结果不证明手机端可见；仍需同一 ChatGPT 账号与工作区执行真机端到端验收。".to_owned(),
                    last_checked_at: Some(checked_at),
                }
            }
        };
        *self
            .remote
            .lock()
            .map_err(|_| "Remote 状态锁不可用。".to_owned())? = Some(result.clone());
        Ok(result)
    }
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

fn provider_setup_options_document() -> ProviderSetupOptions {
    let mut presets = built_in_presets()
        .into_iter()
        .map(|preset| ProviderSetupPreset {
            id: preset.id,
            label: preset.display_name,
            default_base_url: preset.base_url,
            default_model: preset.default_model.unwrap_or_default(),
            requires_api_key: preset.auth != AuthStyle::None,
            context_window: preset.capabilities.context_window,
            max_output_tokens: preset.capabilities.max_output_tokens,
        })
        .collect::<Vec<_>>();
    presets.push(ProviderSetupPreset {
        id: "custom-compatible".to_owned(),
        label: "自定义 OpenAI 兼容".to_owned(),
        default_base_url: String::new(),
        default_model: String::new(),
        requires_api_key: true,
        context_window: None,
        max_output_tokens: None,
    });
    ProviderSetupOptions { presets }
}

fn add_provider_and_model<S: RevisionedConfigStore + ?Sized>(
    store: &S,
    credentials: &dyn CredentialStore,
    input: &mut AddProviderWithModelInput,
) -> Result<RouterConfig, String> {
    validate_setup_input(input)?;
    let (preset, base_url) = resolve_setup_preset(input)?;
    let requires_key = preset.auth != AuthStyle::None;
    if requires_key && input.api_key.is_empty() {
        return Err("此供应商需要 API Key。".to_owned());
    }
    if !requires_key && !input.api_key.is_empty() {
        return Err("此供应商不使用 API Key，请清空密钥输入框。".to_owned());
    }
    if input.api_key.len() > MAX_API_KEY_BYTES {
        return Err("API Key 长度超过安全限制。".to_owned());
    }

    for attempt in 0..CONFIG_UPDATE_ATTEMPTS {
        let (mut config, revision) = store
            .load_with_revision()
            .map_err(|_| "无法读取路由配置。".to_owned())?;
        if input.provider_id == "official" {
            return Err("供应商 ID official 为官方模型保留。".to_owned());
        }
        if config
            .providers
            .iter()
            .any(|provider| provider.id == input.provider_id)
        {
            return Err("供应商 ID 已存在。".to_owned());
        }
        if config.models.iter().any(|model| model.id == input.model_id) {
            return Err("模型 ID 已存在。".to_owned());
        }

        let staged = if requires_key {
            Some(
                credentials
                    .stage_generation(
                        &input.provider_id,
                        &normalize_secret_profile(&input.secret_profile),
                        &input.api_key,
                    )
                    .map_err(|_| "无法将 API Key 保存到操作系统凭据库。".to_owned())?,
            )
        } else {
            None
        };

        config.providers.push(ProviderConfig {
            id: input.provider_id.clone(),
            preset: input.preset_id.clone(),
            base_url: base_url.clone(),
            secret_ref: staged.as_ref().map(ToString::to_string),
            enabled: input.enabled,
            allow_insecure_http: input.allow_insecure_http,
        });
        let order = config
            .models
            .iter()
            .map(|model| model.order)
            .max()
            .unwrap_or(-10)
            .saturating_add(10);
        config.models.push(ModelConfig {
            id: input.model_id.clone(),
            display_name: input.display_name.clone(),
            provider: input.provider_id.clone(),
            upstream_model: input.upstream_model.clone(),
            order,
            enabled: input.enabled,
            context_window: input.context_window.or(preset.capabilities.context_window),
            max_output_tokens: input
                .max_output_tokens
                .or(preset.capabilities.max_output_tokens),
        });
        config.hidden_models.retain(|id| id != &input.model_id);
        if !config.catalog_order.iter().any(|id| id == &input.model_id) {
            config.catalog_order.push(input.model_id.clone());
        }

        if config.validate().is_err() {
            rollback_staged(credentials, staged.as_ref())?;
            return Err("供应商或模型配置无效。".to_owned());
        }
        match store.save_if_revision(&config, &revision) {
            Ok(outcome) => {
                if let Some(warning) = outcome.maintenance_warning() {
                    eprintln!("供应商配置已提交，但本地维护仍待完成：{warning}");
                }
                input.clear_secret();
                return Ok(config);
            }
            Err(StorageError::Conflict(_)) if attempt + 1 < CONFIG_UPDATE_ATTEMPTS => {
                rollback_staged(credentials, staged.as_ref())?;
            }
            Err(StorageError::Conflict(_)) => {
                rollback_staged(credentials, staged.as_ref())?;
                return Err("配置持续被其他程序修改，请稍后重试。".to_owned());
            }
            Err(_) => {
                rollback_staged(credentials, staged.as_ref())?;
                return Err("无法保存供应商配置。".to_owned());
            }
        }
    }
    unreachable!("bounded configuration update loop always returns")
}

fn rollback_staged(
    credentials: &dyn CredentialStore,
    staged: Option<&SecretRef>,
) -> Result<(), String> {
    if let Some(reference) = staged {
        credentials
            .delete(reference)
            .map_err(|_| "配置未提交，且临时凭据清理失败。".to_owned())?;
    }
    Ok(())
}

/// Applies the non-secret fields of an `UpdateProviderInput` to the matching
/// provider in place. The caller is responsible for any credential staging;
/// this helper only touches `base_url` and `enabled`. An unknown provider id or
/// a malformed base URL is reported as an error so the CAS loop can surface it
/// to the user without writing the config.
fn apply_provider_edit(
    config: &mut RouterConfig,
    input: &UpdateProviderInput,
) -> Result<(), String> {
    if input.provider_id == "official" {
        return Err("官方 ChatGPT 供应商的端点与凭据由登录态管理，不可编辑。".to_owned());
    }
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == input.provider_id)
        .ok_or_else(|| format!("未知的供应商：{}", input.provider_id))?;
    if let Some(base_url) = input.base_url.as_ref() {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            provider.base_url = None;
        } else if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            provider.base_url = Some(trimmed.to_owned());
        } else {
            return Err("Base URL 必须以 http:// 或 https:// 开头。".to_owned());
        }
    }
    if let Some(enabled) = input.enabled {
        provider.enabled = enabled;
    }
    if let Some(allow_insecure_http) = input.allow_insecure_http {
        provider.allow_insecure_http = allow_insecure_http;
    }
    Ok(())
}

fn resolve_setup_preset(
    input: &AddProviderWithModelInput,
) -> Result<(ProviderPreset, Option<String>), String> {
    if input.preset_id == "custom-compatible" {
        if !valid_custom_provider_id(&input.provider_id) {
            return Err(
                "自定义供应商 ID 只能包含小写英文字母、数字和连字符，且必须以字母或数字开头。"
                    .to_owned(),
            );
        }
        let preset = custom_compatible_preset(
            "custom-compatible",
            &input.provider_id,
            &input.base_url,
            input.allow_insecure_http,
        )
        .map_err(|_| INSECURE_BASE_URL_MESSAGE.to_owned())?;
        return Ok((preset.clone(), Some(preset.base_url)));
    }
    let mut preset =
        preset_by_id(&input.preset_id).ok_or_else(|| "未知的供应商预设。".to_owned())?;
    if input.base_url.is_empty() || input.base_url.trim_end_matches('/') == preset.base_url {
        return Ok((preset, None));
    }
    let validated = custom_compatible_preset(
        "endpoint-check",
        "Endpoint",
        &input.base_url,
        input.allow_insecure_http,
    )
    .map_err(|_| INSECURE_BASE_URL_MESSAGE.to_owned())?;
    preset.base_url.clone_from(&validated.base_url);
    Ok((preset, Some(validated.base_url)))
}

const INSECURE_BASE_URL_MESSAGE: &str = "Base URL 无效：必须使用 HTTPS 或本机回环 HTTP。自建明文 HTTP 服务请先勾选“允许明文 HTTP（自建服务）”。";

fn valid_custom_provider_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_setup_input(input: &AddProviderWithModelInput) -> Result<(), String> {
    validate_setup_text("供应商 ID", &input.provider_id, MAX_ID_BYTES)?;
    validate_setup_text("预设 ID", &input.preset_id, MAX_ID_BYTES)?;
    validate_setup_text("凭据配置名", &input.secret_profile, MAX_ID_BYTES)?;
    validate_setup_text("模型 ID", &input.model_id, MAX_ID_BYTES)?;
    validate_setup_text("上游模型 ID", &input.upstream_model, MAX_LABEL_BYTES)?;
    validate_setup_text("显示名称", &input.display_name, MAX_LABEL_BYTES)?;
    if input.base_url.len() > MAX_BASE_URL_BYTES {
        return Err("Base URL 长度超过限制。".to_owned());
    }
    if input.context_window == Some(0) || input.max_output_tokens == Some(0) {
        return Err("Context window 和 Max output 必须大于零。".to_owned());
    }
    SecretRef::new_generation(&input.provider_id, &input.secret_profile)
        .map_err(|_| "供应商 ID 或凭据配置名格式无效。".to_owned())?;
    Ok(())
}

fn validate_setup_text(label: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit {
        return Err(format!("{label}不能为空且不得超过 {limit} 字节。"));
    }
    Ok(())
}

#[allow(clippy::fn_params_excessive_bools)]
fn local_setup_failure(
    stage: &'static str,
    message: String,
    integration_installed: bool,
    service_installed: bool,
    healthy: bool,
    restart_chatgpt_required: bool,
) -> LocalSetupFailure {
    LocalSetupFailure {
        stage,
        message,
        integration_installed,
        service_installed,
        healthy,
        restart_chatgpt_required,
        partial: integration_installed || service_installed || healthy,
    }
}

#[allow(clippy::too_many_lines)]
fn complete_local_setup_core<R, F>(
    state: &DesktopState,
    config: &RouterConfig,
    router_executable: &Path,
    state_db: &Path,
    integration: &CodexIntegration,
    runner: R,
    mut health_probe: F,
) -> Result<LocalSetupResult, LocalSetupFailure>
where
    R: CommandRunner,
    F: FnMut(&RouterConfig) -> HealthProbe,
{
    {
        let mut desktop_service = state.service.lock().map_err(|_| {
            local_setup_failure(
                "stop-desktop-service",
                "服务状态锁不可用。".to_owned(),
                false,
                false,
                false,
                false,
            )
        })?;
        desktop_service.stop_owned().map_err(|_| {
            local_setup_failure(
                "stop-desktop-service",
                "无法安全停止由桌面端启动的临时路由。".to_owned(),
                false,
                false,
                false,
                false,
            )
        })?;
    }

    let mut manager = ServiceManager::discover_with_executable(
        state.config.path(),
        state_db,
        router_executable,
        runner,
    )
    .map_err(|_| {
        local_setup_failure(
            "prepare-service",
            "无法准备当前用户的后台服务路径。".to_owned(),
            false,
            false,
            false,
            false,
        )
    })?;
    let definition_path = manager.definition_path().to_path_buf();
    let prior_status = manager.status().map_err(|_| {
        local_setup_failure(
            "inspect-service",
            "无法确认现有后台服务的所有权；未修改该服务。".to_owned(),
            false,
            false,
            false,
            false,
        )
    })?;
    if prior_status == ServiceStatus::NotInstalled {
        match health_probe(config) {
            HealthProbe::Stopped => {}
            HealthProbe::Healthy(_) => {
                return Err(local_setup_failure(
                    "inspect-service",
                    "端口上已有非计划任务管理的路由；为避免接管未知进程，自动接入已停止。"
                        .to_owned(),
                    false,
                    false,
                    false,
                    false,
                ));
            }
            HealthProbe::Occupied => {
                return Err(local_setup_failure(
                    "inspect-service",
                    "路由端口已被其他程序占用，自动接入未改动 Codex 配置。".to_owned(),
                    false,
                    false,
                    false,
                    false,
                ));
            }
        }
    }

    manager.install().map_err(|_| {
        local_setup_failure(
            "install-service",
            "当前用户后台服务安装或启动失败；未写入 Codex 配置。".to_owned(),
            false,
            false,
            false,
            false,
        )
    })?;
    let service_installed = manager.status().map_err(|_| {
        local_setup_failure(
            "verify-service",
            "后台服务已提交，但无法复核其注册状态。".to_owned(),
            false,
            true,
            false,
            false,
        )
    })? == ServiceStatus::Installed;
    if !service_installed {
        return Err(local_setup_failure(
            "verify-service",
            "后台服务未保持已安装状态。".to_owned(),
            false,
            false,
            false,
            false,
        ));
    }

    let mut health = None;
    for _ in 0..START_ATTEMPTS {
        match health_probe(config) {
            HealthProbe::Healthy(document) => {
                health = Some(document);
                break;
            }
            HealthProbe::Occupied => {
                return Err(local_setup_failure(
                    "health-check",
                    "后台服务已安装，但端口响应不是 ModelRelay。".to_owned(),
                    false,
                    true,
                    false,
                    false,
                ));
            }
            HealthProbe::Stopped => thread::sleep(START_RETRY_DELAY),
        }
    }
    let health = health.ok_or_else(|| {
        local_setup_failure(
            "health-check",
            "后台服务已安装，但未在三秒内通过健康检查。".to_owned(),
            false,
            true,
            false,
            false,
        )
    })?;
    let expected_models = enabled_external_models(config);
    if health.routable_external_models != expected_models {
        return Err(local_setup_failure(
            "catalog-validation",
            "路由健康，但其可路由第三方模型目录与当前配置不一致。".to_owned(),
            false,
            true,
            true,
            false,
        ));
    }
    if health.official_catalog_cached
        && (health.external_models.is_empty()
            || !health
                .external_models
                .iter()
                .all(|model| expected_models.contains(model)))
    {
        return Err(local_setup_failure(
            "catalog-validation",
            "路由已有授权模型目录，但对外发布的第三方模型不是当前配置的非空子集。".to_owned(),
            false,
            true,
            true,
            false,
        ));
    }

    let prior_integration_status = integration.status().map_err(|_| {
        local_setup_failure(
            "inspect-codex-integration",
            "路由已经健康启动，但无法安全检查 Codex 接入状态。".to_owned(),
            false,
            true,
            true,
            false,
        )
    })?;
    let (backup, restart_chatgpt_required) =
        if prior_integration_status == IntegrationStatus::Installed {
            (None, false)
        } else {
            let backup = integration.install().map_err(|_| {
                local_setup_failure(
                    "install-codex-integration",
                    "路由已经健康启动，但 Codex 用户配置无法安全合并。".to_owned(),
                    false,
                    true,
                    true,
                    false,
                )
            })?;
            (Some(backup), true)
        };
    let integration_installed = integration.status().map_err(|_| {
        local_setup_failure(
            "verify-codex-integration",
            "Codex 配置已写入，但无法复核接入状态。".to_owned(),
            true,
            true,
            true,
            true,
        )
    })? == IntegrationStatus::Installed;
    if !integration_installed {
        return Err(local_setup_failure(
            "verify-codex-integration",
            "Codex 配置合并后未保持已安装状态。".to_owned(),
            false,
            true,
            true,
            true,
        ));
    }

    Ok(LocalSetupResult {
        codex_config_path: integration.config_path().to_string_lossy().into_owned(),
        recovery_backup_path: backup.map(|path| path.to_string_lossy().into_owned()),
        service_definition_path: definition_path.to_string_lossy().into_owned(),
        bind_address: format!("{}:{}", config.server.host, config.server.port),
        external_models: health.external_models.clone(),
        integration_installed,
        service_installed,
        healthy: true,
        restart_chatgpt_required,
        picker_pending: !health.official_catalog_cached || health.external_models.is_empty(),
        partial: false,
    })
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn dashboard_state(state: State<'_, DesktopState>) -> Result<DashboardState, String> {
    state.dashboard()
}

#[tauri::command]
fn provider_setup_options() -> ProviderSetupOptions {
    provider_setup_options_document()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn add_provider_with_model(
    mut input: AddProviderWithModelInput,
    state: State<'_, DesktopState>,
) -> Result<AddProviderWithModelResult, String> {
    state.add_provider_with_model(&mut input)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
async fn complete_local_setup(
    state: State<'_, DesktopState>,
) -> Result<LocalSetupResult, LocalSetupFailure> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.complete_local_setup())
        .await
        .map_err(|_| {
            local_setup_failure(
                "setup-worker",
                "一键接入后台任务意外退出，请重试。".to_owned(),
                false,
                false,
                false,
                false,
            )
        })?
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn set_service_running(
    running: bool,
    state: State<'_, DesktopState>,
) -> Result<ServiceState, String> {
    state.set_service_running(running)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn set_model_visibility(
    model_id: String,
    visible: bool,
    state: State<'_, DesktopState>,
) -> Result<Vec<ModelSummary>, String> {
    state.set_model_visibility(&model_id, visible)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn reorder_models(
    ordered_ids: Vec<String>,
    state: State<'_, DesktopState>,
) -> Result<Vec<ModelSummary>, String> {
    state.reorder_models(&ordered_ids)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn check_remote_compatibility(
    state: State<'_, DesktopState>,
) -> Result<RemoteCompatibility, String> {
    state.check_remote()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn remove_provider(
    provider_id: String,
    state: State<'_, DesktopState>,
) -> Result<AddProviderWithModelResult, String> {
    state.remove_provider(&provider_id)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn delete_model(
    model_id: String,
    state: State<'_, DesktopState>,
) -> Result<AddProviderWithModelResult, String> {
    state.delete_model(&model_id)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn restore_codex_config(state: State<'_, DesktopState>) -> Result<RestoreCodexResult, String> {
    state.restore_codex_config()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn update_model(
    input: UpdateModelInput,
    state: State<'_, DesktopState>,
) -> Result<AddProviderWithModelResult, String> {
    state.update_model(&input)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn update_provider(
    mut input: UpdateProviderInput,
    state: State<'_, DesktopState>,
) -> Result<AddProviderWithModelResult, String> {
    state.update_provider(&mut input)
}

const RELEASES_URL: &str = "https://github.com/deeplearning1993/modelrelay/releases";

#[tauri::command]
fn open_releases_page() -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut command = Command::new("explorer.exe");
        command.arg(RELEASES_URL);
        configure_background_process(&mut command);
        command
            .spawn()
            .map_err(|error| format!("无法打开发布页面：{error}"))?;
    }
    #[cfg(not(windows))]
    {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        Command::new(program)
            .arg(RELEASES_URL)
            .spawn()
            .map_err(|error| format!("无法打开发布页面：{error}"))?;
    }
    Ok(())
}

#[tauri::command]
fn reveal_path(path: &str) -> Result<(), String> {
    let target = PathBuf::from(path);
    if !target.exists() {
        return Err("路径不存在。".to_owned());
    }
    #[cfg(windows)]
    {
        let mut command = Command::new("explorer.exe");
        command.arg(format!("/select,{}", target.display()));
        configure_background_process(&mut command);
        command
            .spawn()
            .map_err(|error| format!("无法打开资源管理器：{error}"))?;
    }
    #[cfg(not(windows))]
    {
        let parent = target.parent().unwrap_or_else(|| Path::new("/"));
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        Command::new(program)
            .arg(parent)
            .spawn()
            .map_err(|error| format!("无法打开目录：{error}"))?;
    }
    Ok(())
}

/// Best-effort cleanup of the renamed product's previous install directory
/// and its orphaned "Add/Remove Programs" entry. The installer has already
/// migrated the scheduled task, so nothing runs from the old directory. The
/// old uninstaller is deliberately never executed because it would undo the
/// active Codex integration.
fn cleanup_legacy_install_directory() {
    let Some(local) = env::var_os("LOCALAPPDATA") else {
        return;
    };
    let legacy = PathBuf::from(local).join("Codex Model Router");
    if !legacy.join("cmr-service.exe").is_file() {
        return;
    }
    let _ = fs::remove_dir_all(&legacy);
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        "Get-ChildItem 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall' -ErrorAction SilentlyContinue | \
         Where-Object { (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).DisplayName -eq 'Codex Model Router' } | \
         Remove-Item -Recurse -Force",
    ]);
    configure_background_process(&mut command);
    let _ = command.spawn();
}

fn snapshot_with_guard(config: &RouterConfig, managed: &ManagedService) -> ServiceSnapshot {
    let bind_address = config.server.host.parse::<IpAddr>().map_or_else(
        |_| format!("{}:{}", config.server.host, config.server.port),
        |ip| SocketAddr::new(ip, config.server.port).to_string(),
    );
    match probe_health(config) {
        HealthProbe::Healthy(health) => {
            let owned = managed.child.is_some();
            ServiceSnapshot {
                state: ServiceState {
                    status: "running",
                    management: if owned { "desktop" } else { "external" },
                    manageable: owned,
                    bind_address,
                    pid: managed.child.as_ref().map(Child::id),
                    uptime_seconds: managed
                        .started_at
                        .map(|started| started.elapsed().as_secs()),
                    detail: if owned {
                        "健康检查通过；服务由此窗口启动。".to_owned()
                    } else {
                        "健康检查通过；服务由外部进程或系统服务管理，桌面端不会终止未知进程。"
                            .to_owned()
                    },
                },
                health: Some(health),
            }
        }
        HealthProbe::Stopped => {
            let available = find_router_binary().is_some();
            ServiceSnapshot {
                state: ServiceState {
                    status: "stopped",
                    management: if available { "desktop" } else { "unavailable" },
                    manageable: available,
                    bind_address,
                    pid: None,
                    uptime_seconds: None,
                    detail: if available {
                        "端口未监听；可以从此窗口启动本机 cmr。".to_owned()
                    } else {
                        "端口未监听，且未找到 cmr 可执行文件。".to_owned()
                    },
                },
                health: None,
            }
        }
        HealthProbe::Occupied => ServiceSnapshot {
            state: ServiceState {
                status: "unavailable",
                management: "unavailable",
                manageable: false,
                bind_address,
                pid: None,
                uptime_seconds: None,
                detail: "端口有响应，但不是有效的 ModelRelay /health；不会接管或终止该进程。"
                    .to_owned(),
            },
            health: None,
        },
    }
}

/// Reports whether a Codex integration is currently installed for the router's
/// bind address. Failures to inspect the integration are treated as
/// "not installed" so the UI never blocks the user from running `cmr doctor`.
fn codex_integration_state(config: &RouterConfig) -> CodexIntegrationState {
    let installed = CodexIntegration::for_current_user(&config.server.host, config.server.port)
        .and_then(|integration| integration.status())
        .is_ok_and(|status| status == IntegrationStatus::Installed);
    CodexIntegrationState { installed }
}

fn probe_health(config: &RouterConfig) -> HealthProbe {
    let ip = match config.server.host.parse::<IpAddr>() {
        Ok(ip) if ip.is_loopback() => ip,
        _ => return HealthProbe::Occupied,
    };
    let address = SocketAddr::new(ip, config.server.port);
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HEALTH_TIMEOUT) else {
        return HealthProbe::Stopped;
    };
    if stream.set_read_timeout(Some(HEALTH_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(HEALTH_TIMEOUT)).is_err()
    {
        return HealthProbe::Occupied;
    }
    let host = if ip.is_ipv6() {
        format!("[{ip}]:{}", config.server.port)
    } else {
        format!("{ip}:{}", config.server.port)
    };
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return HealthProbe::Occupied;
    }
    let mut response = String::new();
    if (&mut stream)
        .take(HEALTH_RESPONSE_LIMIT)
        .read_to_string(&mut response)
        .is_err()
    {
        return HealthProbe::Occupied;
    }
    parse_health_response(&response).map_or(HealthProbe::Occupied, HealthProbe::Healthy)
}

fn parse_health_response(response: &str) -> Option<HealthDocument> {
    let (headers, body) = response.split_once("\r\n\r\n")?;
    let status = headers.lines().next()?;
    if !(status.starts_with("HTTP/1.1 200 ") || status.starts_with("HTTP/1.0 200 ")) {
        return None;
    }
    let document: Value = serde_json::from_str(body).ok()?;
    if document.get("status")?.as_str()? != "ok"
        || document.get("service")?.as_str()? != "codex-model-router"
    {
        return None;
    }
    let version = document.get("version")?.as_str()?.to_owned();
    let mut external_models = document
        .get("external_models")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    external_models.sort();
    external_models.dedup();
    let mut routable_external_models = document
        .get("routable_external_models")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    routable_external_models.sort();
    routable_external_models.dedup();
    let official_catalog_cached = document.get("official_catalog_cached")?.as_bool()?;
    Some(HealthDocument {
        version,
        external_models,
        routable_external_models,
        official_catalog_cached,
    })
}

fn picker_is_locally_ready(health: &HealthDocument, expected: &[String]) -> bool {
    !expected.is_empty()
        && health.routable_external_models == expected
        && health.official_catalog_cached
        && !health.external_models.is_empty()
        && health
            .external_models
            .iter()
            .all(|model| expected.binary_search(model).is_ok())
}

fn provider_summaries(config: &RouterConfig) -> Vec<ProviderSummary> {
    config
        .providers
        .iter()
        .map(|provider| {
            let official = provider.id == "official";
            let preset = preset_by_id(&provider.preset);
            let label = if official {
                "ChatGPT / Codex".to_owned()
            } else {
                preset
                    .as_ref()
                    .map_or_else(|| provider.id.clone(), |value| value.display_name.clone())
            };
            let protocol = if official || provider.preset == "openai-responses" {
                "Responses".to_owned()
            } else {
                preset.as_ref().map_or_else(
                    || "OpenAI-compatible".to_owned(),
                    |value| protocol_label(value.protocol).to_owned(),
                )
            };
            let credential_status = if official {
                "managed"
            } else if preset
                .as_ref()
                .is_some_and(|value| value.auth == AuthStyle::None)
            {
                "not-required"
            } else if provider.secret_ref.is_some() {
                "referenced"
            } else {
                "missing"
            };
            ProviderSummary {
                id: provider.id.clone(),
                label,
                protocol,
                enabled: provider.enabled,
                credential_status,
                model_count: config
                    .models
                    .iter()
                    .filter(|model| model.provider == provider.id)
                    .count(),
                official,
                base_url: provider.base_url.clone(),
                secret_profile: provider
                    .secret_ref
                    .as_deref()
                    .and_then(|value| SecretRef::parse(value).ok())
                    .map(|reference| reference.profile().to_owned()),
                allow_insecure_http: provider.allow_insecure_http,
            }
        })
        .collect()
}

fn model_summaries(config: &RouterConfig) -> Vec<ModelSummary> {
    let providers: HashMap<&str, &ProviderConfig> = config
        .providers
        .iter()
        .map(|provider| (provider.id.as_str(), provider))
        .collect();
    let ranks: HashMap<&str, usize> = config
        .catalog_order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let fallback_rank = ranks.len();
    let hidden: HashSet<&str> = config.hidden_models.iter().map(String::as_str).collect();
    let mut models = config
        .models
        .iter()
        .map(|model| {
            let provider = providers.get(model.provider.as_str()).copied();
            let provider_label = provider.map_or_else(
                || format!("未知供应商 ({})", model.provider),
                provider_display_name,
            );
            ModelSummary {
                id: model.id.clone(),
                label: model.display_name.clone(),
                provider_id: model.provider.clone(),
                provider_label,
                official: false,
                visible: provider.is_some_and(|provider| provider.enabled)
                    && model.enabled
                    && !hidden.contains(model.id.as_str()),
                capabilities: model_capabilities(provider),
                upstream_model: model.upstream_model.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                enabled: model.enabled,
            }
        })
        .collect::<Vec<_>>();
    let orders: HashMap<&str, i32> = config
        .models
        .iter()
        .map(|model| (model.id.as_str(), model.order))
        .collect();
    models.sort_by_key(|model| {
        (
            ranks
                .get(model.id.as_str())
                .copied()
                .unwrap_or(fallback_rank),
            orders.get(model.id.as_str()).copied().unwrap_or(i32::MAX),
        )
    });
    models
}

fn provider_display_name(provider: &ProviderConfig) -> String {
    if provider.id == "official" {
        return "ChatGPT / Codex".to_owned();
    }
    preset_by_id(&provider.preset).map_or_else(|| provider.id.clone(), |preset| preset.display_name)
}

fn model_capabilities(provider: Option<&ProviderConfig>) -> Vec<String> {
    let mut values = vec![
        "responses".to_owned(),
        "sse".to_owned(),
        "websocket".to_owned(),
        "tools".to_owned(),
        "compaction".to_owned(),
    ];
    let capabilities = provider
        .and_then(|provider| preset_by_id(&provider.preset))
        .map_or_else(ProviderCapabilities::compatible, |preset| {
            preset.capabilities
        });
    if capabilities.reasoning {
        values.push("reasoning".to_owned());
    }
    if capabilities.vision {
        values.push("vision".to_owned());
    }
    if capabilities.audio {
        values.push("audio".to_owned());
    }
    values
}

const fn protocol_label(protocol: ProtocolFamily) -> &'static str {
    match protocol {
        ProtocolFamily::Responses => "Responses",
        ProtocolFamily::OpenAiChatCompletions => "Chat Completions",
        ProtocolFamily::AnthropicMessages => "Anthropic Messages",
        ProtocolFamily::GeminiGenerateContent => "Gemini GenerateContent",
    }
}

fn merge_external_order(
    current: &[String],
    configured: &HashSet<&str>,
    requested: &[String],
) -> Vec<String> {
    let mut replacements = requested.iter();
    let mut merged = Vec::with_capacity(current.len().max(requested.len()));
    for id in current {
        if configured.contains(id.as_str()) {
            if let Some(replacement) = replacements.next() {
                merged.push(replacement.clone());
            }
        } else {
            merged.push(id.clone());
        }
    }
    merged.extend(replacements.cloned());
    merged
}

fn enabled_external_models(config: &RouterConfig) -> Vec<String> {
    let providers: HashSet<&str> = config
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.id.as_str())
        .collect();
    let mut models = config
        .models
        .iter()
        .filter(|model| {
            model.enabled
                && providers.contains(model.provider.as_str())
                && !config
                    .hidden_models
                    .iter()
                    .any(|hidden| hidden == &model.id)
        })
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    models.sort();
    models
}

fn remote_not_checked() -> RemoteCompatibility {
    RemoteCompatibility {
        state: "needs-validation",
        ios: "未验收",
        android: "未验收",
        message:
            "尚未核对本机 /health 与 external_models；移动端结果始终需要同账号、同工作区真机验收。"
                .to_owned(),
        last_checked_at: None,
    }
}

fn unix_milliseconds() -> Result<u64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时间早于 Unix epoch。".to_owned())?;
    u64::try_from(duration.as_millis()).map_err(|_| "系统时间超出支持范围。".to_owned())
}

struct KnownLegacyWindowsRouter {
    backup: LegacyWindowsTaskBackup,
    powershell: PathBuf,
    launcher: PathBuf,
}

#[cfg(windows)]
struct LegacyWindowsRouterPaths {
    powershell: PathBuf,
    python: PathBuf,
    launcher: PathBuf,
    router: PathBuf,
}

#[cfg(windows)]
fn migrate_known_legacy_windows_router(
    config: &RouterConfig,
    router_executable: &Path,
    state_db: &Path,
) -> Result<Option<KnownLegacyWindowsRouter>, LocalSetupFailure> {
    if config.server.host != "127.0.0.1" || config.server.port != 15_722 {
        return Ok(None);
    }
    let user_profile = env::var_os("USERPROFILE")
        .ok_or_else(|| legacy_failure("无法确认当前 Windows 用户目录；未修改旧版后台任务。"))?;
    let local_app_data = env::var_os("LOCALAPPDATA")
        .ok_or_else(|| legacy_failure("无法确认当前 Windows 用户程序目录；未修改旧版后台任务。"))?;
    let windows = env::var_os("WINDIR")
        .ok_or_else(|| legacy_failure("无法确认 Windows 系统目录；未修改旧版后台任务。"))?;
    let legacy_root = PathBuf::from(user_profile).join("CodexRouter");
    let paths = LegacyWindowsRouterPaths {
        launcher: legacy_root.join("Start-Router.ps1"),
        router: legacy_root.join("router.py"),
        powershell: PathBuf::from(windows)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
        python: PathBuf::from(local_app_data)
            .join("Programs")
            .join("Python")
            .join("Python313")
            .join("pythonw.exe"),
    };
    migrate_known_legacy_windows_router_with(
        &config_path_for_migration(config)?,
        state_db,
        router_executable,
        SystemRunner,
        paths,
        exact_legacy_listener_pid,
        stop_exact_legacy_process,
    )
}

#[cfg(windows)]
fn migrate_known_legacy_windows_router_with<R, P, S>(
    config_path: &Path,
    state_db: &Path,
    router_executable: &Path,
    runner: R,
    paths: LegacyWindowsRouterPaths,
    mut listener_pid: P,
    mut stop_process: S,
) -> Result<Option<KnownLegacyWindowsRouter>, LocalSetupFailure>
where
    R: CommandRunner,
    P: FnMut(&Path, &Path) -> Result<Option<u32>, ()>,
    S: FnMut(u32, &Path, &Path) -> Result<(), String>,
{
    const LAUNCHER_SHA256: &str =
        "8369c8bacd986de6b12696013c494cadab504f7bfab3aa8a4e6c57df3f3a0550";
    const ROUTER_SHA256: &str = "2e71d2a90f620a620b5d9f7dd16e8e551b9e6f3372905f4f07481da20aceb095";
    let mut manager =
        ServiceManager::discover_with_executable(config_path, state_db, router_executable, runner)
            .map_err(|_| legacy_failure("无法准备旧版任务迁移；未修改旧任务或进程。"))?;
    let legacy_task_present = manager
        .exact_legacy_windows_task_present(&paths.powershell, &paths.launcher)
        .map_err(|_| legacy_failure("旧版任务的用户、动作或内容无法精确确认；未修改该任务。"))?;
    if !legacy_task_present {
        return Ok(None);
    }
    if !paths.powershell.is_file()
        || !paths.python.is_file()
        || !paths.launcher.is_file()
        || !paths.router.is_file()
    {
        return Err(legacy_failure(
            "检测到精确旧版任务，但其运行文件不完整；未修改旧任务或进程。",
        ));
    }
    if file_sha256(&paths.launcher).as_deref() != Ok(LAUNCHER_SHA256)
        || file_sha256(&paths.router).as_deref() != Ok(ROUTER_SHA256)
    {
        return Err(legacy_failure(
            "检测到旧版目录，但文件指纹不匹配；为避免误删，未修改旧任务或进程。",
        ));
    }
    let pid = listener_pid(&paths.router, &paths.python).map_err(|()| {
        legacy_failure("无法安全确认 15722 端口上的旧版进程；未修改旧任务或进程。")
    })?;
    let Some(backup) = manager
        .remove_exact_legacy_windows_task(&paths.powershell, &paths.launcher)
        .map_err(|_| legacy_failure("旧版任务的用户、动作或内容无法精确确认；未删除该任务。"))?
    else {
        return Ok(None);
    };
    if let Some(pid) = pid
        && let Err(message) = stop_process(pid, &paths.router, &paths.python)
    {
        let _ =
            manager.restore_exact_legacy_windows_task(&backup, &paths.powershell, &paths.launcher);
        return Err(legacy_failure(&message));
    }
    Ok(Some(KnownLegacyWindowsRouter {
        backup,
        powershell: paths.powershell,
        launcher: paths.launcher,
    }))
}

#[cfg(not(windows))]
fn migrate_known_legacy_windows_router(
    _config: &RouterConfig,
    _router_executable: &Path,
    _state_db: &Path,
) -> Result<Option<KnownLegacyWindowsRouter>, LocalSetupFailure> {
    Ok(None)
}

#[cfg(windows)]
fn restore_known_legacy_windows_router(
    config: &RouterConfig,
    router_executable: &Path,
    state_db: &Path,
    legacy: &KnownLegacyWindowsRouter,
) -> Result<(), ()> {
    let config_path = config_path_for_migration(config).map_err(|_| ())?;
    let mut manager = ServiceManager::discover_with_executable(
        &config_path,
        state_db,
        router_executable,
        SystemRunner,
    )
    .map_err(|_| ())?;
    if manager.status().map_err(|_| ())? == ServiceStatus::Installed {
        manager.uninstall().map_err(|_| ())?;
    }
    manager
        .restore_exact_legacy_windows_task(&legacy.backup, &legacy.powershell, &legacy.launcher)
        .map_err(|_| ())
}

#[cfg(not(windows))]
fn restore_known_legacy_windows_router(
    _config: &RouterConfig,
    _router_executable: &Path,
    _state_db: &Path,
    _legacy: &KnownLegacyWindowsRouter,
) -> Result<(), ()> {
    Ok(())
}

fn config_path_for_migration(_config: &RouterConfig) -> Result<PathBuf, LocalSetupFailure> {
    config_store()
        .map(|store| store.path().to_path_buf())
        .map_err(|_| legacy_failure("无法确认路由配置路径；未修改旧版后台任务。"))
}

fn legacy_failure(message: &str) -> LocalSetupFailure {
    local_setup_failure(
        "migrate-legacy-service",
        message.to_owned(),
        false,
        false,
        false,
        false,
    )
}

#[cfg(windows)]
fn file_sha256(path: &Path) -> Result<String, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(windows)]
fn exact_legacy_listener_pid(router: &Path, python: &Path) -> Result<Option<u32>, ()> {
    let mut command = Command::new("netstat.exe");
    command.args(["-ano", "-p", "tcp"]);
    configure_background_process(&mut command);
    let output = command.output().map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pids = text
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() == 5
                && fields[0].eq_ignore_ascii_case("TCP")
                && fields[1] == "127.0.0.1:15722"
                && fields[3].eq_ignore_ascii_case("LISTENING"))
            .then(|| fields[4].parse::<u32>().ok())
            .flatten()
        })
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    match pids.as_slice() {
        [] => Ok(None),
        [pid] if exact_legacy_process(*pid, router, python) => Ok(Some(*pid)),
        _ => Err(()),
    }
}

#[cfg(windows)]
fn exact_legacy_process(pid: u32, router: &Path, python: &Path) -> bool {
    let system = System::new_all();
    let Some(process) = system.process(Pid::from_u32(pid)) else {
        return false;
    };
    let Some(executable) = process.exe() else {
        return false;
    };
    let command = process.cmd();
    command.len() == 3
        && executable
            .to_string_lossy()
            .eq_ignore_ascii_case(&python.to_string_lossy())
        && executable
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("pythonw.exe"))
        && Path::new(&command[0])
            .to_string_lossy()
            .eq_ignore_ascii_case(&executable.to_string_lossy())
        && command[1] == "-u"
        && Path::new(&command[2])
            .to_string_lossy()
            .eq_ignore_ascii_case(&router.to_string_lossy())
}

#[cfg(windows)]
fn stop_exact_legacy_process(pid: u32, router: &Path, python: &Path) -> Result<(), String> {
    if !exact_legacy_process(pid, router, python) {
        return Err("旧版进程在迁移期间发生变化；已尝试恢复旧任务。".to_owned());
    }
    let system = System::new_all();
    let process = system
        .process(Pid::from_u32(pid))
        .ok_or_else(|| "旧版进程在迁移期间消失；已尝试恢复旧任务。".to_owned())?;
    if !process.kill() {
        return Err("无法停止已精确验证的旧版路由；已尝试恢复旧任务。".to_owned());
    }
    for _ in 0..30 {
        if System::new_all().process(Pid::from_u32(pid)).is_none() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("旧版路由未在三秒内停止；已尝试恢复旧任务。".to_owned())
}

fn find_router_binary() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("CMR_DESKTOP_ROUTER_BIN") {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }

    let binary_name = OsString::from(format!("cmr{}", env::consts::EXE_SUFFIX));
    if let Ok(current) = env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join(&binary_name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(&binary_name))
            .find(|candidate| candidate.is_file())
    })
}

fn find_service_binary() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("CMR_DESKTOP_SERVICE_BIN") {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }

    let binary_name = OsString::from(format!("cmr-service{}", env::consts::EXE_SUFFIX));
    if let Ok(current) = env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join(&binary_name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    if let Some(router) = env::var_os("CMR_DESKTOP_ROUTER_BIN")
        && let Some(directory) = Path::new(&router).parent()
    {
        let sibling = directory.join(&binary_name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(&binary_name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn configure_background_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_background_process(_command: &mut Command) {}

fn config_store() -> Result<ConfigStore, String> {
    if let Some(path) = env::var_os("CMR_DESKTOP_CONFIG") {
        return Ok(ConfigStore::new(Path::new(&path)));
    }
    ConfigStore::discover().map_err(|_| "无法解析 ModelRelay 用户配置目录。".to_owned())
}

fn desktop_state_db_path(config: &ConfigStore) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("CMR_DESKTOP_STATE_DB").or_else(|| env::var_os("CMR_STATE_DB"))
    {
        return Ok(PathBuf::from(path));
    }
    let discovered =
        AppPaths::discover().map_err(|_| "无法解析当前 Windows 用户的路由数据目录。".to_owned())?;
    if env::var_os("CMR_DESKTOP_CONFIG").is_some() && config.path() != discovered.config_file {
        return Err(
            "使用 CMR_DESKTOP_CONFIG 时必须同时设置 CMR_DESKTOP_STATE_DB，防止测试配置混用生产会话数据库。"
                .to_owned(),
        );
    }
    Ok(discovered.state_db)
}

fn main() {
    let store = config_store().expect("failed to resolve ModelRelay configuration path");
    thread::spawn(cleanup_legacy_install_directory);
    let app = tauri::Builder::default()
        .manage(DesktopState::new(store))
        .invoke_handler(tauri::generate_handler![
            dashboard_state,
            provider_setup_options,
            add_provider_with_model,
            complete_local_setup,
            set_service_running,
            set_model_visibility,
            reorder_models,
            check_remote_compatibility,
            remove_provider,
            delete_model,
            restore_codex_config,
            update_model,
            update_provider,
            open_releases_page,
            reveal_path
        ])
        .build(tauri::generate_context!())
        .expect("failed to build ModelRelay desktop manager");
    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit) {
            app_handle.state::<DesktopState>().stop_owned_on_exit();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        fs,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use anyhow::Result as AnyhowResult;
    use cmr_cli::service::CommandOutput;
    use cmr_storage::{MemoryCredentialStore, ModelConfig, ProviderConfig};

    use super::*;

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock after Unix epoch")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "cmr-desktop-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create temporary test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let belongs_to_tests = self
                .0
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("cmr-desktop-test-"));
            if belongs_to_tests {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn configured_state() -> (TestDirectory, DesktopState) {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let mut config = RouterConfig::default();
        // Never probe a developer's real default router while running unit tests.
        config.server.port = 0;
        config.providers.push(ProviderConfig {
            id: "zhipu".to_owned(),
            preset: "zhipu".to_owned(),
            base_url: None,
            secret_ref: Some("zhipu/default".to_owned()),
            enabled: true,
            allow_insecure_http: false,
        });
        for (id, order) in [("glm-a", 0), ("glm-b", 1)] {
            config.models.push(ModelConfig {
                id: id.to_owned(),
                display_name: id.to_owned(),
                provider: "zhipu".to_owned(),
                upstream_model: "glm-5.2".to_owned(),
                order,
                enabled: true,
                context_window: Some(1_000_000),
                max_output_tokens: Some(131_072),
            });
        }
        config.catalog_order = vec![
            "official-a".to_owned(),
            "glm-a".to_owned(),
            "official-b".to_owned(),
            "glm-b".to_owned(),
        ];
        store.save(&config).expect("seed configuration");
        (directory, DesktopState::new(store))
    }

    #[derive(Default)]
    struct OneClickRunnerState {
        installed: bool,
        definition: Option<String>,
        calls: Vec<(String, Vec<String>)>,
    }

    #[derive(Clone, Default)]
    struct OneClickRunner(Arc<Mutex<OneClickRunnerState>>);

    impl CommandRunner for OneClickRunner {
        fn run(&mut self, program: &OsStr, arguments: &[OsString]) -> AnyhowResult<CommandOutput> {
            let program = program.to_string_lossy().into_owned();
            let arguments = arguments
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut state = self.0.lock().expect("one-click runner lock");
            state.calls.push((program.clone(), arguments.clone()));
            if program == "whoami" {
                return Ok(CommandOutput::success("DOMAIN\\unit-test-user\r\n"));
            }
            if program != "schtasks" {
                return Ok(CommandOutput::failure("unexpected native program"));
            }
            match arguments.first().map(String::as_str) {
                Some("/Query") if arguments.iter().any(|value| value == "/XML") => {
                    if state.installed {
                        Ok(CommandOutput::success(
                            state.definition.clone().expect("registered definition"),
                        ))
                    } else {
                        Ok(CommandOutput::failure("not registered"))
                    }
                }
                Some("/Query") => Ok(CommandOutput::success("Status: Ready")),
                Some("/Create") => {
                    let xml_index = arguments
                        .iter()
                        .position(|value| value == "/XML")
                        .expect("XML argument");
                    let bytes =
                        fs::read(&arguments[xml_index + 1]).expect("read generated task XML");
                    assert_eq!(&bytes[..2], &[0xff, 0xfe]);
                    let units = bytes[2..]
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>();
                    let definition = String::from_utf16(&units).expect("decode generated task XML");
                    state.definition = Some(definition);
                    state.installed = true;
                    Ok(CommandOutput::success(""))
                }
                Some("/Run" | "/End") => Ok(CommandOutput::success("")),
                _ => Ok(CommandOutput::failure("unexpected schtasks arguments")),
            }
        }
    }

    struct FailingOneClickRunner;

    impl CommandRunner for FailingOneClickRunner {
        fn run(
            &mut self,
            _program: &OsStr,
            _arguments: &[OsString],
        ) -> AnyhowResult<CommandOutput> {
            Ok(CommandOutput::failure(
                "SENTINEL_SECRET_MUST_NEVER_REACH_WEBVIEW",
            ))
        }
    }

    fn setup_input(
        preset_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> AddProviderWithModelInput {
        AddProviderWithModelInput {
            provider_id: provider_id.to_owned(),
            preset_id: preset_id.to_owned(),
            base_url: if preset_id == "zhipu" {
                "https://open.bigmodel.cn/api/coding/paas/v4".to_owned()
            } else {
                "https://compatible.example.test/v1".to_owned()
            },
            api_key: "unit-test-secret-value".to_owned(),
            secret_profile: "default".to_owned(),
            model_id: model_id.to_owned(),
            upstream_model: if preset_id == "zhipu" {
                "glm-5.2".to_owned()
            } else {
                "compatible-model".to_owned()
            },
            display_name: model_id.to_owned(),
            context_window: None,
            max_output_tokens: None,
            enabled: true,
            allow_insecure_http: false,
        }
    }

    struct AlwaysConflictStore {
        inner: ConfigStore,
    }

    impl RevisionedConfigStore for AlwaysConflictStore {
        fn load_with_revision(&self) -> cmr_storage::Result<(RouterConfig, ConfigRevision)> {
            self.inner.load_with_revision()
        }

        fn save_if_revision(
            &self,
            _config: &RouterConfig,
            _expected: &ConfigRevision,
        ) -> cmr_storage::Result<ConfigCommitOutcome> {
            Err(StorageError::Conflict("injected conflict".to_owned()))
        }
    }

    #[derive(Default)]
    struct RecordingCredentialStore {
        inner: MemoryCredentialStore,
        staged: Mutex<Vec<SecretRef>>,
    }

    impl CredentialStore for RecordingCredentialStore {
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
            let reference = self.inner.stage_generation(provider, profile, secret)?;
            self.staged
                .lock()
                .expect("recording credential lock")
                .push(reference.clone());
            Ok(reference)
        }
    }

    #[test]
    fn blank_secret_profile_falls_back_to_the_default_slot() {
        assert_eq!(normalize_secret_profile(""), "default");
        assert_eq!(normalize_secret_profile("   "), "default");
        assert_eq!(normalize_secret_profile(" work "), "work");
    }

    #[test]
    fn setup_options_expose_zhipu_and_custom_defaults() {
        let options = provider_setup_options_document();
        let zhipu = options
            .presets
            .iter()
            .find(|preset| preset.id == "zhipu")
            .expect("zhipu preset");
        assert_eq!(zhipu.label, "Zhipu AI Coding Plan");
        assert_eq!(
            zhipu.default_base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(zhipu.default_model, "glm-5.2");
        assert!(zhipu.requires_api_key);
        assert_eq!(zhipu.context_window, Some(1_000_000));
        assert_eq!(zhipu.max_output_tokens, Some(131_072));
        assert!(
            options
                .presets
                .iter()
                .any(|preset| preset.id == "custom-compatible")
        );
    }

    #[test]
    fn one_click_setup_uses_isolated_codex_config_and_mocked_native_runner() {
        let (directory, state) = configured_state();
        let codex_config = directory.path().join("codex").join("config.toml");
        fs::create_dir_all(codex_config.parent().expect("Codex config parent"))
            .expect("create Codex config parent");
        fs::write(
            &codex_config,
            "[mcp_servers.kept]\ncommand = \"kept.exe\"\n",
        )
        .expect("seed isolated Codex config");
        let integration = CodexIntegration::new(
            &codex_config,
            Some(directory.path().join("codex-integration.json")),
            "127.0.0.1",
            15_722,
        )
        .expect("isolated integration");
        let router_executable = directory.path().join("cmr.exe");
        fs::write(&router_executable, b"test executable placeholder")
            .expect("seed router executable placeholder");
        let runner = OneClickRunner::default();
        let runner_state = Arc::clone(&runner.0);
        let expected = vec!["glm-a".to_owned(), "glm-b".to_owned()];
        let mut probes = 0_u8;

        let result = complete_local_setup_core(
            &state,
            &state.config.load().expect("load router config"),
            &router_executable,
            &directory.path().join("state.sqlite3"),
            &integration,
            runner,
            |_| {
                probes = probes.saturating_add(1);
                if probes == 1 {
                    HealthProbe::Stopped
                } else {
                    HealthProbe::Healthy(HealthDocument {
                        version: "test".to_owned(),
                        external_models: Vec::new(),
                        routable_external_models: expected.clone(),
                        official_catalog_cached: false,
                    })
                }
            },
        )
        .expect("complete isolated setup");

        assert!(result.integration_installed);
        assert!(result.service_installed);
        assert!(result.healthy);
        assert!(result.restart_chatgpt_required);
        assert!(result.picker_pending);
        assert!(!result.partial);
        assert_eq!(
            integration.status().expect("integration status"),
            IntegrationStatus::Installed
        );
        let merged = fs::read_to_string(&codex_config).expect("read merged config");
        assert!(merged.contains("[mcp_servers.kept]"));
        assert!(merged.contains("openai_base_url = \"http://127.0.0.1:15722/v1\""));
        assert!(merged.contains("remote_control = true"));
        let runner_state = runner_state.lock().expect("one-click runner state");
        assert!(runner_state.installed);
        assert!(
            runner_state
                .calls
                .iter()
                .all(|(program, _)| { program == "whoami" || program == "schtasks" })
        );
        assert!(
            runner_state
                .calls
                .iter()
                .all(|(program, _)| !program.eq_ignore_ascii_case("powershell.exe"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn repeated_setup_ignores_preserved_legacy_files_when_the_legacy_task_is_absent() {
        let directory = TestDirectory::new();
        let legacy_root = directory.path().join("CodexRouter");
        fs::create_dir_all(&legacy_root).expect("create preserved legacy directory");
        let paths = LegacyWindowsRouterPaths {
            powershell: directory.path().join("powershell.exe"),
            python: directory.path().join("pythonw.exe"),
            launcher: legacy_root.join("Start-Router.ps1"),
            router: legacy_root.join("router.py"),
        };
        for path in [
            &paths.powershell,
            &paths.python,
            &paths.launcher,
            &paths.router,
        ] {
            fs::write(path, b"preserved legacy file").expect("seed preserved legacy file");
        }
        let listener_probed = AtomicBool::new(false);
        let stop_called = AtomicBool::new(false);

        let result = migrate_known_legacy_windows_router_with(
            &directory.path().join("router.toml"),
            &directory.path().join("state.sqlite3"),
            &directory.path().join("cmr.exe"),
            OneClickRunner::default(),
            paths,
            |_, _| {
                // A repeated setup has the new CMR on 15722. If legacy process
                // inspection were reached, it would correctly reject that PID.
                listener_probed.store(true, Ordering::SeqCst);
                Err(())
            },
            |_, _, _| {
                stop_called.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("an absent legacy task must make repeated setup a no-op");

        assert!(result.is_none());
        assert!(!listener_probed.load(Ordering::SeqCst));
        assert!(!stop_called.load(Ordering::SeqCst));
        assert!(legacy_root.join("Start-Router.ps1").is_file());
        assert!(legacy_root.join("router.py").is_file());
    }

    #[test]
    fn one_click_setup_is_idempotent_for_an_installed_codex_integration() {
        let (directory, state) = configured_state();
        let codex_config = directory.path().join("codex-config.toml");
        let sidecar = directory.path().join("codex-state.json");
        let integration = CodexIntegration::new(&codex_config, Some(sidecar), "127.0.0.1", 15_722)
            .expect("isolated integration");
        integration.install().expect("initial integration install");
        let router_executable = directory.path().join("cmr.exe");
        fs::write(&router_executable, b"test executable placeholder")
            .expect("seed router executable placeholder");
        let runner = OneClickRunner::default();
        let expected = vec!["glm-a".to_owned(), "glm-b".to_owned()];
        let mut probes = 0_u8;

        let result = complete_local_setup_core(
            &state,
            &state.config.load().expect("load router config"),
            &router_executable,
            &directory.path().join("state.sqlite3"),
            &integration,
            runner,
            |_| {
                probes = probes.saturating_add(1);
                if probes == 1 {
                    HealthProbe::Stopped
                } else {
                    HealthProbe::Healthy(HealthDocument {
                        version: "test".to_owned(),
                        external_models: Vec::new(),
                        routable_external_models: expected.clone(),
                        official_catalog_cached: false,
                    })
                }
            },
        )
        .expect("repeat isolated setup");

        assert!(result.recovery_backup_path.is_none());
        assert!(!result.restart_chatgpt_required);
    }

    #[test]
    fn one_click_service_errors_are_redacted_before_serialization() {
        let (directory, state) = configured_state();
        let integration = CodexIntegration::new(
            directory.path().join("codex-config.toml"),
            Some(directory.path().join("codex-state.json")),
            "127.0.0.1",
            15_722,
        )
        .expect("isolated integration");
        let router_executable = directory.path().join("cmr.exe");
        fs::write(&router_executable, b"test executable placeholder")
            .expect("seed router executable placeholder");

        let failure = complete_local_setup_core(
            &state,
            &state.config.load().expect("load router config"),
            &router_executable,
            &directory.path().join("state.sqlite3"),
            &integration,
            FailingOneClickRunner,
            |_| HealthProbe::Stopped,
        )
        .expect_err("native runner failure must be returned");
        let serialized = serde_json::to_string(&failure).expect("serialize failure DTO");

        assert_eq!(failure.stage, "inspect-service");
        assert!(!serialized.contains("SENTINEL_SECRET_MUST_NEVER_REACH_WEBVIEW"));
    }

    #[test]
    fn zhipu_setup_uses_defaults_and_never_writes_key_to_toml() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let credentials = MemoryCredentialStore::default();
        let mut input = setup_input("zhipu", "zhipu", "glm-5.2");

        let config = add_provider_and_model(&store, &credentials, &mut input).expect("add zhipu");
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.id == "zhipu")
            .expect("zhipu provider");
        let model = config
            .models
            .iter()
            .find(|model| model.id == "glm-5.2")
            .expect("glm model");
        assert_eq!(provider.preset, "zhipu");
        assert_eq!(provider.base_url, None);
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(131_072));
        let reference = SecretRef::parse(provider.secret_ref.as_deref().expect("secret ref"))
            .expect("valid generation reference");
        assert!(reference.generation().is_some());
        assert_eq!(
            credentials.get(&reference).expect("read staged credential"),
            Some("unit-test-secret-value".to_owned())
        );
        assert!(input.api_key.is_empty());
        let toml = fs::read_to_string(store.path()).expect("read config text");
        assert!(!toml.contains("unit-test-secret-value"));
    }

    #[test]
    fn custom_compatible_setup_persists_validated_endpoint() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let mut initial = RouterConfig::default();
        initial.hidden_models.push("my-model".to_owned());
        store.save(&initial).expect("seed hidden model id");
        let credentials = MemoryCredentialStore::default();
        let mut input = setup_input("custom-compatible", "my-provider", "my-model");

        let config =
            add_provider_and_model(&store, &credentials, &mut input).expect("add custom provider");
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.id == "my-provider")
            .expect("custom provider");
        assert_eq!(provider.preset, "custom-compatible");
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://compatible.example.test/v1")
        );
        assert_eq!(config.models[0].upstream_model, "compatible-model");
        assert!(!config.hidden_models.contains(&"my-model".to_owned()));
    }

    #[test]
    fn custom_compatible_rejects_runtime_incompatible_provider_id_before_staging() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let credentials = RecordingCredentialStore::default();
        let mut input = setup_input("custom-compatible", "My.Provider", "my-model");

        let error = add_provider_and_model(&store, &credentials, &mut input)
            .expect_err("unsafe custom provider id must fail");
        assert!(error.contains("小写英文字母"));
        assert!(
            credentials
                .staged
                .lock()
                .expect("staged references")
                .is_empty()
        );
    }

    #[test]
    fn setup_rejects_zero_token_limits_and_does_not_duplicate_catalog_slots() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let credentials = MemoryCredentialStore::default();
        let mut invalid = setup_input("zhipu", "zhipu", "glm-5.2");
        invalid.context_window = Some(0);
        assert!(add_provider_and_model(&store, &credentials, &mut invalid).is_err());

        let mut initial = RouterConfig::default();
        initial.catalog_order.push("my-model".to_owned());
        store.save(&initial).expect("seed catalog slot");
        let mut valid = setup_input("custom-compatible", "my-provider", "my-model");
        let config =
            add_provider_and_model(&store, &credentials, &mut valid).expect("add custom provider");
        assert_eq!(
            config
                .catalog_order
                .iter()
                .filter(|id| *id == "my-model")
                .count(),
            1
        );
    }

    #[test]
    fn setup_rolls_back_every_staged_generation_after_cas_conflicts() {
        let directory = TestDirectory::new();
        let inner = ConfigStore::new(directory.path().join("router.toml"));
        inner.save(&RouterConfig::default()).expect("seed config");
        let store = AlwaysConflictStore { inner };
        let credentials = RecordingCredentialStore::default();
        let mut input = setup_input("zhipu", "zhipu", "glm-5.2");

        assert!(add_provider_and_model(&store, &credentials, &mut input).is_err());
        let staged = credentials.staged.lock().expect("staged references");
        assert_eq!(staged.len(), CONFIG_UPDATE_ATTEMPTS);
        for reference in staged.iter() {
            assert_eq!(
                credentials.get(reference).expect("read rolled back entry"),
                None
            );
        }
    }

    #[test]
    fn duplicate_provider_is_rejected_before_staging_a_key() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(directory.path().join("router.toml"));
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".to_owned(),
            preset: "zhipu".to_owned(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        store.save(&config).expect("seed duplicate provider");
        let credentials = RecordingCredentialStore::default();
        let mut input = setup_input("zhipu", "zhipu", "glm-5.2");

        let error = add_provider_and_model(&store, &credentials, &mut input)
            .expect_err("duplicate provider must fail");
        assert_eq!(error, "供应商 ID 已存在。");
        assert!(
            credentials
                .staged
                .lock()
                .expect("staged references")
                .is_empty()
        );
    }

    #[test]
    fn visibility_is_persisted_without_a_secret_value() {
        let (_directory, state) = configured_state();
        state
            .set_model_visibility("glm-a", false)
            .expect("hide model");
        let reloaded = state.config.load().expect("reload configuration");
        assert!(reloaded.hidden_models.contains(&"glm-a".to_owned()));
        assert_eq!(
            reloaded.providers[1].secret_ref.as_deref(),
            Some("zhipu/default")
        );
    }

    #[test]
    fn external_reorder_preserves_unknown_official_slots() {
        let (_directory, state) = configured_state();
        state
            .reorder_models(&["glm-b".to_owned(), "glm-a".to_owned()])
            .expect("reorder models");
        let reloaded = state.config.load().expect("reload configuration");
        assert_eq!(
            reloaded.catalog_order,
            vec![
                "official-a".to_owned(),
                "glm-b".to_owned(),
                "official-b".to_owned(),
                "glm-a".to_owned(),
            ]
        );
    }

    #[test]
    fn dashboard_comes_from_disk_and_marks_only_a_credential_reference() {
        let (_directory, state) = configured_state();
        let dashboard = state.dashboard().expect("dashboard");
        assert_eq!(dashboard.models.len(), 2);
        let zhipu = dashboard
            .providers
            .iter()
            .find(|provider| provider.id == "zhipu")
            .expect("zhipu provider");
        assert_eq!(zhipu.credential_status, "referenced");
        assert!(
            !dashboard.models[0]
                .capabilities
                .contains(&"vision".to_owned())
        );
    }

    #[test]
    fn health_parser_rejects_an_unrelated_service() {
        let valid = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            r#"{"status":"ok","service":"codex-model-router","version":"0.1.0","external_models":["glm-a"],"routable_external_models":["glm-b","glm-a"],"official_catalog_cached":true}"#
        );
        assert_eq!(
            parse_health_response(valid),
            Some(HealthDocument {
                version: "0.1.0".to_owned(),
                external_models: vec!["glm-a".to_owned()],
                routable_external_models: vec!["glm-a".to_owned(), "glm-b".to_owned()],
                official_catalog_cached: true,
            })
        );
        let unrelated = valid.replace("codex-model-router", "unrelated-service");
        assert!(parse_health_response(&unrelated).is_none());
    }

    #[test]
    fn local_ready_allows_picker_capacity_to_publish_a_routable_subset() {
        let expected = vec!["glm-a".to_owned(), "glm-b".to_owned()];
        let mut health = HealthDocument {
            version: "0.1.0".to_owned(),
            external_models: vec!["glm-a".to_owned()],
            routable_external_models: expected.clone(),
            official_catalog_cached: true,
        };
        assert!(picker_is_locally_ready(&health, &expected));

        health.external_models.clear();
        assert!(!picker_is_locally_ready(&health, &expected));
        health.external_models.push("glm-a".to_owned());
        health.official_catalog_cached = false;
        assert!(!picker_is_locally_ready(&health, &expected));
        health.official_catalog_cached = true;
        health.external_models = vec!["not-routable".to_owned()];
        assert!(!picker_is_locally_ready(&health, &expected));
    }

    #[test]
    fn published_models_require_enabled_provider_model_and_non_hidden_state() {
        let (_directory, state) = configured_state();
        let mut config = state.config.load().expect("load configuration");

        config.providers[1].enabled = false;
        assert!(model_summaries(&config).iter().all(|model| !model.visible));
        assert!(enabled_external_models(&config).is_empty());

        config.providers[1].enabled = true;
        config.hidden_models.push("glm-a".to_owned());
        config.models[1].enabled = false;
        let summaries = model_summaries(&config);
        assert!(
            !summaries
                .iter()
                .find(|model| model.id == "glm-a")
                .unwrap()
                .visible
        );
        assert!(
            !summaries
                .iter()
                .find(|model| model.id == "glm-b")
                .unwrap()
                .visible
        );
        assert!(enabled_external_models(&config).is_empty());

        config.hidden_models.clear();
        config.models[1].enabled = true;
        assert_eq!(
            enabled_external_models(&config),
            ["glm-a".to_owned(), "glm-b".to_owned()]
        );
    }

    #[test]
    fn managed_service_without_an_owned_child_never_claims_it_stopped_anything() {
        let mut managed = ManagedService::default();

        assert!(!managed.stop_owned().expect("no-op stop"));
        assert!(managed.child.is_none());
        assert!(managed.started_at.is_none());
    }

    #[test]
    fn managed_service_kills_and_reaps_its_owned_child() {
        #[cfg(windows)]
        let child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn controlled child");
        #[cfg(not(windows))]
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn controlled child");
        let mut managed = ManagedService {
            child: Some(child),
            started_at: Some(Instant::now()),
        };

        assert!(managed.stop_owned().expect("stop controlled child"));
        assert!(managed.child.is_none());
        assert!(managed.started_at.is_none());
    }

    #[test]
    fn config_update_retries_without_overwriting_a_concurrent_change() {
        let (_directory, state) = configured_state();
        let competing_store = state.config.clone();
        let mut injected_conflict = false;

        let updated = state
            .update_config(|config| {
                if !injected_conflict {
                    let mut competing = competing_store.load().expect("load competing config");
                    competing.hidden_models.push("concurrent-model".to_owned());
                    competing_store
                        .save(&competing)
                        .expect("save competing config");
                    injected_conflict = true;
                }
                config
                    .models
                    .iter_mut()
                    .find(|model| model.id == "glm-a")
                    .unwrap()
                    .enabled = false;
                Ok(())
            })
            .expect("retry config update");

        assert!(
            updated
                .hidden_models
                .contains(&"concurrent-model".to_owned())
        );
        assert!(
            !updated
                .models
                .iter()
                .find(|model| model.id == "glm-a")
                .unwrap()
                .enabled
        );
    }
}
