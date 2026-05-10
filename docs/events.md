# Event log reference

The tau bus is fact-based: components broadcast what happened, never requests
or replies. Every event has a dotted name `<category>.<call>` and a typed
payload defined in `crates/tau-proto/src/events.rs`. This document groups the
core events by the component (or class of component) that emits them.

Events are distinct from **messages**: messages are point-to-point control-plane
traffic (handshake, subscribe/intercept, the `LogEvent`/`Ack` envelope, etc.)
and never appear on the bus or in the durable session log. See
[messages.md](messages.md) for the message-side reference.

A few categories don't map to a single emitter — those are grouped by the
class of function that raises them.

## Harness (general)

Emitted by the harness daemon itself, mostly for UI-facing status and
for control of the emit/intercept pipeline.

- **`harness.info`** — A free-form informational message from the
  harness for the user, with a severity (`normal` / `important`). Used
  for things like `/tree` rendering and ad-hoc notices.
- **`harness.models_available`** — The full list of configured models
  as `provider/model_id` strings. Re-emitted when configuration changes.
- **`harness.model_selected`** — Which model is currently selected, plus
  its context-window size if known.
- **`harness.context_usage_changed`** — Updated input/cached token counts
  and percent-of-context-window for the selected model, after each agent
  response that reports usage.
- **`harness.effort_changed`** — The current reasoning-effort level
  (`off` / `minimal` / `low` / `medium` / `high` / `xhigh`).
- **`harness.efforts_available`** — Which effort levels are valid for the
  currently selected model. Empty when no model is selected or the
  provider doesn't support reasoning.

## Session (harness session tracker)

Emitted by the harness's session tracker. They drive the durable session
tree and the prompt lifecycle.

- **`session.started`** — A session was created or switched to. Carries
  a reason (`initial` startup, `new` via `/session new`, `resume` of an
  existing session). Extensions react with per-session setup and reply
  with `extension.context_ready`.
- **`session.shutdown`** — The harness is leaving the current session,
  emitted before `session.started` for the next one. Extensions flush or
  drop per-session state.
- **`session.prompt_queued`** — A user prompt arrived while the agent
  was busy and was queued instead of dispatched.
- **`session.prompt_steered`** — A previously queued prompt is being
  folded into the in-flight turn as a steering message rather than
  starting a fresh turn. Folds into the session tree as one user
  message at the current head.
- **`session.prompt_created`** — The harness persisted a prompt and
  assigned it an id; payload carries the assembled system prompt,
  message history, available tools, model, effort, thinking-summary
  setting, and originator. This is the input handed to the agent.
- **`session.user_message_injected`** — A synthetic user message
  inserted by the harness (e.g. `!`-shell command output, AGENTS.md
  preamble). Folds into the session tree like a real user prompt.

## Agent

Emitted by the agent backend (tau-agent, or any drop-in replacement).

- **`agent.prompt_submitted`** — The agent accepted a `session.prompt_created`
  and started processing it. Echoes the originator.
- **`agent.response_updated`** — Streaming update with the full text so
  far (replace, not delta) and accumulated reasoning summary if any.
  Transient by default.
- **`agent.response_finished`** — Final response: text, any tool calls
  the agent wants to make, usage tokens, final thinking summary,
  echoed originator. Routed by the harness based on the originator.

## Tools

Tool events span three emitters: extensions register/implement tools,
the agent requests calls, and the harness orchestrates dispatch.

- **`tool.register`** *(extension)* — A tool provider advertises a tool
  spec (name, description, JSON-schema parameters, side-effect class).
- **`tool.unregister`** *(extension)* — A previously registered tool is
  withdrawn.
- **`tool.request`** *(agent)* — The agent asks for a tool call by id,
  name, and CBOR arguments. Goes through the harness's dispatch queue.
- **`tool.invoke`** *(harness)* — The harness has decided to run a
  request and is dispatching it to the tool's implementing extension.
- **`tool.result`** *(extension)* — Successful tool result, by call id.
- **`tool.error`** *(extension)* — Tool failure with a message and
  optional structured details.
- **`tool.progress`** *(extension)* — In-flight progress update with an
  optional message and current/total counters. Transient.
