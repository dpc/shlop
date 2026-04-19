# Tau Architecture

## Single binary design

Tau ships as a single unified binary (`tau`) that contains the CLI, agent,
and all built-in tool extensions. When the harness needs to spawn an
extension as a child process, it invokes itself:

```
tau component agent
tau component ext-fs
tau component ext-shell
```

The `component` subcommand is hidden from `--help` — it's an internal
dispatch mechanism used by the harness, not a user-facing command.

### Why a single binary

- **Smaller total size**: shared code (protocol, serialization, std) is
  deduplicated by the linker and LTO.
- **Simpler distribution**: one file to install, no PATH discovery needed
  for extension binaries.
- **Self-contained**: `std::env::current_exe()` resolves the binary path
  for child process spawning — no external lookup required.

### How it works

1. The `tau` crate (`crates/tau/`) is a thin binary that calls
   `tau_cli::main_with_args()`.
2. `main_with_args()` uses clap to parse arguments. The hidden `Component`
   subcommand dispatches to the appropriate extension's `run_stdio()`
   entry point (e.g. `tau_agent::run_stdio()`).
3. All other subcommands (`chat`, `embedded`, `serve`, `send`, etc.) go
   through the normal CLI path.
4. When no config file is found, a default configuration is generated that
   points extension commands to `tau component <name>` using the current
   binary's path.

### Separate binaries are still possible

Each component crate (`tau-agent`, `tau-ext-fs`, `tau-ext-shell`) has its
own `main.rs` that calls `run_stdio()`. These produce standalone binaries
(`tau-agent`, `tau-ext-fs`, `tau-ext-shell`) that can be used independently
— for example, in a config file:

```toml
[[extensions]]
name = "agent"
command = "tau-agent"
role = "agent"
```

This works because the extension protocol is the same regardless of
whether the binary is the unified `tau` or a standalone extension.

## Crate layout

| Crate | Role |
|---|---|
| `tau` | Unified binary (thin wrapper) |
| `tau-cli` | CLI argument parsing, harness runtime, public API |
| `tau-cli-term-raw` | Low-level terminal prompt with async output and diff rendering |
| `tau-proto` | CBOR event protocol types and codec |
| `tau-config` | TOML config loading and path resolution |
| `tau-core` | Event bus, tool registry, session store, policy store |
| `tau-supervisor` | Child process launch and lifecycle management |
| `tau-socket` | Unix socket transport |
| `tau-agent` | Deterministic agent extension |
| `tau-ext-fs` | Filesystem tool extension (fs.read, demo.echo) |
| `tau-ext-shell` | Shell tool extension (shell.exec) |
| `tau-test-support` | Test utilities and fixtures |
