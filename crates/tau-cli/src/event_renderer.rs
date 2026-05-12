//! Drains the event stream from the harness socket and paints it into
//! the terminal UI. Stateful: tracks per-prompt and per-tool-call UI
//! state so streaming updates land in the right block.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use tau_proto::{CborValue, Event};

use crate::tool_render::{
    ToolCallDisplay, build_delegate_completion_display, build_osc1337_set_user_var,
    extension_status_block, extract_diff, format_cache_hit_chip, format_context_chip,
    format_token_stats_line, format_tool_call, format_turn_metrics_chip, render_diff_tool_block,
    render_harness_info, render_shell_block, render_tool_block, render_tool_display,
    session_status_block, streaming_block, synthesize_fallback_display, system_loaded_block,
    system_status_block, ui_dir_block,
};
use crate::{build_label_parts, random_startup_pun};

pub(crate) struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    completion_data: tau_cli_term::CompletionData,
    theme: tau_themes::Theme,
    /// Per-`session_prompt_id` UI state. An entry is created on
    /// `SessionPromptCreated` (or `AgentPromptSubmitted` for prompts
    /// without an explicit creation event) and torn down on
    /// `AgentResponseFinished`. Storing the response block id, thinking
    /// block id/text, and dispatch timestamp in one place means every
    /// per-prompt cleanup is a single `prompts.remove(spid)` instead of
    /// four separate `.remove()` calls easy to forget when extending.
    prompts: HashMap<String, PromptState>,
    /// Block ID of the last user message (for moving on queue).
    last_user_block: Option<tau_cli_term::BlockId>,
    /// Queued user-message blocks (in above_sticky zone).
    /// When `SessionPromptCreated` fires for a dequeued prompt,
    /// the first entry is popped and moved back to history.
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    /// Per-`call_id` UI state. Tracks the live block (if any), the
    /// cached delegate args/progress for in-place re-renders, and
    /// whether the call belongs to a sub-agent side-conversation (in
    /// which case the UI suppresses its progress and result events).
    /// Entries are removed on `ToolResult`/`ToolError`.
    tool_calls: HashMap<String, ToolCallState>,
    /// Live user-shell blocks (from `!`/`!!`) keyed by command_id.
    /// Updated in place as progress chunks arrive, finalized on
    /// `ShellCommandFinished`.
    shell_blocks: HashMap<String, ShellBlockState>,
    /// Live extension blocks keyed by instance_id. Shown in
    /// above_active while starting, moved to history when ready.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, tau_cli_term::BlockId>,
    /// Extensions that are already up in this daemon. `/new` starts a
    /// fresh session, but these processes are intentionally kept.
    ready_extensions: HashSet<String>,
    /// Persistent status bar block showing the current model + effort.
    model_status_block: Option<tau_cli_term::BlockId>,
    /// Live history of completed write/edit blocks plus the data
    /// needed to re-render them. `/set show-diff` flips
    /// `diffs_expanded` and walks this list calling `set_block` so
    /// the entire transcript switches mode at once.
    diff_blocks: Vec<DiffBlockEntry>,
    /// Global expand-diffs toggle.
    diffs_expanded: bool,
    /// Global show-thinking toggle. When false, agent reasoning
    /// summaries are not rendered (live or in history). Controlled
    /// by `/set show-thinking`; persisted in `<state_dir>/cli.json`.
    show_thinking: bool,
    /// Persisted thinking blocks (one per finished assistant turn).
    /// When `show-thinking` flips, every entry is re-rendered as
    /// either the full text or removed, so the toggle takes effect
    /// retroactively across the visible transcript.
    thinking_history: Vec<ThinkingBlockEntry>,
    token_stats_history: Vec<TokenStatsBlockEntry>,
    /// Where to persist `show_diff` / `show_thinking` /
    /// `show_cache_stats` toggles.
    state_dirs: tau_config::settings::TauDirs,
    /// Current model id (cached so we can re-render the status bar
    /// when the effort changes, and vice versa). `None` until the
    /// first `HarnessModelSelected`, or when no model is selected.
    current_model: Option<tau_proto::ModelId>,
    /// Current effort. Mirrored into `effort_state` so the
    /// input thread can read it for Shift+Tab cycling.
    current_effort: tau_proto::Effort,
    /// Current model context usage percent. `None` when the context
    /// window is unknown for the selected model.
    current_context_percent: Option<u8>,
    /// Input tokens consumed by the most recent agent response. `None`
    /// until the first usage report for the current model.
    current_context_input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent
    /// response. `None` until the first cache-usage report.
    current_context_cached_tokens: Option<u64>,
    /// Current model context window, in tokens, if known.
    current_context_window: Option<u64>,
    /// Whether to render provider prompt-cache hit stats in the
    /// model status bar.
    show_cache_stats: bool,
    /// Whether to render per-turn token usage stats below completed
    /// agent responses.
    show_token_stats: bool,
    /// Snapshot of persisted CLI settings, kept in sync with the four
    /// `show_*` fields above by [`Self::save_cli_state`]. The input
    /// loop captures this handle in the `/set` name-completion
    /// closure so the menu can show each setting's current value
    /// without snooping on renderer-thread fields directly.
    cli_state_mirror: std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>>,
    /// End-to-end latency of the most recently completed prompt.
    last_turn_latency: Option<Duration>,
    /// Shared effort mirror for the input thread.
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Shared set of currently-available effort levels, mirrored
    /// from `HarnessEffortsAvailable`. The input thread's Shift+Tab
    /// cycle reads it to skip levels the current model doesn't
    /// support (e.g. `xhigh` on `gpt-5.4-mini`).
    efforts_available:
        std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<tau_proto::Effort>>>,
    /// Context appended to files opened by the external prompt editor.
    /// Locked with `if let Ok(...)` rather than [`crate::locked`] because
    /// this is best-effort UI metadata: if another holder panicked we'd
    /// rather drop one editor-context update than crash the renderer
    /// thread.
    editor_context: std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>>,
    /// Symbol shown before submitted prompts in the transcript.
    submitted_prompt_symbol: String,
}

