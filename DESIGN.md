# tau Design

Status: evolving draft

## Summary

`tau` is a Unix-native LLM agent harness inspired by Pi, but built around operating system primitives instead of a TypeScript/npm runtime.

Core idea:

- everything important is a Unix process
- extensions are standalone binaries
- integration happens over CBOR event streams and event routing
- hot reload is done by rebuilding and restarting processes
- installation is just installing binaries, scripts, or `nix run ...` entrypoints
- extensions can be written in any language

The design goal is to preserve the strengths of Pi's agent model while making the system easier to package, synchronize across machines, sandbox, and extend in a Nix/Unix-centric workflow.

Longer term, `tau` should aim to match the feature set of the Pi agent harness, while replacing the implementation substrate with a Unix-process architecture.

Decision policy for this document:

- once the main architectural direction is clear, unresolved lower-level details should default to the simplest design that preserves the architecture and keeps parity with Pi-like workflows plausible
- these decisions are provisional and can be revised later
- only genuinely architecture-shaping uncertainties need explicit discussion before proceeding

## Motivation

The current pain point is extension development and deployment in an npm-centric ecosystem.

Problems to solve:

- avoid forcing extension authors into Node.js/TypeScript
- make extensions easy to distribute in Nix/dotfiles setups
- let each extension run as its own isolated process
- make sandboxing and permissions a first-class part of the model
- reduce runtime complexity by leaning on ubiquitous Unix facilities
- support simple operational workflows: rebuild, restart, reconnect

## Vision

`tau` should feel like a process-oriented, local-first agent operating system for developers.

Desired properties:

- local-first and Unix-first
- language-agnostic extension model
- easy packaging and deployment
- composable over standard process boundaries
- secure by default, with per-process isolation opportunities
- usable with minimal UI assumptions

## Initial product shape

At a high level, `tau` consists of:

- a core harness process
- one or more extension processes
- one or more user interfaces
- one or more model/tool providers
- a bidirectional event protocol over streams

The agent itself should also be modeled as an extension process rather than being permanently embedded in the core harness.

The wire format direction is CBOR over streams.

CBOR's self-delimiting encoding is expected to be used directly on the stream, so a stream can carry a sequence of CBOR data items without an extra framing layer.

Transport should support both:

- stdio for child processes launched and supervised by the core
- Unix sockets for separately attached clients such as external CLIs, TUIs, or external RPC systems

The protocol should stay the same across transports, but connected client classes may be treated differently by policy and routing.

The protocol direction is event-based rather than RPC-based:

- every message is an event plus payload
- extensions and UIs send events into the core
- the core maintains a global internal event bus
- clients subscribe to the events they want
- each connected client receives only the subset of events it subscribed to and is allowed to see
- the harness can also send directed events to a specific process connection
- there is no protocol-level request/response pairing
- there is no protocol-level event acknowledgment
- if a message is successfully written to the stream, it is treated as delivered at the transport level
- when correlation is needed, such as for tool execution, the relevant event payload carries its own operation identifier

The architecture should be modular enough to support multiple runtime shapes:

- daemon mode: a long-lived background harness process
- embedded mode: the CLI starts and owns the harness process for a session
- hybrid mode: components still communicate through the same RPC/event mechanisms, even when started together

In practice, even embedded mode behaves similarly to a daemon during a session, because the harness is responsible for brokering RPCs and events across participants.

Possible user interfaces:

- a tmux-based workflow with dedicated panes/windows
- a CLI for sending messages and invoking actions
- a TUI for richer interaction

The exact UI is intentionally decoupled from the core harness.

This implies a stronger separation of concerns:

- the harness core is a broker/supervisor/state host
- the harness core also owns the internal global event bus and per-client routing/filtering
- the agent is one participant on the protocol
- tools are provided by extensions
- UIs are separate participants

## Extension model

Extensions are standalone executables.

This includes not only tool providers, but potentially the agent itself and other high-level components.

