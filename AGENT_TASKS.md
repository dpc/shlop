# tau Agent Task Briefs

Status: draft

## Purpose

This document turns `IMPLEMENTATION_PLAN.md` into dispatchable work for parallel agents.

Use it to:

- pick the next highest-priority ready task
- understand dependencies
- keep task boundaries clean
- verify completion with clear acceptance criteria

## Dispatch rules

- Prefer tasks that unblock multiple later tasks.
- Prefer vertical progress over speculative abstraction.
- Do not broaden scope inside a task unless it is required to satisfy acceptance criteria.
- If a task reveals a missing shared interface, define the thinnest version needed and document it.
- Keep protocol and crate interfaces small until the first end-to-end loop works.

## Priority order

### Tier 0: unblock the workspace

- P0. Workspace bootstrap

### Tier 1: define the common substrate

- P1. Protocol crate
- P2. Config crate

### Tier 2: build the core runtime seam

- P3. Core bus
- P4. Tool registry
- P7. Session store

### Tier 3: connect to real processes and clients

- P5. Supervised stdio transport
- P6. Unix socket transport

### Tier 4: prove the first vertical slice

- P8. Minimal agent extension
- P9. First tool extension
- P10. CLI

### Tier 5: become useful for coding work

- P11. Filesystem extension
- P12. Shell extension

### Tier 6: harden the system

- P13. Policy hooks
- P14. Integration test harness

## Ready-to-run parallel batches

### Batch A

Run first:

- P0

### Batch B

Once P0 is done, run in parallel:

- P1
- P2

### Batch C

Once P1 is done, run in parallel:

- P3
- start P5 with provisional interfaces if needed

### Batch D

Once P3 is mostly usable, run in parallel:

- P4
- P7
- P6

### Batch E

Once P4 and P5 are done, run in parallel:

- P8
- P9

### Batch F

Once P7 and at least one transport path are done, run:

- P10

### Batch G

Once the first vertical slice works, run in parallel:

- P11
- P12
- P14

### Batch H

After core behavior stabilizes, run:

- P13

## Dependency summary

| Task | Depends on |
|---|---|
| P0 | none |
| P1 | P0 |
| P2 | P0 |
| P3 | P1 |
| P4 | P1, P3 |
| P5 | P1, P3 |
| P6 | P1, P3 |
| P7 | P3 |
| P8 | P1, P4 |
| P9 | P1, P5 |
| P10 | P3, one transport, P7 |
| P11 | P1, P5 |
| P12 | P1, P5 |
| P13 | P3, P7 |
| P14 | P1, transports, P3 |

## Task briefs

## P0. Workspace bootstrap

Priority: highest

Objective:

Create a Rust workspace skeleton that supports parallel implementation without locking in unnecessary complexity.

Scope:

- initialize workspace manifests
- create initial crate directories
- set up formatting and linting
- add a minimal top-level README or contributor note

Non-goals:

- implementing protocol logic
- implementing runtime behavior

Expected outputs:

- compilable workspace skeleton
- crate names aligned with `IMPLEMENTATION_PLAN.md`, or documented deviations
- documented crate responsibilities

Acceptance criteria:

- `cargo check` succeeds
- workspace layout is understandable without tribal knowledge

## P1. Protocol crate

Priority: highest after P0

Objective:

Implement the smallest useful CBOR event protocol layer shared by all participants.

Scope:

- event envelope type
- event naming representation
- CBOR encoder/decoder for a stream of messages
- initial typed payloads for lifecycle, tool, message, and extension supervision events

Non-goals:

- business logic routing
- transport-specific process management

Expected outputs:

- `tau-proto` crate
- round-trip tests for representative messages
- a small protocol README or module docs

Acceptance criteria:

- representative events round-trip through CBOR unchanged
- the codec is transport-agnostic
- event shapes map cleanly to the design doc taxonomy

## P2. Config crate

Priority: highest after P0

Objective:

Implement TOML configuration loading and additive user/project layering.

Scope:

- config structs
- TOML parsing
- user config loading
- optional project config loading
- additive merge logic for extension entries

Non-goals:

- permissions policy beyond shape placeholders
- process startup

Expected outputs:

- `tau-config` crate
- sample config fixture coverage
- merge semantics documented in code or tests

Acceptance criteria:

- user config loads automatically from the chosen default path logic or a stubbed loader path
- project config can be enabled and appended on top
- project config does not disable user extensions

## P3. Core bus

Priority: highest after P1

Objective:

Build the core connection and event-routing backbone.

Scope:

- internal event bus abstraction
- subscription registry
- connection abstraction shared by stdio and Unix socket clients
- per-client routing and filtering hooks

Non-goals:

- process supervision details
- concrete tool registry behavior beyond what is required for routing hooks

Expected outputs:

- `tau-core` bus layer
- in-memory test clients
- tests for subscription filtering

Acceptance criteria:

- clients can subscribe and only receive allowed subscribed events
- internal routing is independent of transport choice
- the abstraction is sufficient for later stdio and socket adapters

## P4. Tool registry

Priority: high

Objective:

Track tool providers by live connection and route tool requests to available providers.

Scope:

- tool registration state
- unregister-on-disconnect behavior
- duplicate registration warning behavior
- best-effort provider selection at call time
- `tool.request` to directed `tool.invoke` bridging hooks

Non-goals:

- advanced middleware or interception
- sophisticated conflict resolution

Expected outputs:

- registry component in `tau-core`
- tests for registration, duplicate warnings, and disconnect cleanup

Acceptance criteria:

- a provider can register a tool and later receive invocations
- duplicate registrations are allowed and warned about
- disconnect removes stale providers

## P5. Supervised stdio transport

Priority: high

Objective:

Enable the core to start supervised child processes and talk to them over stdio.

