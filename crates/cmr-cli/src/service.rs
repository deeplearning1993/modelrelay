//! Per-user background-service registration.
//!
//! Service definitions are generated without invoking a shell. The command
//! runner and every filesystem path are injectable so unit tests never touch
//! the host service manager.

use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use directories::{BaseDirs, UserDirs};

const SERVICE_LABEL: &str = "io.github.codex-model-router";
const SYSTEMD_UNIT: &str = "codex-model-router.service";
const WINDOWS_TASK: &str = "ModelRelay";
const LEGACY_WINDOWS_TASK: &str = "Codex GLM Router";
const LEGACY_WINDOWS_DESCRIPTION: &str = "Codex GLM Router: login-start + crash-restart.";
const OWNERSHIP_MARKER: &str = "codex-model-router:managed:v1";
const WINDOWS_STOP_POLL_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    Windows,
    MacOs,
    Linux,
}

const SUPPORTED_PLATFORMS: [Platform; 3] = [Platform::Windows, Platform::MacOs, Platform::Linux];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationState {
    Present,
    Missing,
}

impl Platform {
    #[cfg(target_os = "windows")]
    const fn current() -> Self {
        Self::Windows
    }

    #[cfg(target_os = "macos")]
    const fn current() -> Self {
        Self::MacOs
    }

    #[cfg(target_os = "linux")]
    const fn current() -> Self {
        Self::Linux
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    const fn current() -> Self {
        compile_error!("cmr service management supports Windows, macOS, and Linux only");
    }
}

#[derive(Clone, Debug)]
struct ServiceContext {
    platform: Platform,
    executable: PathBuf,
    legacy_windows_executable: Option<PathBuf>,
    config: PathBuf,
    state_db: PathBuf,
    definition: PathBuf,
}

/// Captured result of a native service-manager command.
#[derive(Debug)]
pub struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandOutput {
    /// Builds a successful native command result for an injected runner.
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Builds a failed native command result for an injected runner.
    #[must_use]
    pub fn failure(stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Executes a service-manager command without a shell.
pub trait CommandRunner {
    /// Runs `program` with an already separated argument vector.
    ///
    /// # Errors
    ///
    /// Returns an error when the native program cannot be executed.
    fn run(&mut self, program: &OsStr, arguments: &[OsString]) -> Result<CommandOutput>;
}

/// Production command runner used by the CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &OsStr, arguments: &[OsString]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(arguments)
            .output()
            .with_context(|| format!("run {}", program.to_string_lossy()))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: decode_native_command_output(&output.stdout),
            stderr: decode_native_command_output(&output.stderr),
        })
    }
}

#[cfg(windows)]
fn decode_native_command_output(bytes: &[u8]) -> String {
    use local_encoding_ng::{Encoder as _, Encoding};

    // Windows console programs such as `schtasks.exe` write redirected output
    // in the active OEM code page, not UTF-8. Decode through the OS code-page
    // APIs so localized not-found diagnostics retain their semantic markers.
    // Some Windows tools emit actual UTF-8 (notably structured/XML modes), so
    // preserve a strictly valid UTF-8 stream before trying the OEM code page.
    std::str::from_utf8(bytes).map_or_else(
        |_| {
            Encoding::OEM
                .to_string(bytes)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
        },
        str::to_owned,
    )
}

#[cfg(not(windows))]
fn decode_native_command_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Registration state reported by the native per-user service manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceStatus {
    /// A native service registration exists.
    Installed,
    /// No native service registration exists.
    NotInstalled,
}

/// Recovery record for the one legacy Windows task that predates this project.
///
/// The legacy task is never treated as generally owned. A record can only be
/// produced after its complete action and fixed legacy markers were verified.
#[derive(Clone, Debug)]
pub struct LegacyWindowsTaskBackup {
    definition: String,
    definition_path: PathBuf,
}

impl LegacyWindowsTaskBackup {
    /// Returns the durable XML recovery copy written before task removal.
    #[must_use]
    pub fn definition_path(&self) -> &Path {
        &self.definition_path
    }
}

/// Installs, removes, and queries one per-user router service.
pub struct ServiceManager<R> {
    context: ServiceContext,
    runner: R,
}

impl<R: CommandRunner> ServiceManager<R> {
    /// Resolves production service paths while retaining an injectable runner.
    ///
    /// # Errors
    ///
    /// Returns an error when per-user paths or the executable cannot be resolved.
    pub fn discover(config: &Path, state_db: &Path, runner: R) -> Result<Self> {
        let current = std::env::current_exe().context("resolve the cmr executable")?;
        let executable = service_executable_for_cli(&current)?;
        Self::discover_with_executable(config, state_db, executable, runner)
    }

    /// Resolves production service paths with an explicit router executable.
    ///
    /// Desktop applications must use this constructor so the persistent
    /// service action points to the `cmr` router binary rather than the desktop
    /// manager executable. The runner remains injectable for isolated tests.
    ///
    /// # Errors
    ///
    /// Returns an error when per-user service directories cannot be resolved.
    pub fn discover_with_executable(
        config: &Path,
        state_db: &Path,
        executable: impl Into<PathBuf>,
        runner: R,
    ) -> Result<Self> {
        let platform = Platform::current();
        debug_assert!(SUPPORTED_PLATFORMS.contains(&platform));
        let current_dir = std::env::current_dir().context("resolve the current directory")?;
        let config = lexical_absolute(config, &current_dir);
        let state_db = lexical_absolute(state_db, &current_dir);
        let executable = lexical_absolute(&executable.into(), &current_dir);
        let legacy_windows_executable = legacy_windows_executable(&executable);
        let context = ServiceContext {
            platform,
            executable,
            legacy_windows_executable,
            config,
            definition: definition_path(platform, &state_db)?,
            state_db,
        };
        Ok(Self { context, runner })
    }

    #[cfg(test)]
    fn new(context: ServiceContext, runner: R) -> Self {
        Self { context, runner }
    }

    /// Returns the native service definition path.
    pub fn definition_path(&self) -> &Path {
        &self.context.definition
    }

    /// Writes the native definition, registers it, and starts the router.
    ///
    /// # Errors
    ///
    /// Returns an error when ownership verification or native registration fails.
    pub fn install(&mut self) -> Result<()> {
        match self.context.platform {
            Platform::Windows => self.install_windows(),
            Platform::MacOs => self.install_macos(),
            Platform::Linux => self.install_linux(),
        }
    }

    /// Stops and unregisters the service, returning whether any state existed.
    ///
    /// # Errors
    ///
    /// Returns an error when ownership verification or native removal fails.
    pub fn uninstall(&mut self) -> Result<bool> {
        match self.context.platform {
            Platform::Windows => self.uninstall_windows(),
            Platform::MacOs => self.uninstall_macos(),
            Platform::Linux => self.uninstall_linux(),
        }
    }

    /// Queries registration from the native service manager.
    ///
    /// # Errors
    ///
    /// Returns an error when native status or ownership verification fails.
    pub fn status(&mut self) -> Result<ServiceStatus> {
        match self.context.platform {
            Platform::Windows => self.status_windows(),
            Platform::MacOs => {
                let domain = self.macos_domain()?;
                self.status_macos(&domain)
            }
            Platform::Linux => self.status_linux(),
        }
    }

    /// Checks whether the exact, known `Codex GLM Router` predecessor task is
    /// registered for the current Windows user without modifying it.
    ///
    /// A same-name task with any unrecognized owner or action is reported as an
    /// error rather than as absent. Callers can therefore safely short-circuit
    /// legacy file and process inspection only when this method returns
    /// `false`.
    ///
    /// # Errors
    ///
    /// Returns an error when the task cannot be queried or its complete known
    /// identity cannot be verified.
    pub fn exact_legacy_windows_task_present(
        &mut self,
        powershell_executable: &Path,
        launcher: &Path,
    ) -> Result<bool> {
        if self.context.platform != Platform::Windows {
            return Ok(false);
        }
        let Some(definition) = self.query_windows_definition_named(LEGACY_WINDOWS_TASK)? else {
            return Ok(false);
        };
        let current_user_sid = self.current_windows_user_sid()?;
        let current_user_identity = self.current_windows_identity()?;
        verify_exact_legacy_windows_definition(
            &definition,
            powershell_executable,
            launcher,
            &current_user_sid,
            &current_user_identity,
        )?;
        Ok(true)
    }