For supervised extensions, the preferred transport is stdio. Separately attached Unix socket clients are expected to be mostly UIs or external RPC systems rather than ordinary extensions.

An extension:

- starts as an ordinary process
- is launched and supervised by the harness from configured commands
- usually communicates with the harness over its stdin/stdout streams
- performs a handshake
- sends events to the harness
- subscribes to and receives events
- registers capabilities with the harness through the protocol
- may initiate actions on its own, not only respond to harness-originated events

A key design point is that tool ownership is explicit.

Expected flow:

- an extension registers one or more tools by sending registration events
- the harness records which connected extension instances currently provide each tool
- when the agent invokes a tool, the harness sends a directed tool invocation event to one currently available provider
- the tool invocation payload may include a call or operation identifier generated by the harness
- the extension later sends a tool result event back to the harness, referring to that identifier when needed

Lifecycle behavior:

- tools are registered on extension startup
- tools are automatically unregistered when the extension disconnects
- disconnect may be intentional, caused by a crash, or part of a restart/hot reload
- duplicate tool registrations are allowed
- duplicate registrations should produce a warning
- at call time, the harness routes the request on a best-effort basis to whichever provider is currently available

This keeps tool availability tied to live connections and allows overlapping providers without hard registration failures.

Future direction:

- the architecture will likely need a more robust interception or middleware model, so one extension can observe, modify, wrap, or override tool usage registered by another extension
- this is intentionally deferred for now

Consequences of this model:

- hot reload is just restart
- crash isolation is natural
- per-extension sandboxing is possible
- language choice is unrestricted
- installation can be as simple as placing a binary in PATH or referencing a command

Examples of extension installation forms:

- packaged binary in the system profile
- binary managed by dotfiles/home-manager/NixOS
- script or compiled executable in a project
- `nix run github:dpc/tau-todo`

## Configuration and discovery

Extensions are configured explicitly rather than discovered implicitly.

Configuration layers:

- user-scoped config, loaded automatically
- project-scoped config, optionally enabled and added on top

The central abstraction is simple:

- a configured extension is a command to start as a process

This keeps extension management aligned with normal Unix and Nix workflows.

Likely implications:

- config layering is additive rather than overriding
- project config can register extra extensions, but does not disable user-configured ones
- commands may reference binaries, scripts, or `nix run ...` invocations
- the harness owns process supervision for configured extensions
- extensions are started eagerly in the MVP
- lazy or on-demand startup can be added later
- conceptually, this plays the same role as Pi loading its own TypeScript extensions, except the extension boundary is a process boundary instead of in-process code loading

## Config format sketch

The configuration model should stay simple and centered on process startup.

Core idea:

- config declares a list of extensions to run
- each extension entry primarily describes a command to execute
- user config is always loaded
- project config is optional and additive

A likely extension entry needs at least:

- a human-readable name or identifier
- a command to execute
- optionally arguments or a full argv form
- optionally metadata for future policy, permissions, or startup behavior

Likely future fields:

- transport preference if needed
- startup mode such as eager or lazy
- permission hints or policy declarations
- tags, grouping, or role markers such as agent, tool provider, or UI

Config syntax:

- `tau` configuration should use TOML

This keeps the native config format simple and editable, while still allowing external systems such as Nix to generate it if desired.

## Example TOML config sketch

This is an illustrative shape, not a finalized schema.

```toml
[core]
mode = "embedded"

[[extensions]]
name = "agent"
command = "tau-agent"
args = ["--model", "claude"]
role = "agent"

[[extensions]]
name = "fs"
command = "tau-fs"
role = "tool"

[[extensions]]
name = "git"
command = "tau-git"
role = "tool"

[[extensions]]
name = "todo"
command = "nix"
args = ["run", "github:dpc/tau-todo"]
role = "tool"
```

Project config follows the same general structure and appends more `[[extensions]]` entries when enabled.

