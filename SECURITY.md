# Security policy

Do not include API keys, ChatGPT tokens, `auth.json`, authorization headers,
credential-store exports, request bodies from private tasks, or runtime databases
in issues or pull requests.

The router binds only to loopback addresses. Its logs record request IDs,
provider IDs, model IDs, status and timing; headers and content are excluded.

## Private vulnerability reports

Use GitHub Private Vulnerability Reporting: open this repository's **Security**
tab, choose **Report a vulnerability**, and submit the private advisory form.
GitHub keeps the report and any private-fork remediation discussion visible only
to the reporter and the repository's security collaborators until disclosure is
agreed. Do not open a public issue or pull request for an undisclosed security
problem, and do not attach credentials, private task data, or runtime databases.

## Outbound network boundary

Official-model traffic is fixed to
`https://chatgpt.com/backend-api/codex`. Built-in presets connect to their named
providers' documented endpoints. This is intentionally a general-purpose router,
not a host-allowlist product: a user may explicitly configure a
`custom-compatible` provider or `base_url` override to any HTTPS endpoint, or to
an HTTP endpoint on a loopback address. Provider URLs reject user information,
query strings, and fragments, and the HTTP client does not follow redirects.
External endpoints receive credentials only from their configured credential
reference; inbound ChatGPT authorization material is not forwarded to them.

Loopback binding prevents remote network access; it is not by itself an
authentication boundary between local processes. Every `/models` request is first
forwarded to the official service and its successful response is structurally
validated. The first such request with an eligible account header binds the router,
on a trust-on-first-use (TOFU) basis, to that request's
`ChatGPT-Account-ID`. Only the SHA-256 digest of the header is persisted; the
plaintext account/workspace identifier is not stored. Enrollment requires exactly
one non-empty account header. Catalog requests with a missing, empty, repeated, or
non-matching account header still receive the successfully validated official
catalog, but external entries are not injected, credentials are not enrolled, and
the persisted binding is not changed. All external-model requests require the
already-bound account header and credentials accepted by the official ChatGPT
catalog endpoint (acceptance is cached only for the router process lifetime).

TOFU assumes the first successful caller is the intended ChatGPT account and
workspace. A malicious process running as the same OS user may still be able to
observe or impersonate local application traffic, so run the router only in a
trusted local user session and stop it when untrusted local software is running.
Changing ChatGPT accounts or workspaces changes the account header: official models
remain available, external models disappear from that account's catalog, and its
external-model requests are rejected. There is currently no independent
binding-reset command: stop the router and remove its SQLite state database to reset
the binding. That reset also permanently removes conversation continuity,
model-switch history, and compaction mappings; backups of the database retain both
those records and the account digest.

## Local state and retention

Provider secrets are stored in the operating-system credential vault and are not
written to the router configuration or state database. The SQLite state database
is different: it intentionally stores normalized conversation inputs and outputs,
tool calls and tool results, provider/model-switch history, and portable compaction
summaries in plaintext so later requests can inherit context across providers.
Official opaque compaction items and their summary mappings are stored alongside
that plaintext state. The database also contains the SHA-256 digest used for the
ChatGPT account/workspace TOFU binding, but never the plaintext account header.

The SQLite database is not encrypted at rest and currently has no automatic expiry
or pruning policy. Its contents remain until the user removes the database; local
backups and filesystem snapshots may retain additional copies. Treat the database
and any backup as sensitive conversation data. Stop the router before removing the
database; removal permanently discards locally stored session continuity and
compaction mappings.