    /// Removes the exact, known `Codex GLM Router` predecessor task after
    /// writing and re-reading a recovery XML file.
    ///
    /// `powershell_executable` and `launcher` are supplied by the caller so it
    /// can independently fingerprint the legacy files and current-user paths.
    /// No other task name or action is accepted. The detached legacy Python
    /// child is deliberately not terminated here; callers must independently
    /// identify that process before stopping it.
    ///
    /// # Errors
    ///
    /// Returns an error without deleting the task if the registered XML is not
    /// the exact recognized legacy action, if the recovery copy cannot be
    /// verified, or if Task Scheduler changes the task concurrently.
    pub fn remove_exact_legacy_windows_task(
        &mut self,
        powershell_executable: &Path,
        launcher: &Path,
    ) -> Result<Option<LegacyWindowsTaskBackup>> {
        if self.context.platform != Platform::Windows {
            return Ok(None);
        }
        let Some(definition) = self.query_windows_definition_named(LEGACY_WINDOWS_TASK)? else {
            return Ok(None);
        };
        let current_user_sid = self.current_windows_user_sid()?;
        let current_user_identity = self.current_windows_identity()?;
        verify_exact_legacy_windows_definition(
            &definition,
            powershell_executable,
            launcher,
            &current_user_sid,
            &current_user_identity,
        )?;

        let backup_path = self.write_legacy_windows_backup(&definition)?;
        if self.windows_task_is_active_named(LEGACY_WINDOWS_TASK)? {
            let end = vec![
                OsString::from("/End"),
                OsString::from("/TN"),
                OsString::from(LEGACY_WINDOWS_TASK),
            ];
            self.run_required("schtasks", &end)?;
            self.wait_for_windows_task_to_stop_named(LEGACY_WINDOWS_TASK)?;
        }

        let reverified = self
            .query_windows_definition_named(LEGACY_WINDOWS_TASK)?
            .context("legacy scheduled task disappeared before ownership could be re-verified")?;
        verify_exact_legacy_windows_definition(
            &reverified,
            powershell_executable,
            launcher,
            &current_user_sid,
            &current_user_identity,
        )?;
        if reverified != definition {
            bail!("legacy scheduled task changed concurrently; recovery removal was cancelled");
        }

        let delete = vec![
            OsString::from("/Delete"),
            OsString::from("/TN"),
            OsString::from(LEGACY_WINDOWS_TASK),
            OsString::from("/F"),
        ];
        self.run_required("schtasks", &delete)?;
        if self
            .query_windows_definition_named(LEGACY_WINDOWS_TASK)?
            .is_some()
        {
            bail!("legacy scheduled task still exists after deletion");
        }

        Ok(Some(LegacyWindowsTaskBackup {
            definition,
            definition_path: backup_path,
        }))
    }

    /// Restores a legacy task from a recovery record produced by
    /// [`Self::remove_exact_legacy_windows_task`].
    ///
    /// This is intended only for rolling back a failed in-process migration.
    /// The action is revalidated from both the record and its durable file
    /// before Task Scheduler is modified.
    ///
    /// # Errors
    ///
    /// Returns an error if the recovery copy changed, a different same-name
    /// task now exists, or Task Scheduler cannot recreate and verify the task.
    pub fn restore_exact_legacy_windows_task(
        &mut self,
        backup: &LegacyWindowsTaskBackup,
        powershell_executable: &Path,
        launcher: &Path,
    ) -> Result<()> {
        if self.context.platform != Platform::Windows {
            bail!("legacy Windows task recovery is only available on Windows");
        }
        let current_user_sid = self.current_windows_user_sid()?;
        let current_user_identity = self.current_windows_identity()?;
        verify_exact_legacy_windows_definition(
            &backup.definition,
            powershell_executable,
            launcher,
            &current_user_sid,
            &current_user_identity,
        )?;
        let bytes = fs::read(&backup.definition_path).with_context(|| {
            format!(
                "read legacy task recovery definition {}",
                backup.definition_path.display()
            )
        })?;
        let durable = decode_service_definition(Platform::Windows, &bytes)?;
        if durable != backup.definition {
            bail!("legacy task recovery definition changed after it was written");
        }

        if let Some(existing) = self.query_windows_definition_named(LEGACY_WINDOWS_TASK)? {
            verify_exact_legacy_windows_definition(
                &existing,
                powershell_executable,
                launcher,
                &current_user_sid,
                &current_user_identity,
            )?;
            if existing == backup.definition {
                return Ok(());
            }
            bail!("a different same-name legacy task appeared during migration rollback");
        }

        let create = vec![
            OsString::from("/Create"),
            OsString::from("/TN"),
            OsString::from(LEGACY_WINDOWS_TASK),
            OsString::from("/XML"),
            backup.definition_path.as_os_str().to_os_string(),
            OsString::from("/F"),
        ];
        self.run_required("schtasks", &create)?;
        let restored = self
            .query_windows_definition_named(LEGACY_WINDOWS_TASK)?
            .context("legacy scheduled task was not present after recovery")?;
        verify_exact_legacy_windows_definition(
            &restored,
            powershell_executable,
            launcher,
            &current_user_sid,
            &current_user_identity,
        )?;
        if restored != backup.definition {
            bail!("Task Scheduler did not restore the exact legacy task definition");
        }
        // Desktop migration only calls this recovery path after it has verified
        // a live detached legacy router. The task itself commonly reports
        // `Ready`, so recovery must run it even when `/End` was unnecessary.
        let start = vec![
            OsString::from("/Run"),
            OsString::from("/TN"),
            OsString::from(LEGACY_WINDOWS_TASK),
        ];
        self.run_required("schtasks", &start)?;
        Ok(())
    }

    fn install_windows(&mut self) -> Result<()> {
        let identity = self.run_required("whoami", &[])?.stdout.trim().to_owned();
        if identity.is_empty() || identity.contains('\r') || identity.contains('\n') {
            bail!("whoami returned an invalid current-user identity");
        }
        let definition = render_windows_task(&self.context, &identity)?;
        if let Some(existing) = self.query_windows_definition()? {
            self.verify_windows_owned_definition(&existing)?;
            if self.windows_task_is_active()? {
                let end = vec![
                    OsString::from("/End"),
                    OsString::from("/TN"),
                    OsString::from(WINDOWS_TASK),
                ];
                self.run_required("schtasks", &end)?;
                self.wait_for_windows_task_to_stop()?;
            }
            let stopped = self
                .query_windows_definition()?
                .context("scheduled task disappeared before ownership could be re-verified")?;
            self.verify_windows_owned_definition(&stopped)?;
        }
        self.verify_local_definition_if_present()?;
        self.write_definition(&definition)?;
        let create = vec![
            OsString::from("/Create"),
            OsString::from("/TN"),
            OsString::from(WINDOWS_TASK),
            OsString::from("/XML"),
            self.context.definition.as_os_str().to_os_string(),
            OsString::from("/F"),
        ];
        self.run_required("schtasks", &create)?;
        let registered = self
            .query_windows_definition()?
            .context("scheduled task was not present after creation")?;
        self.verify_windows_owned_definition(&registered)?;
        let start = vec![
            OsString::from("/Run"),
            OsString::from("/TN"),
            OsString::from(WINDOWS_TASK),
        ];
        self.run_required("schtasks", &start)?;
        Ok(())
    }

    fn uninstall_windows(&mut self) -> Result<bool> {
        let registered = if let Some(definition) = self.query_windows_definition()? {
            self.verify_windows_owned_definition(&definition)?;
            true
        } else {
            false
        };
        if registered {
            if self.windows_task_is_active()? {
                let end = vec![
                    OsString::from("/End"),
                    OsString::from("/TN"),
                    OsString::from(WINDOWS_TASK),
                ];
                self.run_required("schtasks", &end)?;
                self.wait_for_windows_task_to_stop()?;
            }

            // Re-query the exact XML after the process has stopped so a
            // concurrently replaced task cannot be deleted under this process.
            let stopped_definition = self
                .query_windows_definition()?
                .context("scheduled task disappeared before ownership could be re-verified")?;
            self.verify_windows_owned_definition(&stopped_definition)?;

            let delete = vec![
                OsString::from("/Delete"),
                OsString::from("/TN"),
                OsString::from(WINDOWS_TASK),
                OsString::from("/F"),
            ];
            self.run_required("schtasks", &delete)?;
            if self.query_windows_definition()?.is_some() {
                bail!("scheduled task still exists after deletion");
            }
        }
        self.verify_local_definition_if_present()?;
        let definition_existed = self.remove_definition()?;
        Ok(registered || definition_existed)
    }

    fn status_windows(&mut self) -> Result<ServiceStatus> {
        let Some(definition) = self.query_windows_definition()? else {
            return Ok(ServiceStatus::NotInstalled);
        };
        self.verify_windows_owned_definition(&definition)?;
        Ok(ServiceStatus::Installed)
    }

    fn wait_for_windows_task_to_stop(&mut self) -> Result<()> {
        self.wait_for_windows_task_to_stop_named(WINDOWS_TASK)
    }