/// One completed file-mutation tool block. Held so `/set show-diff` can
/// re-render every diff in the chat history when the global
/// expand toggle flips.
struct DiffBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
    diff: tau_proto::DiffSummary,
}

/// One finished thinking block. Held so `/set show-thinking` can swap
/// its content between the original reasoning text (visible) and
/// empty content (hidden) without losing the block's position in
/// the transcript.
struct ThinkingBlockEntry {
    block_id: tau_cli_term::BlockId,
    text: String,
}

struct TokenStatsBlockEntry {
    block_id: tau_cli_term::BlockId,
    usage: tau_proto::AgentTokenUsage,
}

/// Per-prompt UI state held by [`EventRenderer`]. Lives from the first
/// event observed for the prompt (`SessionPromptCreated` or
/// `AgentPromptSubmitted`) through `AgentResponseFinished`.
#[derive(Default)]
struct PromptState {
    /// Live agent-response block. `None` until `SessionPromptCreated`
    /// allocates it (some prompts arrive without a creation event).
    response_block_id: Option<tau_cli_term::BlockId>,
    /// Live thinking block. Lazy-created the first time the agent emits
    /// non-empty `thinking`, so backends that don't return reasoning
    /// summaries produce no extra block.
    thinking_block_id: Option<tau_cli_term::BlockId>,
    /// Latest captured thinking text. Held so `AgentResponseFinished`
    /// can render it into history even when the finish event doesn't
    /// carry its own `thinking` payload.
    thinking_text: Option<String>,
    /// Dispatch timestamp, used to compute end-to-end latency on
    /// `AgentResponseFinished`.
    started_at: Option<Instant>,
}

/// Per-tool-call UI state held by [`EventRenderer`]. Created when the
/// agent's `AgentResponseFinished` enumerates the call (or when a
/// sub-agent's finish marks the call as suppressed) and torn down on
/// `ToolResult`/`ToolError`.
#[derive(Default)]
struct ToolCallState {
    /// Live tool-call block. `None` for sub-agent tool calls whose UI
    /// is suppressed (their progress is rolled up into the parent
    /// `delegate` block via `DelegateProgress` instead).
    block_id: Option<tau_cli_term::BlockId>,
    /// Most recent `DelegateProgress` snapshot. On `ToolResult` we
    /// render the completion line with the final `ctx: …` / `tools: …`
    /// chips so the user sees the delegation cost alongside the
    /// response stats.
    delegate_last_progress: Option<tau_proto::DelegateProgress>,
    /// `true` for tool calls in side conversations. Their lifecycle
    /// events (`ToolResult`, `ToolError`, `ToolProgress`) share the bus
    /// with the main agent's, but the UI filters them out.
    is_sub_agent: bool,
}

/// In-flight state for a user `!`/`!!` shell block.
struct ShellBlockState {
    block_id: tau_cli_term::BlockId,
    command: String,
    include_in_context: bool,
    /// Output accumulated from `ShellCommandProgress` chunks. Rendered
    /// under the header each redraw.
    output: String,
}

/// Returns the originator of any prompt-lifecycle event, or
/// [`tau_proto::PromptOriginator::User`] for events that don't carry
/// one (so unrelated events render as before).
fn originator_of(event: &Event) -> tau_proto::PromptOriginator {
    match event {
        Event::UiPromptSubmitted(p) => p.originator.clone(),
        Event::SessionPromptCreated(p) => p.originator.clone(),
        Event::AgentPromptSubmitted(s) => s.originator.clone(),
        Event::AgentResponseUpdated(u) => u.originator.clone(),
        Event::AgentResponseFinished(f) => f.originator.clone(),
        _ => tau_proto::PromptOriginator::User,
    }
}