- **`tool.cancel`** *(harness)* — The harness asks an extension to
  cancel an in-flight call.
- **`tool.cancelled`** *(extension)* — The extension acknowledges that a
  call has been cancelled.
- **`tool.delegate_progress`** *(harness)* — Live snapshot of a sub-agent
  spawned by the `delegate` tool: tools-in-flight, total, context
  tokens, percent. Transient; the UI re-renders the parent tool block.

## Extensions

Two sub-classes:

### Extension supervision (harness supervisor)

Emitted by the harness's supervisor as it manages child extension
processes.

- **`extension.starting`** — A child extension process is being spawned
  (instance id, name, pid).
- **`extension.ready`** — The extension's `Ready` message was received
  by the supervisor, which synthesizes this bus event so subscribers can
  observe that the extension is fully online.
- **`extension.exited`** — The child process exited; carries exit code
  and/or signal.
- **`extension.restarting`** — The supervisor is restarting an extension
  (attempt counter, optional reason).

### Extension-emitted

Emitted by extensions to advertise capabilities or interact with the
harness/agent.

- **`extension.skill_available`** — The extension discovered a skill on
  disk: name, description, file path, and whether to inject it into the
  system prompt.
- **`extension.agents_md_available`** — The extension discovered an
  AGENTS.md file and is shipping its contents eagerly so the harness
  can inject them without a tool round-trip.
- **`extension.context_ready`** — The extension finished publishing
  refreshed prompt context for one session (the reply to
  `session.started`).
- **`extension.agent_query`** — The extension asks the harness to
  dispatch a side prompt to the agent: instruction text, correlation
  `query_id`, optional tool-call attribution and human-readable task
  name (used by the `delegate` tool).
- **`extension.agent_query_result`** — The agent's final answer to an
  earlier `extension.agent_query`, routed point-to-point back to the
  requesting extension. Carries the same `query_id`.
- **`extension.event`** — Custom extension-defined event with a free-form
  dotted name and CBOR payload. The harness routes it like any other
  event; if `session_id` is set it can be folded into that session's
  durable log.

## UI

Emitted by attached UI clients (tau-cli-term, etc.) to express user
intent.

- **`ui.prompt_submitted`** — The user submitted a prompt: session id,
  text, originator (defaults to `user`; reused for extension-driven
  side prompts).
- **`ui.prompt_draft`** — Trailing-edge debounced (≤1/s) snapshot of the
  current draft buffer. Transient — used for "user is alive" signals
  (e.g. notification idle reset), not persisted.
- **`ui.model_select`** — User requests a model switch.
- **`ui.set_effort`** — User requests a reasoning-effort change.
- **`ui.detach_request`** — UI is detaching but wants the daemon to keep
  running so a later `tau --attach` can reconnect.
- **`ui.shell_command`** — User submitted a `!` (in-context) or `!!`
  (UI-only) shell command. Carries command id, command, session id,
  `include_in_context` flag.
- **`ui.switch_session`** — User wants to switch to a different session
  in the same daemon, with `new`/`resume` reason.
- **`ui.tree_request`** — User typed `/tree`: render the session
  branching tree to chat.
- **`ui.navigate_tree`** — User typed `/tree <id>`: move the session
  head to that node so the next prompt branches off there.

## Shell (shell extension, user-initiated commands)

Emitted by `tau-ext-shell` (or any extension implementing `!`/`!!`
commands) in response to a `ui.shell_command`.

- **`shell.command_progress`** — A chunk of stdout/stderr from a running
  user-initiated shell command, correlated by `command_id`. Transient.
- **`shell.command_finished`** — A user-initiated shell command exited
  or was cancelled. Echoes session id, command, and `include_in_context`
  flag from the originating request, plus the truncated combined
  output, exit code, and `cancelled` flag.

## Term (terminal-output side effects)

Targeted at whichever UI is attached and capable of writing escape
sequences to a real terminal. Any component may emit these; the UI is
the only consumer. Components without a terminal silently no-op.

- **`term.osc1337_set_user_var`** — Ask the UI to write an iTerm2
  OSC 1337 `SetUserVar` escape sequence. The UI base64-encodes the
  value and tmux-wraps if needed. Useful for surfacing notifications,
  build status, or other state to terminal-side tooling.
