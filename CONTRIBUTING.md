# Contributing

Contributions are licensed under Apache-2.0. New provider support must include
offline fixtures for text streaming, tool calls, errors and capability rejection.
Never commit live credentials or captured private conversations.

Before submitting a change, run formatting, Clippy, the full workspace tests and
the offline black-box checks documented in [`docs/testing.md`](docs/testing.md).
Do not weaken the loopback, official-upstream allowlist, account-binding, header
isolation, provider-provenance, or exact-one compaction invariants to make a test
fixture easier to run.
