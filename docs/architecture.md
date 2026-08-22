# Architecture

Codex Model Router is a per-user loopback compatibility layer. Codex continues
to authenticate with ChatGPT; the router does not replace the login flow or
persist ChatGPT credentials. It merges configured external models into the
official catalog and adapts external protocols to the Responses contract used by
Codex.

## Data paths

### Catalog

1. Codex requests `/v1/models` from the loopback router.
2. The router forwards the authenticated request to the fixed ChatGPT Codex
   backend and structurally validates the returned official catalog.
3. After the local account-binding checks pass, enabled external entries are
   merged using `catalog_order`, `hidden_models`, and `picker_capacity`.
4. Official entries remain dynamic. The router never replaces them with a
   bundled snapshot.

Model identifiers share one namespace. A configured external identifier that
collides with an official identifier makes the authorized merged catalog fail
closed; it cannot silently shadow an official model.

### Requests

Official-model HTTP, SSE, and WebSocket requests retain the native Responses
shape and are sent to the fixed ChatGPT Codex upstream. Only the required
authentication and protocol headers are forwarded. Provider-specific or custom
credential headers are removed.

External-model requests are decoded into canonical Responses items and then
encoded by one of four provider families:

- OpenAI Responses;
- OpenAI-compatible Chat Completions;
- Anthropic Messages; or
- Gemini GenerateContent.

Only the selected provider's credential is read from the operating-system vault.
ChatGPT authentication is never sent to an external endpoint. Redirects are
disabled, endpoint overrides require HTTPS except for loopback HTTP, and URLs may
not contain user information, query strings, or fragments.

### Context switching

SQLite stores a response graph containing the current-turn canonical input,
canonical output, provider/model identity, and parent response. When a request
stays on a native official cursor, the router preserves `previous_response_id`
and avoids replaying plaintext history. When a request crosses a provider
boundary, it constructs portable history from the graph and removes the upstream
cursor.

Portable history includes messages, function calls, and function-call outputs.
Provider-owned reasoning is stamped with the configured provider-instance ID and
can only return to that exact instance. Foreign opaque reasoning and internal
metadata are removed before an upstream request. An incomplete opaque ancestry
cannot cross providers unless a mapped compaction boundary already summarizes
it.

### Compaction

Every Codex-facing compaction result contains exactly one genuine
`type = "compaction"` output item. External history is summarized in plaintext,
while an authenticated official ChatGPT compaction call supplies the encrypted
item. SQLite stores the mapping between the neutral summary and that official
opaque item.

Standalone `/responses/compact` results use local `cmr_compact_` replay-only IDs;
they are never passed to an upstream as native response cursors. A compaction
boundary resets earlier replay history. Compaction is rejected while a function
call is unresolved so a later tool result cannot become orphaned.

### Streaming and tools

The router emits complete Responses lifecycle events over HTTP SSE and the
Responses WebSocket facade. Stream parsing follows SSE event boundaries rather
than socket chunk boundaries. Function-call identity, argument fragments,
output-index uniqueness, terminal output, and event ordering are validated before
durable state is committed. Malformed or contradictory streams fail closed.

## Desktop and phone Remote

The Desktop picker asks the host's Codex app-server for its model list. Because
the host is configured to use the loopback router, the merged catalog can reach
both the Desktop picker and a phone Remote session connected to that same host.
The phone does not connect to the host's `127.0.0.1` directly and needs no
provider key.

This path is client-version-sensitive. A successful `/health`, `/models`, or
app-server RPC check proves the local path only; it does not prove that a released
Desktop renderer or paired iOS/Android app displays and executes an external
entry. Releases therefore report those outcomes separately in
[`compatibility.md`](compatibility.md) and never infer phone support from a local
test.

## Trust and persistence boundaries

- Router configuration contains endpoints, model metadata, ordering, and vault
  references, but no secret values.
- Provider secrets live in Windows Credential Manager, macOS Keychain, or Linux
  Secret Service.
- SQLite intentionally contains plaintext portable conversation state and must
  be treated as sensitive user data.
- ChatGPT account/workspace binding persists only a SHA-256 digest of the
  account header.
- The server listens on an IP loopback address only and creates no firewall or
  LAN exposure.

See [`SECURITY.md`](../SECURITY.md) for the complete threat and retention model.
