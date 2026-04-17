# tau Implementation Plan

Status: draft

## Goal

Build a first usable `tau` MVP that proves the architecture in `DESIGN.md`:

- core broker/supervisor/state host
- agent as a process
- extensions as supervised processes
- CBOR event protocol over stdio and Unix sockets
- local-first interactive workflow

## Planning assumptions

These are working assumptions for execution, not permanent constraints.

- The core and first-party components should be implemented in Rust.
- External extensions should remain language-agnostic.
- The repository should become a Rust workspace with multiple crates rather than one large binary.
- The first milestone should optimize for proving the architecture, not for maximizing end-user features.
- Protocol and config stability matter early, but full schema freeze can wait until after the first end-to-end loop works.

## MVP definition

The MVP is done when all of the following are true:

- a `tau` core can start configured child processes
- supervised children can speak the CBOR event protocol over stdio
- external clients can attach over a Unix socket using the same protocol
- an agent process can receive a user message and request a tool
- a tool extension can register, receive `tool.invoke`, and send `tool.result`
- the core persists a basic session history
- a CLI can drive the system in embedded mode
- the same core can also run in daemon mode with a later-attached CLI

## Suggested repository shape

A likely initial Rust workspace split:

- `crates/tau-proto`
  - event types
  - CBOR encoding/decoding
  - shared protocol helpers
- `crates/tau-config`
  - TOML config loading
  - user/project config layering
- `crates/tau-core`
  - event bus
  - routing
  - session state
  - tool registry
  - policy hooks
- `crates/tau-supervisor`
  - child process launch and restart
  - stdio transport adapters
- `crates/tau-socket`
  - Unix socket listener and transport adapters
- `crates/tau-cli`
  - embedded mode entrypoint
  - daemon attach commands
  - minimal interactive UI
- `crates/tau-agent`
  - first-party agent process
- `crates/tau-ext-fs`
  - first-party filesystem tool extension
- `crates/tau-ext-shell`
  - first-party shell tool extension

This exact split can be collapsed if it feels too fine-grained, but the separation of concerns should remain.

## Execution strategy

A more agent-oriented dispatch breakdown lives in `AGENT_TASKS.md`.

Work should proceed in waves.

Principles:

- first prove one seam at a time
- keep each wave demonstrable
- prefer vertical slices over abstract framework work
- keep the first tool and first agent intentionally small
- defer polish until both transports and one end-to-end tool flow work

## Wave 0: Project bootstrap

Purpose:

Make the repository ready for parallel work.

Deliverables:

- Rust workspace initialized
- basic formatter and lints configured
- top-level binaries and crate boundaries chosen
- initial README or contributor notes explaining crate responsibilities
- dev shell or Nix wiring as needed

Acceptance criteria:

- `cargo check` succeeds for the empty workspace skeleton
- crate boundaries are documented well enough for parallel agents to work independently

Suggested owner:

- one infra-focused agent

## Wave 1: Protocol and config spine

Purpose:

Define the smallest stable substrate that every other component depends on.

Deliverables:

- CBOR event envelope type
- stream encoder/decoder utilities
- dotted event naming conventions represented in code
- initial protocol events for lifecycle, tools, messages, and supervision
- TOML config parser
- user config plus optional project config additive layering

Acceptance criteria:

- sample config loads successfully
- protocol crate can round-trip representative events through CBOR
- both stdio and socket transports can reuse the same protocol codec layer

Suggested parallel tasks:

- agent A: protocol types and codec
- agent B: config schema and loader

## Wave 2: Core runtime and internal bus

Purpose:

Build the actual harness core as a routing and state host.

Deliverables:

- internal event bus abstraction
- client connection abstraction independent of transport
- subscription tracking
- per-client visibility filtering
- tool registry keyed by live providers
- session/message store abstraction

Acceptance criteria:

- synthetic in-process clients can connect to the core and exchange routed events
- a client can subscribe to one event family and not receive unrelated events
- duplicate tool registrations warn but do not break the registry

Suggested parallel tasks:

- agent A: internal event bus and subscriptions
- agent B: tool registry and routing logic
- agent C: session state interfaces

## Wave 3: Supervision and transports

Purpose:

Connect the core to real processes.

Deliverables:

- supervised child process launcher
- stdio connection adapter
- Unix socket listener
- Unix socket connection adapter
- lifecycle detection and cleanup on disconnect
- extension lifecycle events from supervision

Acceptance criteria:

- the core can start a dummy child process and exchange CBOR events over stdio
- the core can accept a later Unix socket client and exchange the same events
- disconnecting a provider unregisters its tools automatically

Suggested parallel tasks:

- agent A: stdio/supervisor path
- agent B: Unix socket path
- agent C: lifecycle cleanup and integration tests

## Wave 4: First vertical slice

Purpose:

Prove the architecture with the smallest real end-to-end flow.

Deliverables:

- first-party agent process with minimal behavior
- first-party tool extension with one trivial tool
- core support for `tool.request` to `tool.invoke` routing
- tool call IDs and result correlation
- basic session persistence for user and agent messages

Recommended first slice:

- `message.user`
- agent emits `tool.request`
- tool extension receives `tool.invoke`
- tool extension emits `tool.result`
- agent emits `message.agent`

Acceptance criteria:

- one scripted interaction succeeds from CLI input to final agent response
- the persisted session history shows the user message, tool activity, and agent response

Suggested parallel tasks:

- agent A: minimal agent extension
- agent B: trivial first tool extension
- agent C: core glue for tool request routing and persistence

