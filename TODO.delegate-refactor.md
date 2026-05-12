# TODO: move delegate status reporting fully into `tau-ext-core-delegate`

## Goal

Have `tau-ext-core-delegate` own end-to-end status reporting for the
`delegate` tool — progress + final completion display — without the
harness or CLI carrying delegate-specific code. Generalizes to any
future "supervisor" extension that spawns and oversees sub-conversations
(parallel-delegate, agent-team, etc.).

## Current state (2026-05)

The delegate tool's status pipeline straddles three crates:

| Crate                       | Responsibility today                                              |
|-----------------------------|-------------------------------------------------------------------|
| `tau-harness`               | Runs the sub-conversation; owns `context_percent_used`, `tools_in_flight`, `tools_total`; emits `Event::ToolDelegateProgress` with a fully-rendered `ToolDisplay` (`build_delegate_progress_display`). |
| `tau-ext-core-delegate`     | Receives the final sub-conversation result text from the harness via IPC; emits the terminal `ToolResult` / `ToolError` with `display: None`. |
| `tau-cli`                   | Caches the latest `DelegateProgress.display` per call-id (`ToolCallState::delegate_last_progress`); on `ToolResult`/`ToolError` for `tool_name == "delegate"`, calls `build_delegate_completion_display` to merge the cached progress with response stats + status. |

Three places where the CLI still special-cases `tool_name == "delegate"`:

- `crates/tau-cli/src/event_renderer.rs:878` (DelegateProgress rendering)
- `crates/tau-cli/src/event_renderer.rs:900` (ToolResult merge)
- `crates/tau-cli/src/event_renderer.rs:948` (ToolError merge)

Plus the synthesis helper `build_delegate_completion_display` in
`tool_render.rs:254` is delegate-shaped (response-text → lines/bytes stats).

## Why it can't move yet

The delegate extension is a separate process. It has no visibility into
the sub-conversation's state — `context_percent_used`, `tools_in_flight`,
`tools_total`, and the eventual `ctx_window` all live in the harness's
`Conversation` struct. The extension currently only learns "done, here's
the text".

To let the extension own status reporting we need a **reusable harness↔ext
sub-conversation observation API** so the extension can:

1. Observe sub-conversation lifecycle: turn start/finish, tool dispatch,
   context-usage updates, model selection.
2. Read derived totals: `tools_in_flight`, `tools_total`, ctx percent,
   ctx window.
3. Emit `Event::ToolDelegateProgress` itself (or a more general
   "sub-conversation progress" event keyed by the extension).
4. Emit the final `ToolResult`/`ToolError` with `display` already
   populated, so the CLI never special-cases it.

This is also the right shape for **other** extensions that supervise
sub-conversations (e.g. an experimental `parallel-delegate` that fans
out N sub-tasks, or a `code-review` extension that runs a sub-agent and
post-processes the result). Today every such extension would re-derive
the same wiring.

## Design sketch

A new `tau-extension` API surface, something like:

```rust
// in tau-extension (ext-side helpers)
pub struct SubConversationHandle { /* opaque */ }

pub trait SubConversationObserver {
    fn on_turn_started(&mut self, info: TurnInfo);
    fn on_turn_finished(&mut self, info: TurnInfo);
    fn on_tool_dispatched(&mut self, info: ToolDispatchInfo);
    fn on_context_usage_changed(&mut self, percent: u8, input_tokens: u64);
    fn on_done(&mut self, outcome: SubConversationOutcome);
}

// in tau-proto (wire protocol)
pub enum Frame {
    // ...
    SubConversationEvent(SubConversationEvent),
    SubConversationCommand(SubConversationCommand),
}
```

The harness exposes a per-extension event stream scoped to a
sub-conversation that the extension started. The extension reads the
stream, decides what `ToolDelegateProgress`/`ToolResult` events to emit,
and emits them as it sees fit.

Concretely the existing harness paths that call `emit_delegate_progress`
(`process_tool_complete`, `apply_token_usage`, ...) instead would forward
those state changes to the spawning extension as `SubConversationEvent`s.
`emit_delegate_progress` and `build_delegate_progress_display` move into
`tau-ext-core-delegate`.

## Migration phases

- **P1 (small, can do now):** push the *completion display synthesis*
  from CLI into the harness. Harness already has the cached progress
  (it built it). When publishing `ToolResult`/`ToolError` for the
  delegate call, harness pre-populates `display` using a moved version
  of `build_delegate_completion_display`. CLI's three "delegate" branches
  in `event_renderer.rs` go away. Harness still owns delegate. Net: CLI
  becomes generic, harness gains a small amount of delegate-specific
  code (it already has plenty).

- **P2 (medium):** define `SubConversationEvent`/`Command` protocol +
  ext-side `SubConversationObserver` helper in `tau-extension`. Wire
  through one direction first (harness → ext), without changing who
  emits the events on the bus.

- **P3 (medium):** move `emit_delegate_progress` /
  `build_delegate_progress_display` from `tau-harness` into
  `tau-ext-core-delegate`, driven by the P2 event stream. Harness
  publishes nothing delegate-specific.

- **P4 (cleanup):** delete harness fields `parent_tool_call_id`,
  `task_name` on `Conversation` (they exist solely so the harness can
  emit DelegateProgress; once P3 lands they migrate to the ext side or
  flow through P2 events).

## Open questions

- Should the API be one-shot (extension subscribes for the lifetime of
  one sub-conversation) or persistent (extension subscribes once,
  receives events for every sub-conversation it spawns)?
- Is `tau-core::SubscriptionPolicy` rich enough to handle the per-call
  scoping, or do we need a new "sub-conversation channel" concept?
- Should `tools_in_flight`/`tools_total` be derived in `tau-extension`
  from raw `ToolDispatched`/`ToolResult` events, or pre-aggregated by
  the harness? Pre-aggregation is simpler today but ties the ext to
  whatever counters the harness happens to expose.
