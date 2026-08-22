# ModelRelay

An unofficial, local-first model router for Codex. It is designed to preserve the
signed-in ChatGPT/Codex model catalog, add user-selected external models to the
same picker on validated client builds, and keep portable conversation context
when a task switches providers.

> [!IMPORTANT]
> This project is not affiliated with or endorsed by OpenAI. Client integration
> points are version-sensitive and covered by an explicit compatibility matrix.

## Status

The repository is in active `0.2.0` development. The protocol core, provider
interfaces, and desktop UI are functional; see the compatibility matrix before
relying on a specific client build.

## Design invariants

- Loopback-only HTTP/SSE/WebSocket service.
- Official ChatGPT credentials are forwarded in memory and never persisted.
- Third-party secrets live in the operating-system credential store.
- Official models remain dynamically sourced; enabled external models are merged
  in a user-controlled order.
- Cross-provider switching replays portable messages, tool calls/results and a
  neutral compaction summary. Provider-encrypted reasoning is never fabricated.
- A Codex compaction response contains exactly one standard `compaction` item.
- Self-hosted plain-HTTP endpoints require an explicit per-provider opt-in.

## Built-in provider families

Responses API, OpenAI-compatible Chat Completions, Anthropic Messages, and
Gemini `GenerateContent`.

## Workspace layout

- `crates/cmr-core` — shared types, catalog, compaction, and context logic.
- `crates/cmr-providers` — offline protocol adapters.
- `crates/cmr-storage` — config/state persistence, credential vault references.
- `crates/cmr-router` — loopback HTTP/WS server and catalog merge.
- `crates/cmr-cli` — `cmr` and `cmr-service` binaries.
- `apps/desktop` — Tauri desktop manager.

## Building

```sh
cargo test --workspace
cd apps/desktop && npm install && npm run tauri build
```

## License

Apache-2.0