## Wave 5: Make it useful for coding work

Purpose:

Upgrade the minimal slice into something that resembles a real coding harness.

Deliverables:

- filesystem extension with at least read capability
- shell extension with at least command execution capability
- tool progress and tool error handling end to end
- minimal provider selection for duplicate tool names
- visible extension lifecycle events in the CLI

Acceptance criteria:

- user can ask the agent to inspect files and run a shell command
- progress and error events appear in logs or UI output
- restarting a tool extension causes reregistration without corrupting core state

Suggested parallel tasks:

- agent A: filesystem extension
- agent B: shell extension
- agent C: CLI rendering for tool and lifecycle events

## Wave 6: CLI and mode completeness

Purpose:

Make the system operable in both embedded and daemon modes.

Deliverables:

- embedded mode CLI path
- daemon start/stop/status path
- Unix socket attach from CLI
- minimal interactive chat loop
- session resume selection or equivalent basic workflow

Acceptance criteria:

- embedded mode works from one command
- daemon mode can be started, then a later CLI can attach and continue a session
- both modes drive the same core behavior and protocol

Suggested parallel tasks:

- agent A: embedded CLI
- agent B: daemon lifecycle commands
- agent C: attach/reconnect UX

## Wave 7: Policy hooks and hardening

Purpose:

Add the minimal trust and reliability features needed to make the MVP sane.

Deliverables:

- subscription-time policy checks
- persisted approval decisions where needed
- restart policy knobs
- structured logging or event tracing
- regression tests for reconnect, unregister, and duplicate providers

Acceptance criteria:

- forbidden subscriptions are rejected cleanly
- extension restart does not leak stale registrations
- tool calls fail cleanly when no provider is available

Suggested parallel tasks:

- agent A: policy hooks
- agent B: restart and recovery behavior
- agent C: test coverage and traces

## Wave 8: First public protocol subset

Purpose:

Stabilize the minimum interface needed for outside extension authors.

Deliverables:

- documented event families and payload shapes
- Rust helper crate ergonomics improved
- one or two example extensions outside the core tree shape
- extension author quickstart docs

Acceptance criteria:

- an external contributor could write a small extension against the documented protocol
- the first-party examples exercise both stdio and socket attachment models where relevant

Suggested owner:

- one docs-and-API-focused agent, with support from protocol maintainers

## Dispatchable work items

These are good agent-sized tasks that can be delegated with clear boundaries.

### P0. Workspace bootstrap

Output:

- Rust workspace skeleton
- crate layout
- contributor notes

Depends on:

- nothing

### P1. Protocol crate

Output:

- event envelope
- event enums or typed wrappers
- CBOR codec helpers

Depends on:

- workspace bootstrap

### P2. Config crate

Output:

- TOML config structs
- user/project layering
- sample config validation

Depends on:

- workspace bootstrap

### P3. Core bus

Output:

- internal bus
- subscription registry
- connection abstraction

Depends on:

- protocol crate

### P4. Tool registry

Output:

- tool registration tracking
- duplicate warning behavior
- provider resolution path

Depends on:

- protocol crate
- core bus

### P5. Supervised stdio transport

Output:

- child process launch
- stdio adapters
- lifecycle cleanup hooks

Depends on:

- protocol crate
- core bus

### P6. Unix socket transport

Output:

- listener
- attach path
- socket adapters

Depends on:

- protocol crate
- core bus

### P7. Session store

Output:

- message persistence
- tool execution log persistence
- session resume primitives

Depends on:

- core bus

### P8. Minimal agent extension

Output:

- agent process
- `tool.request` path
- simple response loop

Depends on:

- protocol crate
- tool registry

### P9. First tool extension

Output:

- one trivial tool for first end-to-end tests

Depends on:

- protocol crate
- supervised stdio transport

### P10. CLI

Output:

- embedded mode
- daemon attach mode
- basic interactive UX

Depends on:

- core bus
- at least one transport
- session store

### P11. Filesystem extension

Output:

- file read tool
- later file write if desired

Depends on:

- protocol crate
- supervised stdio transport

### P12. Shell extension

Output:

- shell execution tool
- progress/error reporting

Depends on:

- protocol crate
- supervised stdio transport

### P13. Policy hooks

Output:

- subscription-time checks
- approval persistence hooks

Depends on:

- core bus
- session or policy store

### P14. Integration test harness

Output:

- test fixtures for child processes
- end-to-end tests across stdio and Unix socket clients

Depends on:

- protocol crate
- transports
- core bus

## Critical dependency chain

The shortest path to a useful demo is:

1. workspace bootstrap
2. protocol crate
3. core bus
4. supervised stdio transport
5. tool registry
6. minimal agent extension
7. first tool extension
8. CLI

If schedule pressure appears, optimize this chain first.

## Things explicitly deferred

These should not block the MVP:

- sophisticated middleware or tool interception
- remote or multi-user orchestration
- rich TUI work
- full permission model
- protocol freeze for every future event family
- advanced extension discovery beyond config
- lazy extension startup

## Recommended first demo

The first demo should be deliberately small:

- start `tau` in embedded mode
- load one agent extension and one filesystem extension
- user asks to read a file
- agent requests `fs.read`
- filesystem extension returns file content
- agent answers using that content
- session history is persisted

This demo is enough to validate the central architecture.

## Recommended second demo

The second demo should prove the transport split:

- start a daemon core
- supervised agent and tools run over stdio
- later attach a CLI over a Unix socket
- continue a session successfully
- restart one extension and observe clean reregistration

This demo is enough to validate the transport story.
