# CLI reference

The `cmr` executable configures and runs Codex Model Router. Its default paths are
per-user platform directories. Use `cmr config path` to print the resolved router
configuration path.

The following global overrides make portable installs and isolated tests possible:

```text
--config <PATH>          Router TOML
--state-db <PATH>        SQLite session state
--codex-config <PATH>    User-level Codex config.toml
--codex-sidecar <PATH>   Codex integration ownership record
```

## Typical setup

```powershell
cmr config init
cmr provider add zhipu --preset zhipu
cmr model add glm-5.2 --provider zhipu
cmr secret set zhipu
cmr codex install
cmr service install
```

`secret set` accepts no key argument. It asks twice through an invisible terminal
prompt and stores the result under a generation-specific, per-router-installation
reference in Windows Credential Manager, macOS Keychain, or Linux Secret Service.
Only that non-secret reference is written to router TOML; API keys are never
written to router TOML or printed by the CLI. When a key is rotated, the previous
vault generation is retained because a running router uses its startup snapshot.

Use `cmr presets` for a compact preset table or `cmr presets --json` for full,
non-secret capabilities. The built-in providers include OpenAI, Anthropic, Gemini,
Zhipu, DeepSeek, Qwen, Kimi, Doubao, MiniMax, xAI, Mistral, OpenRouter, and Ollama.

## Catalog management

```powershell
cmr model list
cmr model move glm-5.2 0
cmr model hide gpt-example
cmr model unhide gpt-example
cmr model disable glm-5.2
cmr model enable glm-5.2
```

`move` and `hide` accept either configured external ids or official ids. This is
how official and third-party models share one user-controlled picker order. The
router still fetches official entries dynamically; the CLI does not persist a
copy of the official catalog.

## Safe Codex integration

`cmr codex install` edits the user-level `~/.codex/config.toml` only. It preserves
all unrelated settings, including MCP servers, skills, sandbox, approval, feature,
and project tables, while merging exactly these values:

```toml
model_provider = "openai"
openai_base_url = "http://127.0.0.1:15722/v1"

[features]
remote_control = true
```

Before every install, the CLI writes a collision-resistant byte-for-byte backup
beside `config.toml`. It also writes a JSON sidecar recording the backup path,
the exact values it changed, and their original TOML forms. Backups are
intentionally retained.

`cmr codex uninstall` compares every managed key before restoring it. A key is
restored from the backup only when it still equals the value installed by `cmr`.
If the user changed a managed key later, uninstall preserves that change. Other
keys and tables are never restored wholesale, so unrelated edits made after
installation remain intact.

`cmr codex restore` performs an unconditional reset to the pre-install Codex
config. It replays the byte-for-byte backup captured at install time, so every
managed value is reverted even if the user changed it afterwards, and the whole
file returns to its exact pre-install snapshot (unrelated keys added after
install are removed too). If the config did not exist before install, the
current config is deleted to reproduce that default state. The integration
sidecar is removed and the recovery backup is retained. Use `restore` for "put
Codex back to its original default configuration"; use `uninstall` when you want
to keep later manual edits.

Use `cmr codex status` to report `not-installed`, `installed`, or `drifted`.

## Per-user background service

```text
cmr service install
cmr service status
cmr service uninstall
```

Service management is user-scoped and never requests elevation. The installed
definition launches the current `cmr` executable with the resolved `--config`
and `--state-db` paths, so reinstall the service after moving the executable or
changing either path override.

- Windows installs the current-user scheduled task `Codex Model Router`, starts
  it immediately, triggers it at that user's logon, runs with least privilege,
  and retries failures after one minute. Its generated XML is retained under the
  router data directory until uninstall.
- macOS installs `~/Library/LaunchAgents/io.github.codex-model-router.plist` with
  `RunAtLoad` and `KeepAlive`, then bootstraps it into the current GUI domain.
- Linux installs
  `~/.config/systemd/user/codex-model-router.service`, uses
  `Restart=on-failure`, and enables it immediately with `systemctl --user`.

`status` asks the native service manager and prints `installed` or
`not-installed`. `uninstall` stops and unregisters the service and removes only
its generated definition; router configuration, credentials, and SQLite state
are preserved. Executable and data paths are passed as direct arguments rather
than through a command shell, and generated XML, plist, and systemd values are
escaped for their native formats.

## Diagnostics

`cmr doctor` checks:

- router TOML validity and the loopback-only listener invariant;
- required credential references against the OS vault without printing values;
- Codex config integration state;
- the loopback health endpoint, when the router is running;
- exact equality between configured enabled, visible external models and health
  `routable_external_models`; and
- an authorized catalog snapshot whose non-empty `external_models` is the
  actual picker-injected subset of those routable models (picker capacity can
  make it smaller).

An offline router is a warning so configuration can be diagnosed before starting
the service. Invalid configuration, missing required credentials, integration
metadata errors, or a running router with a mismatched model set cause a non-zero
exit status.

The default `compatibility_policy = "warn"` also prints an explicit warning until
the exact Desktop and paired phone Remote builds have passed the release matrix.
With `compatibility_policy = "strict"`, that missing release evidence is a doctor
error. A healthy loopback service is deliberately not treated as proof that a
version-sensitive picker or phone Remote surface works.

For the `0.1.0` pre-release, no validated Desktop/phone build evidence is embedded
in the binary. `strict` is therefore an intentional manual release gate and will
fail `doctor` even when every local check passes; a local setting cannot waive it.
A future release may make the gate pass only by shipping versioned evidence for
the exact client builds that completed the compatibility matrix.

## Complete command surface

```text
cmr serve
cmr doctor
cmr presets [--json]
cmr config init|path|show
cmr provider list
cmr provider add <id> --preset <preset> [--base-url <url>] [--secret-profile <name>]
cmr provider remove <id>
cmr model list
cmr model add <id> --provider <id> [model options]
cmr model enable|disable <id>
cmr model move <id> <zero-based-position>
cmr model hide|unhide <id>
cmr secret set|delete <provider> [--profile <name>]
cmr codex install|uninstall|restore|status
cmr service install|uninstall|status
```

Provider removal is refused while a model references it. Removing a provider does
not delete credentials. `cmr secret delete` safely unbinds the reference from the
configuration but currently retains its vault entry so an already-running router
does not fail. Stop or restart router processes before any future explicit
credential garbage collection.
