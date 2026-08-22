# Testing

The project has two complementary test layers:

- Rust unit tests cover canonical Responses types, provider adapters, SQLite replay,
  configuration merging, header filtering, and compaction invariants.
- `tests/e2e_router.py` treats the compiled `cmr` executable as a black box and
  drives its HTTP, SSE, and WebSocket transports.

The end-to-end suite is fully offline. It starts three temporary loopback HTTP
servers: one emulates the official Responses backend and two emulate independent
OpenAI Chat Completions providers. It creates a temporary router configuration and
SQLite database, generates request-only credential sentinels in memory, and removes
the temporary directory when the suite exits. It does not inspect the current
user's Codex files, operating-system credential vault, databases, or logs.

## Local commands

Build the CLI and run the complete offline suite:

```console
cargo build -p cmr-cli --locked
cargo build -p cmr-cli --features e2e-loopback-upstream
python tests/e2e_router.py
```

The loopback escape hatch is effective only in debug builds. Release builds,
including `--release --all-features`, always retain the production allowlist for
the official ChatGPT Codex upstream.

To exercise a binary in another target directory, provide its exact path:

```console
CMR_BIN=/path/to/cmr python tests/e2e_router.py
```

PowerShell example:

```powershell
$env:CMR_BIN = (Resolve-Path .\target\debug\cmr.exe)
python .\tests\e2e_router.py
```

The black-box suite verifies:

- catalog ordering, hiding, capacity, unique IDs, and collision rejection;
- one SQLite-backed session switching official -> external A -> external B ->
  official while retaining inputs, outputs, function calls, and tool results;
- non-streaming HTTP, full Responses SSE lifecycle, and WebSocket
  `response.create`;
- exactly one genuine `type = compaction` output item for non-streaming, SSE,
  and direct compaction requests, plus replay-only continuation after standalone
  compaction;
- strict external maximum-output validation before HTTP, WebSocket, and compact
  upstream calls;
- forwarding of official authentication and isolation of all ChatGPT/private
  authentication headers from external providers; and
- account binding, provider-instance provenance, malformed-stream rejection, and
  official/external header isolation.

## CI

`.github/workflows/ci.yml` runs on Windows, macOS, and Linux. Every platform checks
Rust formatting, tests the full workspace, treats Clippy warnings as errors, builds
the CLI, runs the offline black-box suite, installs desktop dependencies with
`npm ci`, builds the TypeScript/Vite frontend, checks the desktop Rust host, and
creates one unsigned Tauri bundle (`deb`, `.app`, or NSIS) for the current OS.
The bundle gate proves that distributable desktop artifacts can be assembled
without release signing credentials. The Windows job also performs a silent
NSIS installation and verifies that the runnable `cmr.exe` sidecar is installed
beside `cmr-desktop.exe`, which is the location used by desktop service lookup.
It does not exercise an installed ChatGPT Desktop build or a paired phone.

Phone Remote remains a manual release acceptance gate. A release candidate must
still run the scenarios in [`compatibility.md`](compatibility.md) on real iOS and
Android devices signed into the same ChatGPT account and workspace as the host.
