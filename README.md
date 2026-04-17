# tau

Rust workspace skeleton for the `tau` agent harness.

## Workspace layout

- `crates/tau-proto` — shared protocol types and CBOR codec helpers
- `crates/tau-config` — user and project configuration loading
- `crates/tau-core` — event bus, routing, state, and tool registry
- `crates/tau-supervisor` — supervised child-process and stdio transport glue
- `crates/tau-test-support` — reusable end-to-end test utilities
- `crates/tau-socket` — Unix socket transport glue
- `crates/tau-cli` — CLI entrypoint for embedded and daemon-attached use
- `crates/tau-agent` — first-party agent process
- `crates/tau-ext-fs` — filesystem-oriented extension
- `crates/tau-ext-shell` — shell-oriented extension

## Getting started

- `cargo check`
- `nix develop`
- `selfci check`

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)