Possible future extensions to the schema:

- `enabled = true/false` for local toggling inside one config file
- explicit policy fields
- startup strategy fields
- tags and grouping
- transport overrides for unusual cases

## Process topology sketch

### Embedded mode

In embedded mode, a CLI command starts the core for the lifetime of one interactive session.

Typical structure:

- `tau` CLI starts the core
- the core loads user config, then optional project config
- the core starts supervised child processes over stdio
- the core may also expose a Unix socket for additional clients during that session
- the CLI may act as both launcher and attached UI client

This mode is optimized for fast startup and straightforward local use.

### Daemon mode

In daemon mode, the core is a long-lived process.

Typical structure:

- a background `tau` core process runs per user, workspace, or explicit session scope
- it loads configuration and supervises configured child processes
- external CLIs, TUIs, or automation helpers attach over a Unix socket
- supervised child processes still prefer stdio

This mode is optimized for reconnectability, richer UI attachment, and long-running state.

### Shared invariants

Regardless of mode:

- the core owns supervision, persistence, routing, and policy enforcement
- the agent is a process participant, not hardcoded into the core
- supervised tool extensions are normal child processes
- all participants speak the same CBOR event protocol
- transport differences are adapter details, not protocol forks

## Event taxonomy sketch

Event names should use dotted identifiers such as `tool.invoke`.

Initial event families:

### Lifecycle and connection

- `lifecycle.hello`
- `lifecycle.subscribe`
- `lifecycle.ready`
- `lifecycle.disconnect`

Purpose:

- identify the client
- negotiate basic protocol expectations
- declare subscriptions
- signal readiness and teardown

### Tools

- `tool.register`
- `tool.unregister`
- `tool.invoke`
- `tool.result`
- `tool.error`
- `tool.progress`
- `tool.cancel`
- `tool.cancelled`

Purpose:

- register and remove tool availability
- invoke a tool on a selected provider
- return successful results
- report failures explicitly
- report progress explicitly for long-running work
- request cancellation
- report that a running tool invocation was cancelled

### Chat and session

- `session.start`
- `session.resume`
- `message.user`
- `message.agent`

Purpose:

- represent session lifecycle
- carry user messages into the system
- carry agent responses and related conversation state

### UI

- `ui.input`
- `ui.render`
- `ui.notify`

Purpose:

- let external UIs submit user actions
- receive renderable state or presentation-oriented events
- receive notifications

### Extension supervision

- `extension.starting`
- `extension.ready`
- `extension.exited`
- `extension.restarting`

Purpose:

- expose supervised process lifecycle
- help UIs and operators understand extension status
- support debugging and operational visibility

Notes:

- these names are placeholders for the first protocol sketch, not a finalized schema
- event payloads will carry any needed operation identifiers such as tool call IDs
- tool completion-related events are intentionally split rather than multiplexed through a single result payload
- cancellation is modeled explicitly as events as well, keeping the protocol uniformly event-driven
- more event families will likely be needed for permissions, state sync, supervision control, and model/provider interaction

## Sample event flow

This section tests the current model against one concrete scenario.

Scenario:

- a CLI UI is attached to the harness
- an agent process is running as an extension
- a filesystem tool extension is running as a supervised child process
- the user asks the agent to read a file

Illustrative flow:

1. Core starts supervised processes.

   - core emits `extension.starting` for the agent extension
   - core emits `extension.starting` for the filesystem extension

2. Each process handshakes and subscribes.

   - agent sends `lifecycle.hello`
   - agent sends `lifecycle.subscribe`
   - filesystem extension sends `lifecycle.hello`
   - filesystem extension sends `lifecycle.subscribe`

3. The filesystem extension registers its tool.

   - filesystem extension sends `tool.register`
   - payload describes something like `fs.read`
   - core records that this live connection currently provides that tool
   - core may emit a visibility event later if UIs need to reflect tool availability