    fn wait_for_windows_task_to_stop_named(&mut self, task_name: &str) -> Result<()> {
        for poll in 0..=WINDOWS_STOP_POLL_LIMIT {
            if !self.windows_task_is_active_named(task_name)? {
                return Ok(());
            }
            if poll == WINDOWS_STOP_POLL_LIMIT {
                bail!("scheduled task did not stop after /End");
            }
            wait_before_windows_stop_poll();
        }
        unreachable!("the bounded Windows stop loop always returns or fails")
    }

    fn windows_task_is_active(&mut self) -> Result<bool> {
        self.windows_task_is_active_named(WINDOWS_TASK)
    }

    fn windows_task_is_active_named(&mut self, task_name: &str) -> Result<bool> {
        let arguments = vec![
            OsString::from("/Query"),
            OsString::from("/TN"),
            OsString::from(task_name),
            OsString::from("/FO"),
            OsString::from("CSV"),
            OsString::from("/NH"),
        ];

        let output = self.run_required("schtasks", &arguments)?;
        let state = parse_windows_task_state(&output.stdout)?;
        if matches!(state.as_str(), "running" | "queued" | "正在运行" | "排队") {
            return Ok(true);
        }
        if matches!(state.as_str(), "ready" | "disabled" | "就绪" | "已禁用") {
            return Ok(false);
        }
        bail!("schtasks returned an unknown scheduled-task state")
    }

    fn install_macos(&mut self) -> Result<()> {
        let domain = self.macos_domain()?;
        let target = format!("{domain}/{SERVICE_LABEL}");
        if let Some(registered) = self.query_macos_registration(&target)? {
            self.verify_macos_registration(&registered)?;
            self.verify_local_definition_if_present()?;
            if !self.context.definition.exists() {
                bail!("refusing to replace a launch agent whose ownership cannot be verified");
            }
            let unload = vec![OsString::from("bootout"), OsString::from(&target)];
            self.run_required("launchctl", &unload)?;
        }
        self.verify_local_definition_if_present()?;
        let definition = render_launch_agent(&self.context)?;
        self.write_definition(&definition)?;
        let load = vec![
            OsString::from("bootstrap"),
            OsString::from(&domain),
            self.context.definition.as_os_str().to_os_string(),
        ];
        self.run_required("launchctl", &load)?;
        let registered = self
            .query_macos_registration(&target)?
            .context("launch agent was not registered after bootstrap")?;
        self.verify_macos_registration(&registered)?;
        Ok(())
    }

    fn uninstall_macos(&mut self) -> Result<bool> {
        let domain = self.macos_domain()?;
        let registered = self.status_macos(&domain)? == ServiceStatus::Installed;
        if registered {
            self.verify_local_definition_if_present()?;
            let target = format!("{domain}/{SERVICE_LABEL}");
            let arguments = vec![OsString::from("bootout"), OsString::from(target)];
            self.run_required("launchctl", &arguments)?;
        }
        self.verify_local_definition_if_present()?;
        let definition_existed = self.remove_definition()?;
        Ok(registered || definition_existed)
    }

    fn status_macos(&mut self, domain: &str) -> Result<ServiceStatus> {
        let target = format!("{domain}/{SERVICE_LABEL}");
        match self.query_macos_registration(&target)? {
            Some(registered) => {
                self.verify_macos_registration(&registered)?;
                self.verify_local_definition_if_present()?;
                if !self.context.definition.exists() {
                    bail!("launch agent is registered but its ownership file is missing");
                }
                Ok(ServiceStatus::Installed)
            }
            None => Ok(ServiceStatus::NotInstalled),
        }
    }

    fn macos_domain(&mut self) -> Result<String> {
        let arguments = vec![OsString::from("-u")];
        let output = self.run_required("id", &arguments)?;
        let uid = output.stdout.trim();
        if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("id -u returned an invalid user id");
        }
        Ok(format!("gui/{uid}"))
    }

    fn install_linux(&mut self) -> Result<()> {
        let status = self.status_linux_registration()?;
        if status == RegistrationState::Present {
            if !self.context.definition.exists() {
                bail!("refusing to replace a systemd unit whose ownership cannot be verified");
            }
            let registered = self.query_linux_definition()?;
            self.verify_owned_definition(&registered)?;
        }
        self.verify_local_definition_if_present()?;
        let definition = render_systemd_unit(&self.context)?;
        self.write_definition(&definition)?;
        let user = vec![OsString::from("--user"), OsString::from("daemon-reload")];
        self.run_required("systemctl", &user)?;
        let enable = vec![
            OsString::from("--user"),
            OsString::from("enable"),
            OsString::from("--now"),
            OsString::from(SYSTEMD_UNIT),
        ];
        self.run_required("systemctl", &enable)?;
        if self.status_linux()? != ServiceStatus::Installed {
            bail!("systemd unit was not installed after enable --now");
        }
        Ok(())
    }

    fn uninstall_linux(&mut self) -> Result<bool> {
        let registered = self.status_linux()? == ServiceStatus::Installed;
        if registered {
            self.verify_local_definition_if_present()?;
            let disable = vec![
                OsString::from("--user"),
                OsString::from("disable"),
                OsString::from("--now"),
                OsString::from(SYSTEMD_UNIT),
            ];
            self.run_required("systemctl", &disable)?;
        }
        self.verify_local_definition_if_present()?;
        let definition_existed = self.remove_definition()?;
        if registered || definition_existed {
            let reload = vec![OsString::from("--user"), OsString::from("daemon-reload")];
            self.run_required("systemctl", &reload)?;
        }
        Ok(registered || definition_existed)
    }

    fn status_linux(&mut self) -> Result<ServiceStatus> {
        match self.status_linux_registration()? {
            RegistrationState::Present => {
                self.verify_local_definition_if_present()?;
                if !self.context.definition.exists() {
                    bail!("systemd unit is registered but its ownership file is missing");
                }
                let registered = self.query_linux_definition()?;
                self.verify_owned_definition(&registered)?;
                Ok(ServiceStatus::Installed)
            }
            RegistrationState::Missing => Ok(ServiceStatus::NotInstalled),
        }
    }

    fn status_linux_registration(&mut self) -> Result<RegistrationState> {
        let arguments = vec![
            OsString::from("--user"),
            OsString::from("is-enabled"),
            OsString::from(SYSTEMD_UNIT),
        ];
        self.run_status("systemctl", &arguments)
    }

    fn query_linux_definition(&mut self) -> Result<String> {
        let arguments = vec![
            OsString::from("--user"),
            OsString::from("cat"),
            OsString::from(SYSTEMD_UNIT),
        ];
        let output = self.run_required("systemctl", &arguments)?;
        if output.stdout.trim().is_empty() {
            bail!("systemctl returned an empty registered unit definition");
        }
        Ok(output.stdout)
    }

    fn query_macos_registration(&mut self, target: &str) -> Result<Option<String>> {
        let arguments = vec![OsString::from("print"), OsString::from(target)];
        let output = self.runner.run(OsStr::new("launchctl"), &arguments)?;
        if output.success {
            if output.stdout.trim().is_empty() {
                bail!("launchctl returned an empty registered agent definition");
            }
            return Ok(Some(output.stdout));
        }
        if output_is_not_found(&output) {
            return Ok(None);
        }
        bail_command_failure("launchctl", &output)
    }

    fn verify_macos_registration(&self, definition: &str) -> Result<()> {
        if !definition.contains(OWNERSHIP_MARKER)
            || !launch_agent_registration_action_matches(definition, &self.context)?
        {
            bail!(
                "refusing to manage an existing service with an unknown owner or different action"
            );
        }
        Ok(())
    }

    fn query_windows_definition(&mut self) -> Result<Option<String>> {
        self.query_windows_definition_named(WINDOWS_TASK)
    }

    fn query_windows_definition_named(&mut self, task_name: &str) -> Result<Option<String>> {
        let arguments = vec![
            OsString::from("/Query"),
            OsString::from("/TN"),
            OsString::from(task_name),
            OsString::from("/XML"),
        ];
        let output = self.runner.run(OsStr::new("schtasks"), &arguments)?;
        if output.success {
            if output.stdout.trim().is_empty() {
                bail!("schtasks returned an empty task definition");
            }
            return Ok(Some(output.stdout));
        }
        if output_is_not_found(&output) {
            return Ok(None);
        }
        bail_command_failure("schtasks", &output)
    }

    fn current_windows_user_sid(&mut self) -> Result<String> {
        let arguments = [
            OsString::from("/user"),
            OsString::from("/fo"),
            OsString::from("csv"),
            OsString::from("/nh"),
        ];
        let output = self.run_required("whoami", &arguments)?;
        parse_whoami_sid(&output.stdout)
    }

    fn current_windows_identity(&mut self) -> Result<String> {
        let identity = self.run_required("whoami", &[])?.stdout.trim().to_owned();
        if identity.is_empty() || identity.contains('\r') || identity.contains('\n') {
            bail!("whoami returned an invalid current-user identity");
        }
        Ok(identity)
    }

    fn write_legacy_windows_backup(&self, definition: &str) -> Result<PathBuf> {
        let parent = self
            .context
            .definition
            .parent()
            .context("service definition has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create service recovery directory {}", parent.display()))?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock predates the Unix epoch")?
            .as_nanos();
        let path = parent.join(format!("legacy-codex-glm-router-{nonce}.xml"));
        let bytes = encode_service_definition(Platform::Windows, definition);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create legacy task recovery file {}", path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write legacy task recovery file {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("flush legacy task recovery file {}", path.display()))?;
        drop(file);

        let verified = fs::read(&path)
            .with_context(|| format!("re-read legacy task recovery file {}", path.display()))?;
        let verified = decode_service_definition(Platform::Windows, &verified)?;
        if verified != definition {
            bail!("legacy task recovery file failed read-after-write verification");
        }
        Ok(path)
    }

    fn verify_local_definition_if_present(&mut self) -> Result<()> {
        if !self.context.definition.exists() {
            return Ok(());
        }
        let bytes = fs::read(&self.context.definition).with_context(|| {
            format!(
                "read service definition {}",
                self.context.definition.display()
            )
        })?;
        let definition =
            decode_service_definition(self.context.platform, &bytes).with_context(|| {
                format!(
                    "decode service definition {}",
                    self.context.definition.display()
                )
            })?;
        if self.context.platform == Platform::Windows {
            self.verify_windows_owned_definition(&definition)
        } else {
            self.verify_owned_definition(&definition)
        }
    }

    fn verify_windows_owned_definition(&mut self, definition: &str) -> Result<()> {
        if definition.contains(OWNERSHIP_MARKER)
            && windows_action_matches(definition, &self.context)?
        {
            return Ok(());
        }
        if definition.contains(OWNERSHIP_MARKER)
            && legacy_windows_action_matches(definition, &self.context)?
        {
            let current_user_identity = self.current_windows_identity()?;
            let current_user_sid = self.current_windows_user_sid()?;
            if windows_definition_matches_current_user(
                definition,
                &current_user_sid,
                &current_user_identity,
            )? {
                return Ok(());
            }
        }
        bail!("refusing to manage an existing service with an unknown owner or different action")
    }

    fn verify_owned_definition(&self, definition: &str) -> Result<()> {
        let owned = definition.contains(OWNERSHIP_MARKER)
            && match self.context.platform {
                Platform::Windows => windows_action_matches(definition, &self.context)?,
                Platform::MacOs => launch_agent_action_matches(definition, &self.context)?,
                Platform::Linux => systemd_action_matches(definition, &self.context)?,
            };
        if !owned {
            bail!(
                "refusing to manage an existing service with an unknown owner or different action"
            );
        }
        Ok(())
    }

    fn write_definition(&self, contents: &str) -> Result<()> {
        let parent = self
            .context
            .definition
            .parent()
            .context("service definition has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create service directory {}", parent.display()))?;
        let encoded = encode_service_definition(self.context.platform, contents);
        fs::write(&self.context.definition, encoded).with_context(|| {
            format!(
                "write service definition {}",
                self.context.definition.display()
            )
        })
    }

    fn remove_definition(&self) -> Result<bool> {
        if !self.context.definition.exists() {
            return Ok(false);
        }
        fs::remove_file(&self.context.definition).with_context(|| {
            format!(
                "remove service definition {}",
                self.context.definition.display()
            )
        })?;
        Ok(true)
    }

    fn run_status(&mut self, program: &str, arguments: &[OsString]) -> Result<RegistrationState> {
        let output = self.runner.run(OsStr::new(program), arguments)?;
        if output.success {
            return Ok(RegistrationState::Present);
        }
        if output_is_not_found(&output) {
            return Ok(RegistrationState::Missing);
        }
        bail_command_failure(program, &output)
    }

    fn run_required(&mut self, program: &str, arguments: &[OsString]) -> Result<CommandOutput> {
        let output = self.runner.run(OsStr::new(program), arguments)?;
        if !output.success {
            let detail = output.stderr.trim();
            if detail.is_empty() {
                bail!("{program} failed while managing the per-user router service");
            }
            bail!("{program} failed while managing the per-user router service: {detail}");
        }
        Ok(output)
    }
}

