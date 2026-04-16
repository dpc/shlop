# shlop

Rust workspace skeleton for the `shlop` agent harness.

## Workspace layout

- `crates/shlop-proto` — shared protocol types and CBOR codec helpers
- `crates/shlop-config` — user and project configuration loading
- `crates/shlop-core` — event bus, routing, state, and tool registry
- `crates/shlop-supervisor` — supervised child-process and stdio transport glue
- `crates/shlop-socket` — Unix socket transport glue
- `crates/shlop-cli` — CLI entrypoint for embedded and daemon-attached use
- `crates/shlop-agent` — first-party agent process
- `crates/shlop-ext-fs` — filesystem-oriented extension
- `crates/shlop-ext-shell` — shell-oriented extension

## Getting started

- `cargo check`
- `nix develop`
- `selfci check`

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)