4. The user enters a message in the UI.

   - CLI sends `ui.input` or `message.user`
   - core persists the message as part of session state
   - core routes the event to the agent because the agent subscribed to it

5. The agent decides to invoke the tool.

   - agent emits an event requesting tool use
   - core resolves a currently available provider for the requested tool
   - core creates a tool call identifier if needed
   - core sends a directed `tool.invoke` event to the selected filesystem extension

6. The tool extension executes the work.

   - filesystem extension reads the file
   - if the operation takes time, it may emit `tool.progress`
   - if the operation fails, it emits `tool.error`
   - if the operation succeeds, it emits `tool.result`
   - payload refers to the tool call identifier

7. Core routes the outcome.

   - core records the tool execution in session or tool history
   - core routes tool completion information to the agent
   - core may also route selected visibility events to subscribed UIs

8. The agent responds to the user.

   - agent emits `message.agent`
   - core persists the response
   - core routes the response to subscribed UIs

Observations:

- the protocol remains event-only throughout
- correlation is carried in payload fields such as tool call IDs
- the core remains responsible for routing, persistence, supervision, and provider selection
- extensions remain simple stream-speaking processes

Working assumptions from this flow:

- the agent emits `tool.request` when asking the core to use a tool
- the core normalizes user-originated input into `message.user` for session history, while `ui.input` remains a UI-facing input event
- ordinary UIs should be able to subscribe to relevant message, tool, and extension supervision events, subject to policy

## Packaging and developer experience

The harness should make it easy to build extensions, but should not require a specific language runtime.

Likely support:

- a stable CBOR-based event protocol shared across transports
- a small Rust crate or crates for protocol bindings and stream decoding ergonomics
- easy extension scaffolding
- simple examples in multiple languages

Principle:

- protocol first
- helper libraries second
- runtime lock-in never

## Security model direction

The process boundary is a core design feature, not an implementation detail.

This should enable:

- per-extension permissions
- per-extension sandboxing
- OS-level isolation mechanisms such as namespaces, seccomp, landlock, or similar techniques where available
- clear auditability of what binary is being executed

Current direction:

- do not overdesign permissions yet
- extensions are explicitly listed in config, which is the first trust and approval boundary
- later, config can grow to specify what each extension is allowed or not allowed to do
- when an extension starts and requests subscriptions to event families, the harness can check whether those subscriptions are permitted
- Unix socket clients are a different class of participant from supervised extensions and will likely follow different policy rules

## Implementation roadmap sketch

A more dispatch-oriented execution plan lives in `IMPLEMENTATION_PLAN.md`.

A reasonable implementation order is:

1. core event bus and connection abstraction
   - unify stdio and Unix socket clients behind one internal connection model
   - encode and decode CBOR event streams

2. supervised process management
   - start configured extensions
   - monitor exit status
   - emit supervision lifecycle events

3. minimal protocol and session storage
   - handshake
   - subscribe
   - message persistence
   - tool registration state

4. first agent process and first tool extension
   - prove end-to-end tool request, invoke, result flow
   - keep the first example extremely small

5. CLI UI
   - send user messages
   - render conversation and key lifecycle events
   - connect in embedded mode first, then daemon attach

6. permissions and policy hooks
   - enforce subscription-time checks
   - persist approval decisions where needed

7. protocol hardening and extension SDKs
   - publish Rust crates
   - write reference extensions
   - stabilize the first public protocol subset

This order intentionally proves the architectural seams before growing the product surface.

## MVP direction

The first usable version of `tau` should prioritize a small but coherent local-first core.

Proposed MVP scope:

- single-user local operation
- daemon mode and CLI-driven embedded mode
- explicit user-scoped and project-scoped extension configuration
- harness-owned extension launch and supervision
- eager startup of configured extensions
- CBOR event protocol over stdio and Unix sockets
- extension handshake and capability registration
- tool registration, tool invocation, and tool result events
- an agent implemented as an extension process
- basic conversation/session persistence
- basic permission tracking and approval persistence
- enough UI to drive the harness from a CLI, with richer TUI/tmux workflows allowed to come later