Scope:

- child process launcher
- stdin/stdout CBOR stream adapters
- disconnect detection
- lifecycle cleanup hooks
- extension lifecycle event emission

Non-goals:

- Unix socket attach
- policy system

Expected outputs:

- `tau-supervisor` crate or equivalent module
- dummy child-process integration test

Acceptance criteria:

- the core can launch a child and exchange protocol events over stdio
- child exit is detected reliably
- tool registrations from a dead child are removed

## P6. Unix socket transport

Priority: high

Objective:

Allow later-attached external clients to speak the same protocol over Unix sockets.

Scope:

- Unix socket listener
- accept loop
- per-connection CBOR adapters
- integration with the shared connection abstraction

Non-goals:

- rich CLI UX
- process supervision

Expected outputs:

- `tau-socket` crate or equivalent module
- socket transport tests

Acceptance criteria:

- an external client can attach after core startup
- the same protocol codec and connection model work over the socket path
- attached clients participate in normal subscription and routing behavior

## P7. Session store

Priority: high

Objective:

Persist the minimum session state needed for the MVP.

Scope:

- message history persistence
- tool execution record persistence
- session identifiers or equivalent basic session handles
- minimal resume primitives

Non-goals:

- full Pi parity for all stored state
- complex migration machinery

Expected outputs:

- storage abstraction in `tau-core` or a dedicated crate
- tests for append and reload behavior

Acceptance criteria:

- user and agent messages survive process restart through stored state
- tool activity can be associated with a session record

## P8. Minimal agent extension

Priority: vertical-slice critical

Objective:

Implement the smallest possible first-party agent process that proves the architecture.

Scope:

- handshake and subscribe
- receive `message.user`
- emit `tool.request`
- receive tool outcome events
- emit `message.agent`

Non-goals:

- sophisticated planning
- advanced model/provider integration

Expected outputs:

- `tau-agent` crate
- one deterministic or very simple agent behavior for testing

Acceptance criteria:

- the agent can participate as a normal process client
- one scripted conversation path works end to end

## P9. First tool extension

Priority: vertical-slice critical

Objective:

Implement one trivial tool extension for the first end-to-end loop.

Scope:

- handshake and subscribe
- register one tool
- handle one `tool.invoke`
- emit `tool.result` or `tool.error`

Non-goals:

- useful real-world functionality if it slows down the first slice

Expected outputs:

- simple extension crate
- fixtureable deterministic behavior

Acceptance criteria:

- the extension can be supervised over stdio
- the core can invoke its tool and receive a result reliably

## P10. CLI

Priority: vertical-slice critical

Objective:

Provide the minimal user-facing way to drive `tau`.

Scope:

- embedded mode startup
- daemon attach mode
- minimal interactive prompt or message-send command
- render conversation messages and key lifecycle events

Non-goals:

- full TUI
- tmux integration

Expected outputs:

- `tau-cli` crate
- one ergonomic path for embedded use
- one ergonomic path for daemon attach

Acceptance criteria:

- user can send a message and see the response in embedded mode
- user can later attach to a daemon and continue interaction

## P11. Filesystem extension

Priority: useful-coding-work high

Objective:

Add a real filesystem-oriented extension for coding workflows.

Scope:

- at least one file read tool
- useful error handling
- optional later expansion to write or list operations

Non-goals:

- exhaustive filesystem API

Expected outputs:

- `tau-ext-fs` crate
- tests with temporary directories

Acceptance criteria:

- agent can request file reads through the core and receive content
- expected error paths are surfaced cleanly

## P12. Shell extension

Priority: useful-coding-work high

Objective:

Add a shell-oriented extension for command execution.

Scope:

- command execution tool
- `tool.progress` support where useful
- `tool.error` support

Non-goals:

- sandboxing perfection in the first version

Expected outputs:

- `tau-ext-shell` crate
- basic execution tests

Acceptance criteria:

- agent can request command execution and receive results
- failure and progress behavior are represented through events

## P13. Policy hooks

Priority: medium after vertical slice

Objective:

Add the first practical enforcement hooks without overbuilding the permission system.

Scope:

- subscription-time policy checks
- persisted approval decisions where needed
- separate handling path for supervised extensions and socket clients

Non-goals:

- full long-term permission model
- sandbox implementation details

Expected outputs:

- policy module or hooks in core
- tests for allowed and rejected subscriptions

Acceptance criteria:

- forbidden subscriptions are denied clearly
- approved behavior is persisted as needed for the MVP

## P14. Integration test harness

Priority: medium-high once transports exist

Objective:

Create reusable end-to-end coverage for the architecture.

Scope:

- child process test fixtures
- Unix socket client fixtures
- scripted end-to-end scenarios
- restart and disconnect tests

Non-goals:

- exhaustive property testing before the MVP exists

Expected outputs:

- test utilities usable by multiple crates
- first end-to-end scenario tests

Acceptance criteria:

- the first vertical slice is covered by automated integration tests
- restart and unregister behavior are covered

## Recommended agent assignments

If multiple agents are available, a good initial split is:

- Agent 1: P0, then P1
- Agent 2: P2, then P7
- Agent 3: P3, then P4
- Agent 4: P5, then P6
- Agent 5: P8, then P9
- Agent 6: P10

After the first vertical slice works:

- Agent 1 or 5: P11
- Agent 4 or another systems-focused agent: P12
- Agent 2 or 3: P13
- Agent 6 or a QA-focused agent: P14

## Definition of coordination success

Coordination is going well if:

- agents are blocked mostly by explicit dependencies, not by unclear ownership
- shared interfaces are introduced in small, reviewable increments
- the first vertical slice lands before major API polishing begins
- protocol changes are driven by end-to-end needs rather than anticipation