fn parse_whoami_sid(stdout: &str) -> Result<String> {
    let line = stdout
        .trim_start_matches('\u{feff}')
        .trim()
        .lines()
        .next()
        .context("whoami returned no current-user SID")?;
    if stdout.trim().lines().count() != 1 {
        bail!("whoami returned an ambiguous current-user SID");
    }
    let (_, sid) = line
        .rsplit_once(',')
        .context("whoami returned an invalid current-user SID record")?;
    let sid = sid.trim().trim_matches('"');
    if !sid.starts_with("S-1-5-")
        || sid
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && byte != b'-' && byte != b'S')
    {
        bail!("whoami returned an invalid current-user SID");
    }
    Ok(sid.to_owned())
}

fn parse_windows_task_state(stdout: &str) -> Result<String> {
    let mut lines = stdout
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let line = lines
        .next()
        .context("schtasks returned no scheduled-task state")?;
    if lines.next().is_some() {
        bail!("schtasks returned an ambiguous scheduled-task state");
    }
    let state = if line.contains("\",\"") {
        line.rsplit("\",\"")
            .next()
            .context("schtasks returned an invalid CSV state record")?
            .trim_matches('"')
    } else {
        line.trim_matches('"')
    };
    if state.is_empty() || state.contains(',') {
        bail!("schtasks returned an invalid scheduled-task state");
    }
    Ok(state.to_lowercase())
}

fn verify_exact_legacy_windows_definition(
    definition: &str,
    powershell_executable: &Path,
    launcher: &Path,
    current_user_sid: &str,
    current_user_identity: &str,
) -> Result<()> {
    let working_directory = launcher
        .parent()
        .context("legacy launcher has no parent directory")?;
    let powershell = escape_xml(path_text(powershell_executable)?)?;
    let launcher = path_text(launcher)?;
    let working_directory = escape_xml(path_text(working_directory)?)?;
    let current_user_sid = escape_xml(current_user_sid)?;
    let current_user_identity = escape_xml(current_user_identity)?;
    let raw_arguments = format!(
        "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{launcher}\""
    );
    let arguments = escape_xml(&raw_arguments)?;
    let lowered = definition.to_ascii_lowercase();
    let required = [
        format!("<Description>{LEGACY_WINDOWS_DESCRIPTION}</Description>"),
        format!("<URI>\\{LEGACY_WINDOWS_TASK}</URI>"),
        format!("<Command>{powershell}</Command>"),
        format!("<WorkingDirectory>{working_directory}</WorkingDirectory>"),
        format!("<UserId>{current_user_sid}</UserId>"),
        format!("<UserId>{current_user_identity}</UserId>"),
        "<LogonType>InteractiveToken</LogonType>".to_owned(),
        "<LogonTrigger>".to_owned(),
    ];
    if required
        .iter()
        .any(|marker| !lowered.contains(&marker.to_ascii_lowercase()))
        || (!lowered.contains(&format!(
            "<arguments>{}</arguments>",
            arguments.to_ascii_lowercase()
        )) && !lowered.contains(&format!(
            "<arguments>{}</arguments>",
            raw_arguments.to_ascii_lowercase()
        )))
        || lowered.matches("<actions").count() != 1
        || lowered.matches("<exec>").count() != 1
        || lowered.matches("<command>").count() != 1
        || lowered.matches("<arguments>").count() != 1
        || lowered.matches("<workingdirectory>").count() != 1
        || lowered.matches("<userid>").count() != 2
        || lowered.matches("<runlevel>").count() > 1
        || (lowered.contains("<runlevel>")
            && !lowered.contains("<runlevel>leastprivilege</runlevel>"))
    {
        bail!("refusing to migrate an unrecognized or non-current-user legacy scheduled task");
    }
    Ok(())
}

fn encode_service_definition(platform: Platform, contents: &str) -> Vec<u8> {
    if platform != Platform::Windows {
        return contents.as_bytes().to_vec();
    }

    // `schtasks /Create /XML` imports task files through the Task Scheduler's
    // UTF-16 XML reader. A UTF-8 file, even with a matching XML declaration,
    // is rejected on supported Windows releases as "cannot switch encoding".
    let contents = contents.replacen(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
        1,
    );
    let mut encoded = Vec::with_capacity(2 + contents.len() * 2);
    encoded.extend_from_slice(&[0xff, 0xfe]);
    for unit in contents.encode_utf16() {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    encoded
}

fn decode_service_definition(platform: Platform, bytes: &[u8]) -> Result<String> {
    if platform != Platform::Windows {
        return String::from_utf8(bytes.to_vec()).context("service definition is not UTF-8");
    }

    let (little_endian, payload) = if let Some(payload) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (true, payload)
    } else if let Some(payload) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (false, payload)
    } else {
        // Accept UTF-8 definitions left by 0.1.0/early 0.1.1 so a repaired
        // release can verify and replace them instead of stranding ownership.
        return String::from_utf8(bytes.to_vec())
            .context("Windows service definition is neither UTF-16 nor legacy UTF-8");
    };
    if payload.len() % 2 != 0 {
        bail!("Windows UTF-16 service definition has an odd byte length");
    }
    let units = payload
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).context("Windows service definition is not valid UTF-16")
}