Likely MVP non-goals:

- full Pi feature parity from day one
- advanced interception, middleware, or tool wrapping
- multi-user or remote-first operation
- elaborate UI integration beyond what is needed for a usable local workflow
- lazy or on-demand extension startup
- complex load balancing or deterministic conflict resolution for duplicate tool providers

Rationale:

- prove the process architecture first
- prove the extension protocol first
- make extension authoring viable before polishing advanced orchestration features
- leave room to grow toward Pi-level feature coverage without overcommitting the first implementation

## Non-goals, at least initially

- requiring npm/Node.js to build ordinary extensions
- coupling extension authoring to a single implementation language
- making the UI architecture the center of the system

## Default working decisions

These are provisional defaults for moving the design forward without blocking on every detail.

### Product and scope

- `tau` is local-first and primarily targets interactive coding workflows first
- automation and CI matter, but are not the first design center
- the first release should intentionally narrow scope to get the core process architecture right
- multi-user and remote-first operation are out of scope initially

### Core architecture

- the core should expose one internal event bus abstraction with transport-specific connection adapters underneath
- the event envelope should stay simple, likely a small CBOR map containing event name plus payload
- stdio and Unix sockets should share the same connection model in the core
- restart behavior should default to supervised restart with visible lifecycle events
- reconnecting extensions are expected to resubscribe and reregister capabilities after restart
- correlation identifiers should be used whenever an event family needs multi-step tracking, not only for tools
- the agent is a unified participant in the same protocol, but with some special responsibilities in orchestration and session progression
- externally attached Unix socket clients should be treated as a distinct client class from supervised extensions

### Extension model

- extensions may eventually expose tools, views, prompts, hooks, background jobs, and providers
- the first practical extension API priority is tool provision plus event subscription/publication
- future interception and middleware should be designed as an extension of the event model rather than a separate subsystem
- important extension-initiated actions should become explicit event types rather than ad hoc messages
- configured extensions should get sensible default subscriptions, with tighter permission policy added later

### UX and UI

- tmux is a valid reference workflow, not the only official direction
- the first UI should be minimal, scriptable, and sufficient for day-to-day use
- richer TUI behavior can be added later
- shell workflow should remain largely in the user's own shell, with selective integration where useful

### State and reproducibility

The long-term direction is broad state coverage comparable to Pi-style harness features.

Expected persisted state eventually includes:

- conversation history
- tool logs and execution records
- permissions and approval decisions
- extension registry and configuration-derived runtime metadata
- session snapshots or resumable session state
- UI state where useful

Likely principle:

- persist meaningful user-facing and harness-operational state
- do not treat transient process connectivity itself as durable state

Open questions:

- What should be declarative and syncable through dotfiles/Nix?
- How reproducible should sessions be across machines?
- Which persisted state belongs in project scope versus user scope versus ephemeral runtime state?

## Remaining major open questions

These are the questions still most likely to affect the architecture materially:

- how far the first release should go toward Pi parity versus staying intentionally minimal
- what the first concrete set of event schemas should look like
- how permission policy should evolve beyond config-based trust and subscription-time checks
- what the eventual interception or middleware model for tool wrapping should be
- how model/provider integration should be represented in the protocol

## Current understanding

The strongest differentiator of `tau` is not just that it is "Pi in another language". The core differentiator is a Unix process architecture where extensibility, isolation, packaging, and operations all align with how Unix systems already work.

A second important differentiator is protocol philosophy: keep the extension boundary simple by using a CBOR message stream of events rather than a heavier request/response framework.

A third important differentiator is transport philosophy: support both stdio and Unix sockets under one protocol, while preferring stdio for supervised child processes so process lifecycle and liveness follow ordinary Unix process behavior.
