# Desktop manager

This directory contains the framework-free TypeScript UI and Tauri 2 host for
Codex Model Router.

## Normal user flow

The desktop app owns the complete local onboarding path. A user clicks **添加供应商**,
selects a provider, and enters its Base URL, API key, and model ID. Provider-specific
defaults are filled automatically, while less common fields remain available under
**高级设置**.

One click on **保存并一键接入 Codex** then saves the provider and model, stores the
API key in the current user's operating-system credential vault, registers and starts
the current-user login service, validates router health and the external catalog, and
safely backs up and merges the Codex user configuration.

After success, the only user action is to completely quit and reopen ChatGPT. If a
later setup stage fails, the dialog reports the exact stage and retained work.
**重试接入** reruns only the idempotent local setup command; it never resubmits,
reads, or asks for the API key again. The app binds no public interface and adds no
firewall rule.

Existing configurations do not need to be entered again: when at least one external
model is enabled, **一键接入 Codex** runs the same setup directly from the provider
section.

## Development

```text
npm install
npm run build
npm run tauri dev
npm run tauri build
```

Rust unit tests live beside the Tauri commands and always use a temporary
configuration file plus port `0`, so they neither read a developer's router
configuration nor probe the normal listener. Run them from the workspace root:

```text
cargo test -p cmr-desktop
```

## Real state, not a demo state

The Tauri commands load and atomically save the same non-secret `RouterConfig`
used by `cmr`. Model visibility updates `hidden_models`; drag ordering updates
`catalog_order` while preserving unknown and dynamically discovered official
model IDs. The manager never reads `~/.codex/config.toml`, ChatGPT auth files, or
stored credential values. Provider rows expose only whether a `secret_ref`
exists, not whether the referenced vault entry contains a valid key.

The default router config path comes from `cmr_storage::AppPaths`. Development
and portable builds may select a separate file with `CMR_DESKTOP_CONFIG`. This
override is also how integration tests avoid touching a developer's real router
configuration.

## Service ownership

Service status is based on a real loopback `GET /health` request. A response is
accepted only when it identifies `service: "codex-model-router"`; an unrelated
listener is reported as unavailable.

Release bundles include a target-matched `cmr` executable beside the desktop
binary, so installed builds do not depend on a checkout's `target` directory or
on `PATH`. `npm run tauri build` builds that sidecar from the locked Rust
workspace before Tauri packages it. `tauri dev` intentionally does not build a
sidecar: development retains the existing sibling/`PATH` lookup, and an explicit
local executable can be selected with `CMR_DESKTOP_ROUTER_BIN`.

The manager may stop only a child it started during the current desktop session.
A healthy service started by a scheduled task, launch agent, systemd user unit,
terminal, or earlier app session is labelled externally managed and is never
terminated by PID guessing. Desktop one-click setup and the CLI use the same
cross-platform service manager and produce the same current-user definition.

On Windows the child is created without a console window. Startup succeeds only
after `/health` passes; a child that exits or misses the three-second deadline is
cleaned up.

## Remote status boundary

The Remote check requires the saved enabled, visible model set to match the
running service's sorted `routable_external_models`. It then requires an
authorized official catalog snapshot and a non-empty `external_models` list;
that list is the subset actually injected into the picker and may be smaller
because of picker capacity. “Local ready” means only that these local
prerequisites agree. It is not proof that a particular ChatGPT mobile release
publishes or routes an external model. Release acceptance still requires a real
iOS/Android device signed into the same ChatGPT account and workspace; see
`../../docs/compatibility.md`.

CI builds an unsigned native bundle on each desktop OS as a packaging gate. That
gate does not replace the same-account, same-workspace real-device Remote
acceptance described above.

The Windows gate silently installs the generated NSIS package and verifies that
`cmr.exe` is a non-empty, runnable sibling of `cmr-desktop.exe`. Before NSIS
removes application files, its uninstall hook first asks that installed sidecar
to safely unregister its owned background service, then to restore only the
Codex configuration values still owned by the router. Either cleanup failure
aborts file removal. Router configuration, credentials, and SQLite state remain
user data and are not deleted by the installer.

## Credentials

This is a native Tauri application: HTML/CSS/TypeScript renders inside the
system WebView while the local Rust host owns configuration, service control,
and credential-vault access. It does not load a remotely hosted management page.

The provider form accepts an API key only for the duration of one local Tauri
command. It never copies that value into dashboard state, logs it, writes it to
TOML, or returns it to the UI. The Rust host writes the key directly to the
current user's operating-system credential vault, and the password field is
cleared in a `finally` path after both success and failure. Presets that do not
require authentication disable the key field completely.

The CLI remains available for automation and development, but it is not required for
the normal desktop onboarding flow.