fn wait_before_windows_stop_poll() {
    #[cfg(not(test))]
    std::thread::sleep(std::time::Duration::from_millis(100));
}

fn definition_path(platform: Platform, state_db: &Path) -> Result<PathBuf> {
    match platform {
        Platform::Windows => {
            let data_dir = state_db.parent().unwrap_or_else(|| Path::new("."));
            Ok(data_dir.join("service").join("codex-model-router-task.xml"))
        }
        Platform::MacOs => {
            let user = UserDirs::new().context("resolve the current user's home directory")?;
            Ok(user
                .home_dir()
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")))
        }
        Platform::Linux => {
            let base = BaseDirs::new().context("resolve the current user's config directory")?;
            Ok(base
                .config_dir()
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT))
        }
    }
}

fn lexical_absolute(path: &Path, current_dir: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn service_executable_for_cli(current_executable: &Path) -> Result<PathBuf> {
    service_executable_for_cli_on(Platform::current(), current_executable)
}

fn service_executable_for_cli_on(platform: Platform, current_executable: &Path) -> Result<PathBuf> {
    let (cli_name, service_name) = match platform {
        Platform::Windows => ("cmr.exe", "cmr-service.exe"),
        Platform::MacOs | Platform::Linux => ("cmr", "cmr-service"),
    };
    let is_cli = current_executable.file_name().is_some_and(|name| {
        if platform == Platform::Windows {
            name.to_string_lossy().eq_ignore_ascii_case(cli_name)
        } else {
            name == OsStr::new(cli_name)
        }
    });
    if is_cli {
        let parent = current_executable
            .parent()
            .context("the cmr executable has no parent directory")?;
        let service = parent.join(service_name);
        if service.is_file() {
            return Ok(service);
        }
    }
    Ok(current_executable.to_path_buf())
}

fn legacy_windows_executable(service_executable: &Path) -> Option<PathBuf> {
    if service_executable.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case("cmr-service.exe")
    }) {
        return service_executable
            .parent()
            .map(|parent| parent.join("cmr.exe"));
    }
    None
}

fn service_arguments(context: &ServiceContext) -> Result<Vec<String>> {
    Ok(vec![
        "--config".to_owned(),
        path_text(&context.config)?.to_owned(),
        "--state-db".to_owned(),
        path_text(&context.state_db)?.to_owned(),
        "serve".to_owned(),
    ])
}