impl EventRenderer {
    #[cfg(test)]
    pub(crate) fn new(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
    ) -> Self {
        // Tests pass a state_dir of None so toggles never touch the
        // user's real `~/.local/state/tau/cli.json`.
        Self::new_with_state(
            handle,
            completion_data,
            theme,
            tau_config::settings::CliState::default(),
            tau_config::settings::TauDirs {
                config_dir: None,
                state_dir: None,
            },
            ">".to_string(),
        )
    }

    pub(crate) fn new_with_state(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
        state: tau_config::settings::CliState,
        state_dirs: tau_config::settings::TauDirs,
        submitted_prompt_symbol: String,
    ) -> Self {
        let cli_state_mirror = std::sync::Arc::new(std::sync::Mutex::new(state.clone()));
        Self {
            handle,
            completion_data,
            theme,
            prompts: HashMap::new(),
            last_user_block: None,
            queued_user_blocks: VecDeque::new(),
            tool_calls: HashMap::new(),
            shell_blocks: HashMap::new(),
            extension_blocks: HashMap::new(),
            ready_extensions: HashSet::new(),
            model_status_block: None,
            diff_blocks: Vec::new(),
            diffs_expanded: state.show_diff,
            show_thinking: state.show_thinking,
            show_cache_stats: state.show_cache_stats,
            show_token_stats: state.show_token_stats,
            cli_state_mirror,
            thinking_history: Vec::new(),
            token_stats_history: Vec::new(),
            state_dirs,
            current_model: None,
            current_effort: tau_proto::Effort::Off,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_cached_tokens: None,
            current_context_window: None,
            last_turn_latency: None,
            effort_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::Effort::Off.as_u8(),
            )),
            // Empty until the first `HarnessEffortsAvailable`
            // arrives. The input loop's BackTab handler treats an
            // empty set as "no allowed levels known yet" and
            // skips sending a request the harness would just clamp.
            efforts_available: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::BTreeSet::new(),
            )),
            editor_context: std::sync::Arc::new(std::sync::Mutex::new(
                tau_cli_term::EditorContext::default(),
            )),
            submitted_prompt_symbol,
        }
    }

    fn save_cli_state(&self) {
        let state = tau_config::settings::CliState {
            show_diff: self.diffs_expanded,
            show_thinking: self.show_thinking,
            show_cache_stats: self.show_cache_stats,
            show_token_stats: self.show_token_stats,
        };
        if let Ok(mut mirror) = self.cli_state_mirror.lock() {
            *mirror = state.clone();
        }
        state.save(&self.state_dirs);
    }

    /// Shared snapshot of the persisted CLI settings, updated in sync
    /// with every successful `/set` (i.e. on every
    /// [`Self::save_cli_state`] call). Cloned by the input loop so the
    /// `/set` name-completion menu can show each setting's current
    /// value without touching renderer-thread fields directly.
    pub(crate) fn cli_state_mirror(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>> {
        self.cli_state_mirror.clone()
    }

    pub(crate) fn editor_context(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>> {
        self.editor_context.clone()
    }

    /// Returns a clone of the shared effort mirror, used by the
    /// input thread to read the current level for Shift+Tab cycling.
    pub(crate) fn effort_state(&self) -> std::sync::Arc<std::sync::atomic::AtomicU8> {
        self.effort_state.clone()
    }

    /// Returns a clone of the shared available-efforts set. The
    /// input thread uses it to skip levels the current model
    /// doesn't expose (e.g. `xhigh` on `gpt-5.4-mini`).
    pub(crate) fn efforts_available(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<tau_proto::Effort>>> {
        self.efforts_available.clone()
    }

    /// Apply a `/set <name> <value>` change. The caller (input loop)
    /// has already validated `name` and `value` against the
    /// [`crate::settings_registry`] table.
    pub(crate) fn apply_setting(&mut self, name: &str, value: &str) {
        let on = value == "true";
        match name {
            "show-diff" => self.set_diffs_expanded(on),
            "show-thinking" => self.set_show_thinking(on),
            "show-cache-stats" => self.set_show_cache_stats(on),
            "show-token-stats" => self.set_show_token_stats(on),
            _ => {}
        }
    }

    /// Set the global expand-diffs flag and re-render every diff
    /// block in the chat history so the entire transcript switches
    /// mode at once. No-op if already in the requested state.
    fn set_diffs_expanded(&mut self, on: bool) {
        if self.diffs_expanded == on {
            return;
        }
        self.diffs_expanded = on;
        for entry in &self.diff_blocks {
            let block = render_diff_tool_block(
                &self.theme,
                &entry.display,
                &entry.diff,
                self.diffs_expanded,
            );
            self.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Set the global show-thinking flag and re-render every prior
    /// thinking block in the transcript so the change takes effect
    /// retroactively (full text when on, empty content when off).
    /// Live in-flight thinking blocks are also flipped. New turns
    /// continue to be gated by the same flag.
    ///
    /// Empty content is used instead of `remove_block` so the
    /// block's position in the transcript is preserved; turning
    /// back on restores the original reasoning text in place.
    fn set_show_thinking(&mut self, on: bool) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        if self.show_thinking == on {
            return;
        }
        self.show_thinking = on;
        for entry in &self.thinking_history {
            let display = if self.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.handle.set_block(
                entry.block_id,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        for state in self.prompts.values() {
            let Some(bid) = state.thinking_block_id else {
                continue;
            };
            let display = if self.show_thinking {
                state.thinking_text.clone().unwrap_or_default()
            } else {
                String::new()
            };
            self.handle.set_block(
                bid,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Force a full repaint after a `/set show-*` change. Edited blocks
    /// from earlier in the transcript may already have scrolled out of
    /// the visible window, so the renderer needs to redraw from scratch
    /// for the change to take effect retroactively across scrollback.
    fn invalidate_for_retroactive_toggle(&mut self) {
        self.handle.invalidate_screen();
    }

    /// Set provider prompt-cache hit stats visibility in the status bar.
    fn set_show_cache_stats(&mut self, on: bool) {
        if self.show_cache_stats == on {
            return;
        }
        self.show_cache_stats = on;
        self.render_model_status();
        self.save_cli_state();
    }

    fn set_show_token_stats(&mut self, on: bool) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        if self.show_token_stats == on {
            return;
        }
        self.show_token_stats = on;
        for entry in &self.token_stats_history {
            let text = if self.show_token_stats {
                format_token_stats_line(&entry.usage)
            } else {
                String::new()
            };
            self.handle.set_block(
                entry.block_id,
                themed_block(&self.theme, names::SYSTEM_INFO, text),
            );
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Clears all session-scoped UI state and re-renders an empty
    /// transcript. Persistent user preferences such as `show-diff`
    /// and `show-thinking` are intentionally preserved.
    fn clear_for_new_session(&mut self) {
        self.prompts.clear();
        self.last_user_block = None;
        self.queued_user_blocks.clear();
        self.tool_calls.clear();
        self.shell_blocks.clear();
        self.extension_blocks.clear();
        self.model_status_block = None;
        self.diff_blocks.clear();
        self.thinking_history.clear();
        self.token_stats_history.clear();
        // Model selection and effort are harness-global, not
        // session-scoped. `/new` only causes a SessionStarted event;
        // the harness does not re-emit HarnessModelSelected for the
        // unchanged model. Keep the cached selection so the status bar
        // can be recreated after clearing the terminal output.
        self.current_context_percent = None;
        self.current_context_input_tokens = None;
        self.current_context_cached_tokens = None;
        self.last_turn_latency = None;
        self.handle.clear_output();
        self.render_session_preamble();
        if self.current_model.is_some() {
            self.render_model_status();
        }
    }

    fn render_session_preamble(&mut self) {
        use tau_cli_term::{StyledBlock, StyledText};
        use tau_themes::names;

        let logo = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_LOGO);
        let name = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_NAME);
        let version_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_VERSION);
        let build_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_BUILD);
        let pun_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_PUN);
        let pun = random_startup_pun();
        let (version, build) = build_label_parts();
        let banner = StyledText::from(vec![
            tau_cli_term::Span::new("▀█▀▀ ", logo),
            tau_cli_term::Span::new("tau", name),
            tau_cli_term::Span::new(version.trim_start_matches("tau"), version_style),
            tau_cli_term::Span::new(" ", Default::default()),
            tau_cli_term::Span::new(build, build_style),
            tau_cli_term::Span::new("\n", Default::default()),
            tau_cli_term::Span::new(" █▄▖ ", logo),
            tau_cli_term::Span::new(pun, pun_style),
        ]);
        self.handle.print_output(StyledBlock::new(banner));
        let mut extensions: Vec<_> = self.ready_extensions.iter().collect();
        extensions.sort();
        for extension_name in extensions {
            self.handle
                .print_output(extension_status_block(&self.theme, extension_name, "kept"));
        }
    }

    fn render_model_status(&mut self) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        let label = match self.current_model.as_ref() {
            None => "no model selected".to_string(),
            Some(model) => {
                let level = if matches!(self.current_effort, tau_proto::Effort::Off) {
                    "none".to_owned()
                } else {
                    self.current_effort.to_string()
                };
                let context = format_context_chip(
                    self.current_context_input_tokens,
                    self.current_context_percent,
                    self.current_context_window,
                );
                let cache = if self.show_cache_stats {
                    format_cache_hit_chip(
                        self.current_context_input_tokens,
                        self.current_context_cached_tokens,
                    )
                } else {
                    String::new()
                };
                let turn_metrics = format_turn_metrics_chip(self.last_turn_latency);
                format!("{model} ({level}){context}{cache}{turn_metrics}")
            }
        };
        let block = themed_block(&self.theme, names::MODEL_STATUS, label);
        match self.model_status_block {
            Some(bid) => {
                self.handle.set_block(bid, block);
            }
            None => {
                let bid = self.handle.new_block(block);
                self.handle.push_below(bid);
                self.model_status_block = Some(bid);
            }
        }
        self.handle.redraw();
    }

    fn submitted_prompt_prefix(&self) -> String {
        format!("{} ", self.submitted_prompt_symbol)
    }

    pub(crate) fn handle_disconnect(&mut self, reason: Option<String>) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        let reason = reason.as_deref().unwrap_or("disconnected");
        self.handle
            .print_output(themed_block(&self.theme, names::SYSTEM_DISCONNECT, reason));
    }

    pub(crate) fn handle(&mut self, event: &Event) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        // Side-conversation `AgentResponseFinished` events get filtered
        // out by `originator_of(event).is_user()` below — but we still
        // need to learn which `call_id`s those side conversations
        // emit, so we can suppress the matching `ToolResult` /
        // `ToolError` / `ToolProgress` (which carry no originator) on
        // their way past. Otherwise sub-agent tool activity would
        // leak into the user's transcript.
        if let Event::AgentResponseFinished(finished) = event
            && !finished.originator.is_user()
        {
            for call in &finished.tool_calls {
                self.tool_calls.insert(
                    call.id.to_string(),
                    ToolCallState {
                        is_sub_agent: true,
                        ..ToolCallState::default()
                    },
                );
            }
        }

        // Skip events that belong to a side conversation spawned by an
        // extension (e.g. the std-notifications idle-summarizer). They
        // travel on the same bus as the user's interactive turn but
        // must not paint into the user's chat window or perturb its
        // pending-block bookkeeping.
        if !originator_of(event).is_user() {
            return;
        }

        match event {
            Event::SessionStarted(started)
                if matches!(started.reason, tau_proto::SessionStartReason::New) =>
            {
                self.clear_for_new_session();
            }
            Event::UiPromptSubmitted(prompt) => {
                let block = themed_block(
                    &self.theme,
                    names::USER_PROMPT,
                    format!("{}{}", self.submitted_prompt_prefix(), prompt.text),
                );
                let id = self.handle.print_output(block);
                self.last_user_block = Some(id);
            }
            Event::SessionPromptQueued(queued) => {
                if let Some(id) = self.last_user_block.take() {
                    self.handle.remove_block(id);
                    let block = themed_block(
                        &self.theme,
                        names::USER_PROMPT_QUEUED,
                        format!("{}{} (queued)", self.submitted_prompt_prefix(), queued.text),
                    );
                    let queued_id = self.handle.new_block(block);
                    self.handle.push_above_sticky(queued_id);
                    self.handle.redraw();
                    self.queued_user_blocks
                        .push_back((queued_id, queued.text.clone()));
                }
            }
            Event::SessionPromptSteered(steered) => {
                // The harness folded a queued prompt into the current
                // turn's next round (alongside tool results) instead of
                // waiting for `Idle`. Promote the "(queued)" rendering
                // to a regular user prompt so the transcript reads
                // naturally above the agent's continuing response.
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{text}", self.submitted_prompt_prefix()),
                    ));
                    self.handle.redraw();
                } else {
                    // No matching "(queued)" block — fall back to
                    // rendering the steered text directly so the user
                    // still sees their message land.
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{}", self.submitted_prompt_prefix(), steered.text),
                    ));
                    self.handle.redraw();
                }
            }
            Event::SessionPromptCreated(prompt) => {
                if prompt.originator.is_user()
                    && let Ok(mut context) = self.editor_context.lock()
                {
                    context.active_prompt = None;
                }
                let entry = self
                    .prompts
                    .entry(prompt.session_prompt_id.to_string())
                    .or_default();
                entry.started_at = Some(Instant::now());
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{text}", self.submitted_prompt_prefix()),
                    ));
                }

                let block = streaming_block(&self.theme, names::AGENT_PENDING, "");
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.prompts
                    .entry(prompt.session_prompt_id.to_string())
                    .or_default()
                    .response_block_id = Some(id);
            }
            Event::AgentPromptSubmitted(submitted) => {
                self.prompts
                    .entry(submitted.session_prompt_id.to_string())
                    .or_default()
                    .started_at = Some(Instant::now());
            }
            Event::AgentResponseUpdated(update) => {
                let spid = update.session_prompt_id.as_str();

                if update.originator.is_user()
                    && let Ok(mut context) = self.editor_context.lock()
                {
                    context.active_prompt = if update.text.is_empty() {
                        None
                    } else {
                        Some(update.text.clone())
                    };
                }

                // Thinking is its own block, lazy-created the first
                // time non-empty summary content arrives. Rendered
                // above the response block (in `above_active`). Always
                // accumulate the text so the toggle can flip on
                // retroactively, but only paint the live block when
                // `show_thinking` is on.
                if let Some(thinking) = update.thinking.as_deref()
                    && !thinking.is_empty()
                {
                    self.prompts
                        .entry(spid.to_owned())
                        .or_default()
                        .thinking_text = Some(thinking.to_owned());
                    if self.show_thinking {
                        let block = streaming_block(&self.theme, names::AGENT_THINKING, thinking);
                        let existing_tbid =
                            self.prompts.get(spid).and_then(|s| s.thinking_block_id);
                        if let Some(tbid) = existing_tbid {
                            self.handle.set_block(tbid, block);
                        } else {
                            // Insert the thinking block ABOVE the
                            // pending response block in `above_active`.
                            // The response block was pushed first
                            // (in SessionPromptCreated), so a plain
                            // push would land below it. Briefly
                            // remove the response, push thinking,
                            // re-push response — net effect: thinking
                            // is at the response's old position and
                            // the response moves down by one.
                            let tbid = self.handle.new_block(block);
                            let response_bid =
                                self.prompts.get(spid).and_then(|s| s.response_block_id);
                            if let Some(response_bid) = response_bid {
                                self.handle.remove_above_active(response_bid);
                                self.handle.push_above_active(tbid);
                                self.handle.push_above_active(response_bid);
                            } else {
                                self.handle.push_above_active(tbid);
                            }
                            self.prompts
                                .entry(spid.to_owned())
                                .or_default()
                                .thinking_block_id = Some(tbid);
                        }
                        self.handle.redraw();
                    }
                }

                if let Some(bid) = self.prompts.get(spid).and_then(|s| s.response_block_id) {
                    let block =
                        streaming_block(&self.theme, names::AGENT_RESPONSE, update.text.clone());
                    self.handle.set_block(bid, block);
                    self.handle.redraw();
                }
            }
            Event::AgentResponseFinished(finished) => {
                let spid = finished.session_prompt_id.as_str();
                // Drain the whole per-prompt state in one shot — every
                // field tracked through the stream is consumed here.
                let prompt_state = self.prompts.remove(spid).unwrap_or_default();
                self.last_turn_latency = prompt_state
                    .started_at
                    .map(|started_at| started_at.elapsed());

                // Finalize the thinking block above the response.
                // Prefer the finished event's payload if it carries
                // one; fall back to whatever streaming captured.
                let thinking = finished.thinking.clone().or(prompt_state.thinking_text);
                if let Some(tbid) = prompt_state.thinking_block_id {
                    self.handle.remove_block(tbid);
                }
                if self.show_thinking
                    && let Some(thinking) = thinking.filter(|t| !t.is_empty())
                {
                    let bid = self.handle.print_output(themed_block(
                        &self.theme,
                        names::AGENT_THINKING,
                        thinking.clone(),
                    ));
                    self.thinking_history.push(ThinkingBlockEntry {
                        block_id: bid,
                        text: thinking,
                    });
                }

                if let Some(bid) = prompt_state.response_block_id {
                    self.handle.remove_block(bid);
                }

                let text = finished.text.as_deref().unwrap_or("");
                if !text.is_empty() {
                    if finished.originator.is_user()
                        && let Ok(mut context) = self.editor_context.lock()
                    {
                        context.last_agent_response = Some(text.to_owned());
                        context.active_prompt = None;
                    }
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::AGENT_RESPONSE,
                        text,
                    ));
                }
                if let Some(usage) = finished.token_usage.clone() {
                    let display = if self.show_token_stats {
                        format_token_stats_line(&usage)
                    } else {
                        String::new()
                    };
                    let bid = self.handle.print_output(themed_block(
                        &self.theme,
                        names::SYSTEM_INFO,
                        display,
                    ));
                    self.token_stats_history.push(TokenStatsBlockEntry {
                        block_id: bid,
                        usage,
                    });
                }

                // Only the main agent's tool calls land in the UI as
                // their own blocks. Sub-agent (side conversation) tool
                // activity is summarized live under the parent's
                // `delegate` block via `DelegateProgress` instead, so
                // the user sees one line per delegation rather than
                // a flood of nested invocations.
                if finished.originator.is_user() {
                    for call in &finished.tool_calls {
                        let display = format_tool_call(call.name.as_str(), call.display.as_ref());
                        let block = render_tool_block(&self.theme, &display);
                        let id = self.handle.new_block(block);
                        self.handle.push_above_active(id);
                        self.tool_calls.insert(
                            call.id.to_string(),
                            ToolCallState {
                                block_id: Some(id),
                                ..ToolCallState::default()
                            },
                        );
                    }
                    if !finished.tool_calls.is_empty() {
                        self.handle.redraw();
                    }
                }
                self.render_model_status();
            }
            Event::ToolProgress(progress) => {
                let state = self.tool_calls.get(progress.call_id.as_str());
                if state.is_some_and(|s| s.is_sub_agent) {
                    return;
                }
                if state.is_none_or(|s| s.block_id.is_none()) {
                    let text = tau_harness::format_tool_progress(progress);
                    self.handle
                        .print_output(themed_block(&self.theme, names::SHELL_OUTPUT, text));
                }
            }
            Event::ToolDelegateProgress(progress) => {
                let call_id = progress.call_id.as_str();
                // Snapshot the latest counters and ctx info regardless
                // of whether the block is still live; the `ToolResult`
                // handler reuses them on the completion line.
                let state = self.tool_calls.entry(call_id.to_owned()).or_default();
                state.delegate_last_progress = Some(progress.clone());
                let Some(bid) = state.block_id else {
                    // Block already torn down (delegate finished or
                    // never rendered) — nothing to update.
                    return;
                };
                let display = match &progress.display {
                    Some(descriptor) => render_tool_display("delegate", descriptor),
                    None => render_tool_display(
                        "delegate",
                        &synthesize_fallback_display("delegate", None),
                    ),
                };
                let block = render_tool_block(&self.theme, &display);
                self.handle.set_block(bid, block);
            }
            Event::ToolResult(result) => {
                let call_id = result.call_id.as_str();
                // Sub-agent tool activity stays out of the user's
                // transcript — its progress is rolled up under the
                // parent's `delegate` block by `DelegateProgress`.
                let prior = self.tool_calls.remove(call_id).unwrap_or_default();
                if prior.is_sub_agent {
                    return;
                }
                if let Some(bid) = prior.block_id {
                    self.handle.remove_block(bid);
                }
                let last_progress = prior.delegate_last_progress;
                let display = if result.tool_name.as_str() == "delegate" {
                    let descriptor = build_delegate_completion_display(
                        last_progress.as_ref().and_then(|p| p.display.as_ref()),
                        &result.result,
                        None,
                    );
                    render_tool_display("delegate", &descriptor)
                } else if let Some(descriptor) = &result.display {
                    render_tool_display(&result.tool_name, descriptor)
                } else {
                    render_tool_display(
                        &result.tool_name,
                        &synthesize_fallback_display(&result.tool_name, None),
                    )
                };
                let diff = result
                    .display
                    .as_ref()
                    .and_then(|d| match &d.payload {
                        Some(tau_proto::ToolDisplayPayload::Diff(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .or_else(|| extract_diff(&result.result));
                if let Some(diff) = diff {
                    let block =
                        render_diff_tool_block(&self.theme, &display, &diff, self.diffs_expanded);
                    let bid = self.handle.print_output(block);
                    self.diff_blocks.push(DiffBlockEntry {
                        block_id: bid,
                        display,
                        diff,
                    });
                } else {
                    self.handle
                        .print_output(render_tool_block(&self.theme, &display));
                }
            }
            Event::ToolError(error) => {
                let call_id = error.call_id.as_str();
                let prior = self.tool_calls.remove(call_id).unwrap_or_default();
                if prior.is_sub_agent {
                    return;
                }
                if let Some(bid) = prior.block_id {
                    self.handle.remove_block(bid);
                }
                let last_progress = prior.delegate_last_progress;
                let cbor = error.details.as_ref();
                let display = if error.tool_name.as_str() == "delegate" {
                    let descriptor = build_delegate_completion_display(
                        last_progress.as_ref().and_then(|p| p.display.as_ref()),
                        cbor.unwrap_or(&CborValue::Null),
                        Some(&error.message),
                    );
                    render_tool_display("delegate", &descriptor)
                } else if let Some(descriptor) = &error.display {
                    render_tool_display(&error.tool_name, descriptor)
                } else {
                    render_tool_display(
                        &error.tool_name,
                        &synthesize_fallback_display(&error.tool_name, Some(&error.message)),
                    )
                };
                self.handle
                    .print_output(render_tool_block(&self.theme, &display));
            }
            Event::UiShellCommand(cmd) => {
                // Create a running block now; the harness will echo
                // progress and a finished event back to us via the
                // bus. Both bangs render the same; the context bit
                // just labels the suffix.
                let label = if cmd.include_in_context {
                    "running".to_owned()
                } else {
                    "running [no context]".to_owned()
                };
                let block = render_shell_block(&self.theme, &cmd.command, "", Some(&label));
                let block_id = self.handle.new_block(block);
                self.handle.push_above_active(block_id);
                self.handle.redraw();
                self.shell_blocks.insert(
                    cmd.command_id.to_string(),
                    ShellBlockState {
                        block_id,
                        command: cmd.command.clone(),
                        include_in_context: cmd.include_in_context,
                        output: String::new(),
                    },
                );
            }
            Event::ShellCommandProgress(progress) => {
                if let Some(state) = self.shell_blocks.get_mut(progress.command_id.as_str()) {
                    state.output.push_str(&progress.chunk);
                    let label = if state.include_in_context {
                        "running".to_owned()
                    } else {
                        "running [no context]".to_owned()
                    };
                    let block = render_shell_block(
                        &self.theme,
                        &state.command,
                        &state.output,
                        Some(&label),
                    );
                    self.handle.set_block(state.block_id, block);
                    self.handle.redraw();
                }
            }
            Event::ShellCommandFinished(finished) => {
                let Some(state) = self.shell_blocks.remove(finished.command_id.as_str()) else {
                    return;
                };
                // Use the final, post-truncation output from the
                // extension rather than our streaming buffer so the
                // UI matches what the harness injected into context.
                self.handle.remove_block(state.block_id);
                let suffix = if finished.cancelled {
                    "cancelled".to_owned()
                } else {
                    match finished.exit_code {
                        Some(0) => "[0]".to_owned(),
                        Some(code) => format!("[{code}]"),
                        None => "[?]".to_owned(),
                    }
                };
                let suffix = if state.include_in_context {
                    suffix
                } else {
                    format!("{suffix} [no context]")
                };
                let block = render_shell_block(
                    &self.theme,
                    &finished.command,
                    &finished.output,
                    Some(&suffix),
                );
                self.handle.print_output(block);
            }
            Event::ExtensionStarting(starting) => {
                let block =
                    extension_status_block(&self.theme, &starting.extension_name, "starting");
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.extension_blocks.insert(starting.instance_id, id);
            }
            Event::ExtensionReady(ready) => {
                if let Some(bid) = self.extension_blocks.remove(&ready.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.ready_extensions
                    .insert(ready.extension_name.to_string());
                self.handle.print_output(extension_status_block(
                    &self.theme,
                    &ready.extension_name,
                    "ready",
                ));
            }
            Event::ExtensionExited(exited) => {
                if let Some(bid) = self.extension_blocks.remove(&exited.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.ready_extensions.remove(exited.extension_name.as_str());
                self.handle.print_output(extension_status_block(
                    &self.theme,
                    &exited.extension_name,
                    "exited",
                ));
            }
            Event::ExtAgentsMdAvailable(agents) => {
                self.handle.print_output(system_loaded_block(
                    &self.theme,
                    &agents.file_path,
                    &agents.content,
                ));
            }
            Event::ExtensionContextReady(_) => {
                self.handle.print_output(system_status_block(
                    &self.theme,
                    "session context ",
                    "ready",
                ));
            }
            Event::HarnessInfo(info) => {
                self.handle
                    .print_output(render_harness_info(&self.theme, info));
            }
            Event::HarnessSessionDir(session_dir) => {
                self.handle.print_output(session_status_block(
                    &self.theme,
                    &session_dir.path,
                    "/",
                    session_dir.status.as_str(),
                ));
            }
            Event::HarnessUiDir(ui_dir) => {
                self.handle
                    .print_output(ui_dir_block(&self.theme, &ui_dir.path));
            }
            Event::HarnessModelsAvailable(models) => {
                let items: Vec<tau_cli_term::CompletionItem> = models
                    .models
                    .iter()
                    .map(|m| tau_cli_term::CompletionItem::plain(m.to_string()))
                    .collect();
                self.completion_data
                    .set_arg_completions(tau_cli_term::CommandName::new("/model"), items);
            }
            Event::HarnessModelSelected(selected) => {
                self.current_model = selected.model.clone();
                self.current_context_window = selected.context_window;
                self.last_turn_latency = None;
                self.render_model_status();
            }
            Event::HarnessContextUsageChanged(changed) => {
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_cached_tokens = changed.cached_tokens;
                self.current_context_percent = changed.percent_used;
                self.render_model_status();
            }
            Event::HarnessEffortChanged(changed) => {
                self.current_effort = changed.level;
                self.effort_state
                    .store(changed.level.as_u8(), std::sync::atomic::Ordering::Relaxed);
                self.render_model_status();
            }
            Event::Osc1337SetUserVar(req) => {
                let in_tmux = std::env::var_os("TMUX").is_some();
                let seq = build_osc1337_set_user_var(&req.name, &req.value, in_tmux);
                self.handle.print_terminal_escape(seq);
            }
            Event::HarnessEffortsAvailable(avail) => {
                let items: Vec<tau_cli_term::CompletionItem> = avail
                    .levels
                    .iter()
                    .map(|l| tau_cli_term::CompletionItem::plain(l.as_str()))
                    .collect();
                self.completion_data
                    .set_arg_completions(tau_cli_term::CommandName::new("/effort"), items);
                if let Ok(mut set) = self.efforts_available.lock() {
                    set.clear();
                    set.extend(avail.levels.iter().copied());
                }
            }
            other => {
                tracing::trace!(
                    target: "tau_cli::ui",
                    event = ?std::mem::discriminant(other),
                    "unhandled event variant"
                );
            }
        }
    }
}
