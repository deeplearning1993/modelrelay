# Compatibility and Remote acceptance

Codex Model Router integrates with version-sensitive Codex and ChatGPT surfaces.
Support is earned by an end-to-end test against exact released client versions;
it is never inferred from a successful local health check.

The router is designed to integrate with compatible Desktop and ChatGPT Remote
builds. Only matrix cells marked `Passed` are validated support claims; every
current `Not tested` cell remains an unverified design target.

## Status vocabulary

| Status | Meaning |
| --- | --- |
| Passed | The complete scenario passed on the recorded host and client build. |
| Experimental | The route works, but an upstream contract is undocumented or unstable. |
| Blocked | A known upstream or local issue prevents the scenario. |
| Not tested | No release evidence exists for this combination. |

## Release matrix

Every release replaces `Not tested` with a result and records the exact OS,
Codex host, ChatGPT Desktop, iOS, and Android build numbers in its release notes.

| Host | Local CLI/app-server | Desktop picker and task | iOS ChatGPT Remote | Android ChatGPT Remote | User service |
| --- | --- | --- | --- | --- | --- |
| Windows 11 x64 | Not tested | Not tested | Not tested | Not tested | Task Scheduler: not tested |
| macOS 14+ Apple silicon | Not tested | Not tested | Not tested | Not tested | LaunchAgent: not tested |
| macOS 14+ Intel | Not tested | Not tested | Not tested | Not tested | LaunchAgent: not tested |
| Ubuntu 24.04 x64 | Not tested | CLI/app-server only | Not tested | Not tested | systemd user: not tested |
| Ubuntu 24.04 arm64 | Not tested | CLI/app-server only | Not tested | Not tested | systemd user: not tested |

Linux has no assumed ChatGPT Desktop surface. Its Remote result applies only when
the supported Codex CLI/app-server is running as the selected Remote host.

## Required test setup

- The desktop host and both phones use the same ChatGPT account and workspace.
- The host is online, Remote control is enabled, and the router listens only on
  loopback. No inbound firewall rule or LAN listener is required.
- The official catalog is fetched through the signed-in Codex host. Test logs
  record model identifiers and outcomes, never tokens, keys, `auth.json`, request
  authorization headers, conversation text, or tool payloads.
- Use one official model and two mock external adapters with distinct IDs. A live
  external provider is optional for pre-release CI and mandatory for a release
  candidate.

## Acceptance scenarios

1. Start with an untouched signed-in Codex configuration, install the router,
   and confirm official models still appear and complete a task.
2. Enable external models, publish a chosen order, restart the Codex host, and
   confirm the Desktop picker matches that order up to the client's capacity.
3. From iOS and Android Remote, select the same host and verify the identical
   published external IDs appear, can start a task, call a tool, and stream the
   final response.
4. In one task, run official -> external A -> external B -> official. Confirm each
   turn inherits portable messages plus tool calls/results, while provider-only
   encrypted reasoning is never replayed to another provider.
5. Trigger compaction before and after a provider switch. Each Codex response
   must contain exactly one standard `type=compaction` output item and the next
   turn must retain the neutral summary.
6. Exercise Responses over HTTP, SSE, and WebSocket, including fragmented tool
   arguments, parallel calls, cancellation, reconnect, and upstream errors.
7. Reorder and hide models from the desktop manager. Confirm the next atomic
   catalog publication appears identically on Desktop, iOS Remote, and Android
   Remote without deleting provider configuration.
8. Stop the host and verify phones report it unavailable rather than silently
   sending the selected external model to an unrelated upstream.
9. Upgrade, crash, restart, uninstall, and restore. Confirm the user-level service
   recovers and unrelated MCP, skills, sandbox, approval, plugin, and project
   settings remain byte-for-byte unchanged.

## Release gate

A platform is advertised as supported only when all applicable scenarios pass.
Remote support remains `Experimental` unless both mobile platforms pass with the
current stable ChatGPT releases. If an upstream client changes its catalog or
routing behavior, the affected matrix cell is immediately downgraded instead of
being hidden behind a local-success indicator.