fn render_windows_task(context: &ServiceContext, identity: &str) -> Result<String> {
    let executable = escape_xml(path_text(&context.executable)?)?;
    let identity = escape_xml(identity)?;
    let arguments = service_arguments(context)?
        .iter()
        .map(|argument| windows_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let arguments = escape_xml(&arguments)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>{identity}</Author>
    <Description>{OWNERSHIP_MARKER}: Runs ModelRelay for the current Windows user.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{identity}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{identity}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>255</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{executable}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

fn render_launch_agent(context: &ServiceContext) -> Result<String> {
    let mut program_arguments = Vec::with_capacity(6);
    program_arguments.push(path_text(&context.executable)?.to_owned());
    program_arguments.extend(service_arguments(context)?);
    let program_arguments = program_arguments
        .iter()
        .map(|argument| {
            escape_xml(argument).map(|escaped| format!("    <string>{escaped}</string>"))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CodexModelRouterManaged</key>
  <string>{OWNERSHIP_MARKER}</string>
  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>CODEX_MODEL_ROUTER_MANAGED</key>
    <string>{OWNERSHIP_MARKER}</string>
  </dict>
  <key>ProgramArguments</key>
  <array>
{program_arguments}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>ThrottleInterval</key>
  <integer>5</integer>
</dict>
</plist>
"#
    ))
}

fn render_systemd_unit(context: &ServiceContext) -> Result<String> {
    let mut command = Vec::with_capacity(6);
    command.push(systemd_quote(path_text(&context.executable)?));
    command.extend(
        service_arguments(context)?
            .iter()
            .map(|argument| systemd_quote(argument)),
    );
    Ok(format!(
        "# {OWNERSHIP_MARKER}\n\
[Unit]\n\
 Description=ModelRelay\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
# The ':' prefix disables systemd environment-variable substitution.\n\
ExecStart=:{}\n\
Restart=on-failure\n\
RestartSec=5s\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        command.join(" ")
    ))
}

fn windows_action_matches(definition: &str, context: &ServiceContext) -> Result<bool> {
    windows_action_matches_executable(definition, context, &context.executable)
}

fn legacy_windows_action_matches(definition: &str, context: &ServiceContext) -> Result<bool> {
    let Some(executable) = context.legacy_windows_executable.as_deref() else {
        return Ok(false);
    };
    windows_action_matches_executable(definition, context, executable)
}

fn windows_action_matches_executable(
    definition: &str,
    context: &ServiceContext,
    executable: &Path,
) -> Result<bool> {
    let executable = escape_xml(path_text(executable)?)?;
    let raw_arguments = service_arguments(context)?
        .iter()
        .map(|argument| windows_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let escaped_arguments = escape_xml(&raw_arguments)?;
    let scheduler_arguments = escaped_arguments
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    Ok(
        definition.contains(&format!("<Command>{executable}</Command>"))
            && (definition.contains(&format!("<Arguments>{escaped_arguments}</Arguments>"))
                || definition.contains(&format!("<Arguments>{scheduler_arguments}</Arguments>"))),
    )
}

fn windows_definition_matches_current_user(
    definition: &str,
    current_user_sid: &str,
    current_user_identity: &str,
) -> Result<bool> {
    let lowered = definition.to_ascii_lowercase();
    let sid = escape_xml(current_user_sid)?.to_ascii_lowercase();
    let identity = escape_xml(current_user_identity)?.to_ascii_lowercase();
    let author = format!("<author>{identity}</author>");
    let sid_user = format!("<userid>{sid}</userid>");
    let identity_user = format!("<userid>{identity}</userid>");
    let current_user_ids =
        lowered.matches(&sid_user).count() + lowered.matches(&identity_user).count();

    Ok(lowered.contains(&author)
        && lowered.matches("<userid>").count() == 2
        && current_user_ids == 2
        && lowered.contains("<logontrigger>")
        && lowered.contains("<logontype>interactivetoken</logontype>")
        && lowered.contains("<runlevel>leastprivilege</runlevel>"))
}

fn launch_agent_action_matches(definition: &str, context: &ServiceContext) -> Result<bool> {
    let mut arguments = Vec::with_capacity(6);
    arguments.push(path_text(&context.executable)?.to_owned());
    arguments.extend(service_arguments(context)?);
    for argument in arguments {
        let escaped = escape_xml(&argument)?;
        if !definition.contains(&format!("<string>{escaped}</string>")) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn launch_agent_registration_action_matches(
    definition: &str,
    context: &ServiceContext,
) -> Result<bool> {
    if definition.contains("<plist") {
        return launch_agent_action_matches(definition, context);
    }
    let mut arguments = Vec::with_capacity(6);
    arguments.push(path_text(&context.executable)?.to_owned());
    arguments.extend(service_arguments(context)?);
    Ok(arguments
        .iter()
        .all(|argument| definition.contains(argument)))
}

fn systemd_action_matches(definition: &str, context: &ServiceContext) -> Result<bool> {
    let mut command = vec![systemd_quote(path_text(&context.executable)?)];
    command.extend(
        service_arguments(context)?
            .iter()
            .map(|argument| systemd_quote(argument)),
    );
    Ok(definition.contains(&format!("ExecStart=:{}", command.join(" "))))
}

fn output_is_not_found(output: &CommandOutput) -> bool {
    let detail = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    [
        "not found",
        "not-found",
        "cannot find",
        "could not find",
        "no such file",
        "no such process",
        "not registered",
        "0x80070002",
        "找不到",
        "不存在",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
}

fn bail_command_failure<T>(program: &str, output: &CommandOutput) -> Result<T> {
    let detail = output.stderr.trim();
    let detail = if detail.is_empty() {
        output.stdout.trim()
    } else {
        detail
    };
    if detail.is_empty() {
        bail!("{program} failed while managing the per-user router service");
    }
    bail!("{program} failed while managing the per-user router service: {detail}")
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("service path is not valid Unicode: {}", path.display()))
}

fn escape_xml(value: &str) -> Result<String> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\u{9}' | '\u{A}' | '\u{D}' => escaped.push(character),
            character
                if ('\u{20}'..='\u{D7FF}').contains(&character)
                    || ('\u{E000}'..='\u{FFFD}').contains(&character)
                    || ('\u{10000}'..='\u{10FFFF}').contains(&character) =>
            {
                escaped.push(character);
            }
            _ => bail!("service value contains a character that XML 1.0 cannot represent"),
        }
    }
    Ok(escaped)
}

fn windows_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn systemd_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_ascii_control() => {
                write!(quoted, "\\x{:02x}", u32::from(character))
                    .expect("writing to a String cannot fail");
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tempfile::tempdir;

    use super::*;

    #[derive(Debug)]
    struct Invocation {
        program: String,
        arguments: Vec<String>,
    }

    #[derive(Default)]
    struct MockRunner {
        calls: Vec<Invocation>,
        outputs: VecDeque<CommandOutput>,
    }

    impl MockRunner {
        fn with_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                calls: Vec::new(),
                outputs: outputs.into_iter().collect(),
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&mut self, program: &OsStr, arguments: &[OsString]) -> Result<CommandOutput> {
            self.calls.push(Invocation {
                program: program.to_string_lossy().into_owned(),
                arguments: arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            });
            Ok(self.outputs.pop_front().unwrap_or_else(|| successful("")))
        }
    }

    fn successful(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn unsuccessful() -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "not registered".to_owned(),
        }
    }

    fn unsuccessful_with(detail: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: detail.to_owned(),
        }
    }

    fn context(platform: Platform, root: &Path, definition: &str) -> ServiceContext {
        ServiceContext {
            platform,
            executable: root.join("cmr & router.exe"),
            legacy_windows_executable: None,
            config: root.join("config <local>.toml"),
            state_db: root.join("state \"local\".sqlite3"),
            definition: root.join(definition),
        }
    }

    fn legacy_windows_definition(
        powershell: &Path,
        launcher: &Path,
        sid: &str,
        identity: &str,
    ) -> String {
        format!(
            r"<Task>
<RegistrationInfo><Description>{LEGACY_WINDOWS_DESCRIPTION}</Description><URI>\{LEGACY_WINDOWS_TASK}</URI></RegistrationInfo>
<Triggers><LogonTrigger><UserId>{identity}</UserId></LogonTrigger></Triggers>
<Principals><Principal><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType></Principal></Principals>
<Actions><Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>",
            escape_xml(path_text(powershell).expect("PowerShell path")).expect("escape PowerShell"),
            escape_xml(&format!(
                "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{}\"",
                path_text(launcher).expect("launcher path")
            ))
            .expect("escape arguments"),
            escape_xml(path_text(launcher.parent().expect("launcher parent")).expect("parent path"))
                .expect("escape working directory")
        )
    }

    #[test]
    fn legacy_windows_presence_is_read_only_and_short_circuits_when_absent() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let powershell = temporary.path().join("powershell.exe");
        let launcher = temporary
            .path()
            .join("CodexRouter")
            .join("Start-Router.ps1");
        let runner = MockRunner::with_outputs([unsuccessful()]);
        let mut manager = ServiceManager::new(context, runner);

        assert!(
            !manager
                .exact_legacy_windows_task_present(&powershell, &launcher)
                .expect("missing task is safe to ignore")
        );
        assert_eq!(manager.runner.calls.len(), 1);
        assert_eq!(manager.runner.calls[0].program, "schtasks");
        assert_eq!(manager.runner.calls[0].arguments[0], "/Query");
        assert!(
            manager.runner.calls[0]
                .arguments
                .iter()
                .all(|argument| argument != "/Delete" && argument != "/End")
        );
    }

    #[test]
    fn legacy_windows_verifier_accepts_only_exact_current_user_action() {
        let powershell = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        let launcher = Path::new(r"C:\Users\test\CodexRouter\Start-Router.ps1");
        let sid = "S-1-5-21-100-200-300-1001";
        let identity = r"DOMAIN\test";
        let exact = legacy_windows_definition(powershell, launcher, sid, identity);
        verify_exact_legacy_windows_definition(&exact, powershell, launcher, sid, identity)
            .expect("exact known legacy task");

        for foreign in [
            exact.replace(sid, "S-1-5-21-9-9-9-1001"),
            exact.replace(identity, r"DOMAIN\foreign"),
            exact.replace("powershell.exe", "foreign.exe"),
            exact.replace(
                "</LogonType>",
                "</LogonType><RunLevel>HighestAvailable</RunLevel>",
            ),
            exact.replace(
                "</Actions>",
                "<Exec><Command>extra.exe</Command></Exec></Actions>",
            ),
            exact.replace(LEGACY_WINDOWS_DESCRIPTION, "lookalike task"),
        ] {
            verify_exact_legacy_windows_definition(&foreign, powershell, launcher, sid, identity)
                .expect_err("foreign or ambiguous task must be preserved");
        }
    }

    #[test]
    fn legacy_windows_removal_backs_up_and_detects_toctou() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let powershell = temporary.path().join("powershell.exe");
        let launcher = temporary
            .path()
            .join("CodexRouter")
            .join("Start-Router.ps1");
        let sid = "S-1-5-21-100-200-300-1001";
        let identity = r"DOMAIN\user";
        let definition = legacy_windows_definition(&powershell, &launcher, sid, identity);
        let changed = definition.replace(LEGACY_WINDOWS_DESCRIPTION, "changed concurrently");
        let runner = MockRunner::with_outputs([
            successful(&definition),
            successful(&format!("\"DOMAIN\\user\",\"{sid}\"\r\n")),
            successful("DOMAIN\\user\r\n"),
            successful("Ready"),
            successful(&changed),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        let error = manager
            .remove_exact_legacy_windows_task(&powershell, &launcher)
            .expect_err("concurrent replacement must abort deletion");

        assert!(
            error.to_string().contains("unrecognized") || error.to_string().contains("concurrent")
        );
        assert!(
            manager
                .runner
                .calls
                .iter()
                .all(|call| { !call.arguments.iter().any(|argument| argument == "/Delete") })
        );
        assert!(
            temporary
                .path()
                .join("legacy-codex-glm-router-")
                .parent()
                .is_some()
        );
        assert!(
            temporary
                .path()
                .read_dir()
                .expect("read recovery directory")
                .any(|entry| entry
                    .expect("recovery entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("legacy-codex-glm-router-"))
        );
    }

    #[test]
    fn whoami_sid_parser_is_strict() {
        assert_eq!(
            parse_whoami_sid("\"DOMAIN\\user\",\"S-1-5-21-1-2-3-1001\"\r\n").expect("valid SID"),
            "S-1-5-21-1-2-3-1001"
        );
        assert!(parse_whoami_sid("DOMAIN\\user,S-1-5-21-x").is_err());
        assert!(parse_whoami_sid("a,S-1-5-1\nb,S-1-5-2").is_err());
    }

    #[test]
    fn windows_task_state_parser_uses_only_the_csv_status_column() {
        assert_eq!(
            parse_windows_task_state("\"\\ModelRelay\",\"N/A\",\"Ready\"\r\n").expect("ready CSV"),
            "ready"
        );
        assert_eq!(
            parse_windows_task_state("\"\\ModelRelay\",\"N/A\",\"正在运行\"\r\n")
                .expect("localized running CSV"),
            "正在运行"
        );
        assert!(parse_windows_task_state("Ready\nRunning").is_err());
    }

    #[test]
    fn windows_install_uses_current_user_task_and_injected_runner() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let registered =
            render_windows_task(&context, "DOMAIN\\current-user").expect("render task");
        let runner = MockRunner::with_outputs([
            successful("DOMAIN\\current-user\r\n"),
            unsuccessful(),
            successful(""),
            successful(&registered),
            successful(""),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        manager.install().expect("install task");

        let bytes = fs::read(manager.definition_path()).expect("read task bytes");
        assert_eq!(&bytes[..2], &[0xff, 0xfe]);
        let definition = decode_service_definition(Platform::Windows, &bytes).expect("read task");
        assert!(definition.contains("encoding=\"UTF-16\""));
        assert!(definition.contains("<LogonTrigger>"));
        assert!(definition.contains("<UserId>DOMAIN\\current-user</UserId>"));
        assert!(definition.contains("<RestartOnFailure>"));
        assert!(definition.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(definition.contains("cmr &amp; router.exe"));
        assert!(definition.contains("config &lt;local&gt;.toml"));
        assert!(definition.contains("state \\&quot;local\\&quot;.sqlite3"));
        assert_eq!(manager.runner.calls[0].program, "whoami");
        assert_eq!(manager.runner.calls[1].program, "schtasks");
        assert_eq!(manager.runner.calls[1].arguments[0], "/Query");
        assert_eq!(manager.runner.calls[2].arguments[0], "/Create");
        assert_eq!(manager.runner.calls[3].arguments[0], "/Query");
        assert_eq!(manager.runner.calls[4].arguments[0], "/Run");
        assert!(
            manager
                .runner
                .calls
                .iter()
                .all(|call| call.program != "cmd" && call.program != "powershell")
        );
    }

    #[cfg(windows)]
    #[test]
    fn cli_uses_the_sibling_background_binary_for_windows_services() {
        let temporary = tempdir().expect("temporary directory");
        let cli = temporary.path().join("cmr.exe");
        let service = temporary.path().join("cmr-service.exe");
        fs::write(&cli, b"console binary").expect("write CLI fixture");
        assert_eq!(
            service_executable_for_cli(&cli).expect("fallback without service binary"),
            cli
        );
        fs::write(&service, b"background binary").expect("write service fixture");

        assert_eq!(
            service_executable_for_cli(&cli).expect("resolve service executable"),
            service
        );
        assert_eq!(legacy_windows_executable(&service), Some(cli.clone()));
        assert_eq!(legacy_windows_executable(&cli), None);
    }

    #[test]
    fn cli_uses_the_sibling_background_binary_for_macos_services() {
        let temporary = tempdir().expect("temporary directory");
        let cli = temporary.path().join("cmr");
        let service = temporary.path().join("cmr-service");

        assert_eq!(
            service_executable_for_cli_on(Platform::MacOs, &cli)
                .expect("fallback without service binary"),
            cli
        );
        fs::write(&service, b"background binary").expect("write service fixture");
        assert_eq!(
            service_executable_for_cli_on(Platform::MacOs, &cli)
                .expect("resolve service executable"),
            service
        );
    }

    #[test]
    fn cli_uses_the_sibling_background_binary_for_linux_services() {
        let temporary = tempdir().expect("temporary directory");
        let cli = temporary.path().join("cmr");
        let service = temporary.path().join("cmr-service");

        assert_eq!(
            service_executable_for_cli_on(Platform::Linux, &cli)
                .expect("fallback without service binary"),
            cli
        );
        fs::write(&service, b"background binary").expect("write service fixture");
        assert_eq!(
            service_executable_for_cli_on(Platform::Linux, &cli)
                .expect("resolve service executable"),
            service
        );
    }

    #[test]
    fn windows_install_upgrades_only_the_exact_current_user_sibling_cli_action() {
        let temporary = tempdir().expect("temporary directory");
        let mut context = context(Platform::Windows, temporary.path(), "task.xml");
        context.executable = temporary.path().join("cmr-service.exe");
        context.legacy_windows_executable = legacy_windows_executable(&context.executable);
        let mut legacy_context = context.clone();
        legacy_context.executable = context
            .legacy_windows_executable
            .clone()
            .expect("legacy sibling");
        legacy_context.legacy_windows_executable = None;
        let legacy =
            render_windows_task(&legacy_context, "DOMAIN\\current-user").expect("old task");
        let registered = render_windows_task(&context, "DOMAIN\\current-user").expect("new task");
        let runner = MockRunner::with_outputs([
            successful("DOMAIN\\current-user\r\n"),
            successful(&legacy),
            successful("DOMAIN\\current-user\r\n"),
            successful("\"DOMAIN\\current-user\",\"S-1-5-21-1-2-3-1001\"\r\n"),
            successful("Ready"),
            successful(&legacy),
            successful("DOMAIN\\current-user\r\n"),
            successful("\"DOMAIN\\current-user\",\"S-1-5-21-1-2-3-1001\"\r\n"),
            successful(""),
            successful(&registered),
            successful(""),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        manager.install().expect("upgrade the exact 0.1.2 action");

        let definition = decode_service_definition(
            Platform::Windows,
            &fs::read(manager.definition_path()).expect("read new local task"),
        )
        .expect("decode new task");
        assert!(definition.contains("<Command>"));
        assert!(definition.contains("cmr-service.exe</Command>"));
        assert!(!definition.contains("<Command>cmr.exe</Command>"));
        assert!(
            manager
                .runner
                .calls
                .iter()
                .any(|call| call.arguments.first().is_some_and(|arg| arg == "/Create"))
        );
    }

    #[test]
    fn windows_sibling_upgrade_rejects_a_different_user_or_directory() {
        let temporary = tempdir().expect("temporary directory");
        let mut context = context(Platform::Windows, temporary.path(), "task.xml");
        context.executable = temporary.path().join("cmr-service.exe");
        context.legacy_windows_executable = legacy_windows_executable(&context.executable);
        let mut foreign_context = context.clone();
        foreign_context.executable = temporary.path().join("other").join("cmr.exe");
        foreign_context.legacy_windows_executable = None;
        let foreign_path =
            render_windows_task(&foreign_context, "DOMAIN\\current-user").expect("foreign path");
        let runner = MockRunner::with_outputs([
            successful("DOMAIN\\current-user\r\n"),
            successful(&foreign_path),
        ]);
        let mut manager = ServiceManager::new(context.clone(), runner);
        manager
            .install()
            .expect_err("a different-directory action must remain untouched");
        assert!(
            manager
                .runner
                .calls
                .iter()
                .all(|call| !call.arguments.iter().any(|argument| argument == "/Create"))
        );

        let mut old_context = context.clone();
        old_context.executable = context
            .legacy_windows_executable
            .clone()
            .expect("legacy sibling");
        old_context.legacy_windows_executable = None;
        let foreign_user =
            render_windows_task(&old_context, "DOMAIN\\foreign").expect("foreign user");
        let runner = MockRunner::with_outputs([
            successful(&foreign_user),
            successful("DOMAIN\\current-user\r\n"),
            successful("\"DOMAIN\\current-user\",\"S-1-5-21-1-2-3-1001\"\r\n"),
        ]);
        let mut manager = ServiceManager::new(context, runner);
        manager
            .status()
            .expect_err("a different-user sibling action must remain untouched");
    }

    #[test]
    fn windows_reinstall_stops_and_reverifies_owned_task_before_replacing_it() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let registered =
            render_windows_task(&context, "DOMAIN\\current-user").expect("render task");
        fs::write(
            &context.definition,
            encode_service_definition(Platform::Windows, &registered),
        )
        .expect("write owned local definition");
        let runner = MockRunner::with_outputs([
            successful("DOMAIN\\current-user\r\n"),
            successful(&registered),
            successful("Running"),
            successful(""),
            successful("Ready"),
            successful(&registered),
            successful(""),
            successful(&registered),
            successful(""),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        manager.install().expect("reinstall owned task");

        assert_eq!(
            manager
                .runner
                .calls
                .iter()
                .map(|call| {
                    (
                        call.program.as_str(),
                        call.arguments.first().map_or("", String::as_str),
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("whoami", ""),
                ("schtasks", "/Query"),
                ("schtasks", "/Query"),
                ("schtasks", "/End"),
                ("schtasks", "/Query"),
                ("schtasks", "/Query"),
                ("schtasks", "/Create"),
                ("schtasks", "/Query"),
                ("schtasks", "/Run"),
            ]
        );
    }

    #[test]
    fn launch_agent_has_run_at_load_keep_alive_and_safe_xml() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::MacOs, temporary.path(), "agent.plist");
        let rendered = render_launch_agent(&context).expect("render plist");
        assert!(rendered.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(rendered.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(rendered.contains("cmr &amp; router.exe"));
        assert!(rendered.contains("config &lt;local&gt;.toml"));
        assert!(rendered.contains("state &quot;local&quot;.sqlite3"));

        let runner = MockRunner::with_outputs([
            successful("501\n"),
            unsuccessful(),
            successful(""),
            successful(&rendered),
        ]);
        let mut manager = ServiceManager::new(context, runner);
        manager.install().expect("install launch agent");
        assert_eq!(manager.runner.calls[0].program, "id");
        assert_eq!(manager.runner.calls[1].arguments[0], "print");
        assert_eq!(manager.runner.calls[2].arguments[0], "bootstrap");
    }

    #[test]
    fn systemd_lifecycle_is_isolated_and_restarts_on_failure() {
        let temporary = tempdir().expect("temporary directory");
        let mut context = context(Platform::Linux, temporary.path(), SYSTEMD_UNIT);
        context.executable = PathBuf::from("/opt/cmr/$HOME/%n/\"router\"\nnext");
        let registered = render_systemd_unit(&context).expect("render unit");
        let runner = MockRunner::with_outputs([
            unsuccessful(),
            successful(""),
            successful(""),
            successful("enabled"),
            successful(&registered),
            successful("enabled"),
            successful(&registered),
            successful("enabled"),
            successful(&registered),
            successful(""),
            successful(""),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        manager.install().expect("install systemd user unit");
        let definition = fs::read_to_string(manager.definition_path()).expect("read unit");
        assert!(definition.contains("Restart=on-failure"));
        assert!(definition.contains("ExecStart=:"));
        assert!(definition.contains("%%n"));
        assert!(definition.contains("\\\"router\\\"\\nnext"));
        assert_eq!(manager.status().expect("status"), ServiceStatus::Installed);
        assert!(manager.uninstall().expect("uninstall"));
        assert!(!manager.definition_path().exists());
        assert!(
            manager
                .runner
                .calls
                .iter()
                .all(|call| call.program == "systemctl")
        );
        assert!(manager.runner.calls.iter().any(|call| {
            call.arguments == ["--user", "enable", "--now", "codex-model-router.service"]
        }));
        assert!(manager.runner.calls.iter().any(|call| {
            call.arguments == ["--user", "disable", "--now", "codex-model-router.service"]
        }));
    }

    #[test]
    fn windows_command_line_quote_handles_quotes_and_trailing_slashes() {
        assert_eq!(windows_quote("plain"), "\"plain\"");
        assert_eq!(windows_quote("a\\\"b\\"), "\"a\\\\\\\"b\\\\\"");
    }

    #[test]
    fn windows_action_accepts_task_scheduler_normalized_argument_quotes() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let rendered =
            render_windows_task(&context, "DOMAIN\\current-user").expect("render task XML");
        let normalized = rendered.replace("&quot;", "\"");

        assert!(
            windows_action_matches(&normalized, &context)
                .expect("compare Task Scheduler-normalized XML")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_output_uses_the_active_oem_code_page() {
        use local_encoding_ng::{Encoder as _, Encoding};

        let localized_not_found = "错误: 系统找不到指定的文件。";
        // GitHub-hosted runners use a US OEM code page that cannot represent
        // the CJK fixture; keep the round-trip invariant meaningful there with
        // an ASCII fixture that every OEM code page can encode.
        let (fixture, encoded) = if let Ok(encoded) = Encoding::OEM.to_bytes(localized_not_found) {
            (localized_not_found, encoded)
        } else {
            let ascii = "ERROR: The system cannot find the file specified.";
            let encoded = Encoding::OEM
                .to_bytes(ascii)
                .expect("ASCII encodes in every OEM code page");
            (ascii, encoded)
        };
        let decoded = decode_native_command_output(&encoded);

        assert_eq!(decoded, fixture);
        assert!(output_is_not_found(&CommandOutput::failure(decoded)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_output_preserves_valid_utf8_structured_output() {
        let structured = "<Description>当前用户</Description>";

        assert_eq!(
            decode_native_command_output(structured.as_bytes()),
            structured
        );
    }

    #[test]
    fn windows_definition_encoding_round_trips_non_ascii_text() {
        let source = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Task>当前用户</Task>";
        let encoded = encode_service_definition(Platform::Windows, source);

        assert_eq!(&encoded[..2], &[0xff, 0xfe]);
        let decoded =
            decode_service_definition(Platform::Windows, &encoded).expect("decode UTF-16");
        assert_eq!(
            decoded,
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task>当前用户</Task>"
        );
    }

    #[test]
    fn windows_decoder_accepts_legacy_utf8_owned_definitions() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let registered =
            render_windows_task(&context, "DOMAIN\\current-user").expect("render task");
        fs::write(&context.definition, registered.as_bytes()).expect("write legacy UTF-8 task");
        let mut manager = ServiceManager::new(context, MockRunner::default());

        manager
            .verify_local_definition_if_present()
            .expect("legacy owned definition remains verifiable");
    }

    #[test]
    fn windows_decoder_rejects_malformed_utf16() {
        let error = decode_service_definition(Platform::Windows, &[0xff, 0xfe, b'<'])
            .expect_err("odd UTF-16 payload must fail");

        assert!(error.to_string().contains("odd byte length"));
    }

    #[test]
    fn windows_install_refuses_an_unknown_same_name_task() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let runner = MockRunner::with_outputs([
            successful("DOMAIN\\current-user\r\n"),
            successful(
                "<Task><Actions><Exec><Command>foreign.exe</Command></Exec></Actions></Task>",
            ),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        let error = manager
            .install()
            .expect_err("unknown task must be preserved");

        assert!(
            error
                .to_string()
                .contains("unknown owner or different action")
        );
        assert!(!manager.definition_path().exists());
        assert!(
            manager
                .runner
                .calls
                .iter()
                .all(|call| !call.arguments.iter().any(|argument| argument == "/Create"))
        );
    }

    #[test]
    fn windows_uninstall_ends_reverifies_and_then_deletes_owned_task() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let registered =
            render_windows_task(&context, "DOMAIN\\current-user").expect("render task");
        fs::write(
            &context.definition,
            encode_service_definition(Platform::Windows, &registered),
        )
        .expect("write owned local definition");
        let runner = MockRunner::with_outputs([
            successful(&registered),
            successful("Running"),
            successful(""),
            successful("Ready"),
            successful(&registered),
            successful(""),
            unsuccessful(),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        assert!(manager.uninstall().expect("uninstall owned task"));
        assert!(!manager.definition_path().exists());
        assert_eq!(
            manager
                .runner
                .calls
                .iter()
                .map(|call| (call.program.as_str(), call.arguments[0].as_str()))
                .collect::<Vec<_>>(),
            [
                ("schtasks", "/Query"),
                ("schtasks", "/Query"),
                ("schtasks", "/End"),
                ("schtasks", "/Query"),
                ("schtasks", "/Query"),
                ("schtasks", "/Delete"),
                ("schtasks", "/Query"),
            ]
        );
    }

    #[test]
    fn status_propagates_manager_errors_that_are_not_explicitly_not_found() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Windows, temporary.path(), "task.xml");
        let runner = MockRunner::with_outputs([unsuccessful_with("access denied")]);
        let mut manager = ServiceManager::new(context, runner);

        let error = manager
            .status()
            .expect_err("access failure must be surfaced");

        assert!(error.to_string().contains("access denied"));
    }

    #[test]
    fn macos_status_refuses_a_registered_agent_with_an_unknown_action() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::MacOs, temporary.path(), "agent.plist");
        fs::write(
            &context.definition,
            render_launch_agent(&context).expect("render owned local agent"),
        )
        .expect("write owned local agent");
        let runner = MockRunner::with_outputs([
            successful("501\n"),
            successful(&format!(
                "{OWNERSHIP_MARKER}\nprogram = /usr/bin/foreign-router"
            )),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        let error = manager
            .status()
            .expect_err("unknown registered action must be refused");

        assert!(
            error
                .to_string()
                .contains("unknown owner or different action")
        );
    }

    #[test]
    fn linux_status_refuses_a_registered_unit_with_an_unknown_action() {
        let temporary = tempdir().expect("temporary directory");
        let context = context(Platform::Linux, temporary.path(), SYSTEMD_UNIT);
        fs::write(
            &context.definition,
            render_systemd_unit(&context).expect("render owned local unit"),
        )
        .expect("write owned local unit");
        let runner = MockRunner::with_outputs([
            successful("enabled\n"),
            successful(&format!(
                "# {OWNERSHIP_MARKER}\n[Service]\nExecStart=/usr/bin/foreign-router\n"
            )),
        ]);
        let mut manager = ServiceManager::new(context, runner);

        let error = manager
            .status()
            .expect_err("unknown registered action must be refused");

        assert!(
            error
                .to_string()
                .contains("unknown owner or different action")
        );
    }

    #[test]
    fn relative_service_paths_are_frozen_as_lexical_absolutes_at_install_time() {
        let temporary = tempdir().expect("temporary directory");
        let install_cwd = temporary.path().join("workspace").join("project");
        let later_cwd = temporary.path().join("elsewhere");
        let config = lexical_absolute(Path::new("../shared/config.toml"), &install_cwd);
        let state_db = lexical_absolute(Path::new("./state/router.sqlite3"), &install_cwd);
        let mut context = context(Platform::Linux, temporary.path(), SYSTEMD_UNIT);
        context.config = config.clone();
        context.state_db = state_db.clone();

        let rendered_before = render_systemd_unit(&context).expect("render before cwd change");
        let later_config = lexical_absolute(Path::new("../shared/config.toml"), &later_cwd);
        let rendered_after = render_systemd_unit(&context).expect("render after cwd change");

        assert!(config.is_absolute());
        assert!(state_db.is_absolute());
        assert!(!config.components().any(|part| part == Component::ParentDir));
        assert!(!state_db.components().any(|part| part == Component::CurDir));
        assert_ne!(config, later_config);
        assert_eq!(rendered_before, rendered_after);
        assert!(rendered_before.contains(&systemd_quote(
            path_text(&config).expect("Unicode config path")
        )));
        assert!(rendered_before.contains(&systemd_quote(
            path_text(&state_db).expect("Unicode state path")
        )));
    }
}
