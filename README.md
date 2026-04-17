# Tau coding agent

> Tau is like [Pi][pi], but twice as much.

[Pi][pi] is truly a breath of fresh air in the AI harness space,
but it doesn't go far enough. Tau is twice as as Unix-like,
which is twice as everything.

Instead of being built on top of a Typescript runtime, Tau builds on top
the most venerable, powerful and ubiquitous runtime there is: Unix itself.

Tau runs all its components as standalone Posix processes, communicating
over stdio/rpc.

Components include:

* UI
* harness
* LLM API
* each extension

This architecture has tremendous benefits:

* each component can be ran and sandboxed individually using tools like bubblewrap, docker, VM, or a different machine
* components can be implemented in any programming language,
*

[pi]: https://shittycodingagent.ai/

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
