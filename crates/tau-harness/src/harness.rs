//! [`Harness`]: the central event loop. Owns the bus, registry, session
//! store, and the live extensions; routes every event between the agent,
//! tools, and clients.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tau_config::Config;
use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, DefaultSubscriptionPolicy, EventBus,
    EventLog, PolicyStore, RouteError, SessionStore, ToolRegistry, ToolRouteError,
};
use tau_proto::{
    AgentResponseFinished, AgentToolCall, CborValue, ClientKind, Event, EventSelector,
    HarnessContextUsageChanged, HarnessModelSelected, HarnessModelsAvailable, LifecycleDisconnect,
    ModelId, SessionId, SessionPromptCreated, SessionPromptId, SessionPromptQueued, ToolCallId,
    ToolDefinition, ToolError, ToolName, ToolRegister, ToolRequest,
};

use crate::conversation::{Conversation, ConversationId};
use crate::daemon::InteractionOutcome;
use crate::debug_log::DebugEventLog;
use crate::dirs::policy_store_path_from;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
use crate::error::HarnessError;
use crate::event::{
    ChannelSink, HarnessEvent, WriterShutdown, spawn_reader_thread, spawn_writer_thread,
};
use crate::extension::{
    ExtensionEntry, ExtensionState, extension_stderr_log_path, spawn_in_process, spawn_supervised,
};
use crate::format::{format_tool_progress, render_entry_preview};
use crate::model::{
    clamp_effort, context_percent_used, efforts_for_model, load_model_list, model_context_window,
    save_harness_state, selected_effort_for_model,
};
use crate::prompt::{
    assemble_conversation, build_system_prompt, cbor_map_bool, cbor_map_text,
    render_agents_context_message,
};
use crate::settings::load_harness_settings_or_warn;
use crate::turn::{PromptSubmission, TurnState};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests;

/// Connection ID used for harness-owned tools (e.g. the `skill` tool).
const HARNESS_CONNECTION_ID: &str = "__harness__";

pub(crate) struct Harness {
    /// Sender side of the harness's central event channel. Cloned into
    /// each per-connection reader thread so they can feed
    /// `HarnessEvent`s back into the main loop.
    pub(crate) tx: Sender<HarnessEvent>,
    /// Receiver side of the central event channel. The main loop
    /// blocks on this and dispatches one `HarnessEvent` at a time.
    pub(crate) rx: Receiver<HarnessEvent>,
    /// Routes protocol events between connections (agent ↔ extensions
    /// ↔ socket clients). Owns connection state and per-connection
    /// outgoing queues.
    pub(crate) bus: EventBus,
    /// Maps tool name → providing connection. Used to route an
    /// outgoing `ToolRequest` to the extension that registered the
    /// tool.
    pub(crate) registry: ToolRegistry,
    /// Append-only on-disk session store. Owns one `SessionTree` per
    /// session id, derived by folding the durable per-session event log
    /// at `<state_dir>/<session_id>/events.cbor`. The tree is never
    /// mutated through any other path — every entry comes from a
    /// persisted protocol event, so the on-disk log and the in-memory
    /// view cannot drift.
    pub(crate) store: SessionStore,
    /// The single session this harness owns. UserMessages with a
    /// different `session_id` are rejected. Pi-style: one harness =
    /// one active session at a time. Switching sessions tears the
    /// harness down and respawns extensions; that's a future
    /// `switch_session` operation, not silent multi-session.
    pub(crate) current_session_id: SessionId,
    /// FIFO of session ids for tool calls the agent has just emitted
    /// but for which we haven't yet seen the corresponding outgoing
    /// `ToolRequest`. The agent's `ToolUse` event doesn't carry a
    /// session id, so we tag the next request popped from this queue
    /// with the recorded session.
    pub(crate) pending_request_sessions: VecDeque<SessionId>,
    /// `call_id` → owning conversation for every tool call currently
    /// in flight. Read by `session_id_for_event` (via the
    /// conversation) to attribute incoming `ToolResult` / `ToolError`
    /// / `ToolProgress` events back to the originating session.
    pub(crate) tool_conversations: std::collections::HashMap<ToolCallId, ConversationId>,
    /// `call_id` → tool name for in-flight calls. Used for lifecycle
    /// messages and debug formatting where the result event itself
    /// only carries the id.
    pub(crate) pending_tool_names: std::collections::HashMap<ToolCallId, ToolName>,
    /// `call_id` → connection id of the extension currently servicing
    /// the call. Needed to route cancellation requests back to the
    /// right provider.
    pub(crate) pending_tool_providers:
        std::collections::HashMap<ToolCallId, tau_proto::ConnectionId>,
    /// Append-only ring of recent protocol events. Client follower
    /// threads tail this log on connect to replay state and stay live.
    pub(crate) event_log: std::sync::Arc<EventLog>,
    /// Writer channels for socket clients, keyed by connection ID.
    /// Used to start follower threads for log-based replay + delivery.
    pub(crate) client_writers: std::collections::HashMap<tau_proto::ConnectionId, Sender<Event>>,
    /// Buffered human-readable lifecycle messages (extension init,
    /// model changes, etc.) surfaced to the UI as part of the next
    /// `InteractionOutcome`.
    pub(crate) lifecycle_messages: Vec<String>,
    /// Every spawned or in-process extension. Indexed by position;
    /// supervises restart, shutdown, and per-extension ack state.
    pub(crate) extensions: Vec<ExtensionEntry>,
    /// Connection id assigned to the agent extension. Other code paths
    /// branch on this to special-case agent traffic (e.g. tool-call
    /// emission, session prompt routing).
    pub(crate) agent_connection_id: tau_proto::ConnectionId,
    /// Monotonic source for `ExtensionInstanceId`s, bumped as
    /// extensions are constructed. Underscore-prefixed because nothing
    /// reads it after `new`/`new_supervised` returns.
    pub(crate) _next_instance_counter: u64,
    /// Monotonic counter used to mint synthetic `sp-N`
    /// `SessionPromptId`s when dispatching prompts to the agent.
    pub(crate) next_session_prompt_id: u64,
    /// Monotonic counter used to mint synthetic `ToolCallId`s when
    /// the agent emits a tool call with an empty id. See
    /// `synthesize_call_id` for why.
    pub(crate) next_synthetic_call_id: u64,
    /// Maps session_prompt_id → owning conversation for in-flight
    /// prompts. The conversation knows its `session_id`, so older
    /// `prompt_sessions[spid]` lookups become two hops:
    /// `prompt_conversations[spid]` → `conversations[cid].session_id`.
    pub(crate) prompt_conversations: std::collections::HashMap<SessionPromptId, ConversationId>,
    /// All in-flight conversations keyed by `ConversationId`. The
    /// user's interactive UI thread is one fixed entry (see
    /// `default_conversation_id`); side queries from extensions spawn
    /// additional entries that live until their final response is
    /// routed back to the requesting extension.
    pub(crate) conversations: std::collections::HashMap<ConversationId, Conversation>,
    /// Id of the user's main interactive conversation. Always present
    /// in `conversations` for the harness's whole lifetime.
    pub(crate) default_conversation_id: ConversationId,
    /// Whose turn it is in the agent interaction loop.
    pub(crate) turn_state: TurnState,
    /// Queued prompts waiting for the agent slot to free up.
    /// Each entry is (conversation_id, text) and is persisted only
    /// when it is actually dispatched. Holds both interactive user
    /// prompts (default conversation) and side queries from
    /// extensions (ext-query conversations).
    //
    // Future: add a steering queue for mid-turn injection. Steering
    // messages would be injected after tool-call turns complete but
    // before the next LLM call, allowing the user to redirect the
    // agent while it's working. See PI_PROMPT_QUEUEING.md for Pi's
    // two-tier (steering + follow-up) design.
    pub(crate) pending_prompts: VecDeque<(ConversationId, String)>,
    /// Append-only event debug log.
    pub(crate) debug_log: Option<DebugEventLog>,
    /// All available models as `"provider/model_id"` strings.
    pub(crate) available_models: Vec<ModelId>,
    /// Currently selected model as `"provider/model_id"`.
    pub(crate) selected_model: ModelId,
    /// Currently selected reasoning effort level.
    pub(crate) selected_effort: tau_proto::Effort,
    /// Currently selected reasoning summary mode. Sent to providers
    /// that advertise `supportsReasoningSummary`; ignored elsewhere.
    pub(crate) selected_thinking_summary: tau_proto::ThinkingSummary,
    /// Input tokens consumed by the most recent agent response, if
    /// the provider reported it. `None` until the first usage report
    /// for the current model.
    pub(crate) context_input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent
    /// response, if the provider reported them.
    pub(crate) context_cached_tokens: Option<u64>,
    /// Percentage of the selected model's context window currently
    /// used. `None` when the model's context window is unknown.
    pub(crate) context_percent_used: Option<u8>,
    /// Provider/model registry, kept for runtime lookups (e.g.
    /// computing available efforts per current model).
    pub(crate) model_registry: tau_config::settings::ModelRegistry,
    /// Skills discovered by extensions, keyed by name.
    pub(crate) discovered_skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// AGENTS.md files discovered by extensions, in delivery order.
    pub(crate) discovered_agents_files: Vec<DiscoveredAgentsFile>,
    /// Sessions whose AGENTS/skill discovery has completed.
    pub(crate) initialized_sessions: std::collections::HashSet<SessionId>,
    /// Session prompt IDs that have already been completed by the agent.
    /// Used to dedupe duplicate `AgentResponseFinished` events that can
    /// arise under at-least-once delivery (e.g. an agent that reconnects
    /// after a crash and replays its last prompt).
    pub(crate) completed_prompts: std::collections::HashSet<SessionPromptId>,
    /// Tool invocations from the current agent turn that have not been
    /// dispatched yet. Drained in FIFO order by
    /// `drain_pending_tool_invocations` whenever the in-flight set
    /// allows the next call through. Cleared out implicitly: a turn
    /// only completes once this is empty and `in_flight_tool_kinds` is
    /// empty.
    pub(crate) pending_tool_invocations:
        VecDeque<(ConversationId, AgentToolCall, tau_proto::ToolSideEffects)>,
    /// Kind of every tool call currently dispatched but not yet
    /// completed (no `ToolResult`/`ToolError` received). Keyed by
    /// `call_id`. Used by the dispatch state machine to decide whether
    /// the next queued invocation can proceed: a `Pure` call may go
    /// whenever no `Mutating` is in flight; a `Mutating` call may go
    /// only when this set is empty.
    pub(crate) in_flight_tool_kinds:
        std::collections::HashMap<ToolCallId, tau_proto::ToolSideEffects>,
    /// Directory layout (config + state) the harness reads and writes.
    pub(crate) dirs: tau_config::settings::TauDirs,
}

pub(crate) type AgentRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

pub(crate) fn default_agent_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    tau_agent::run(r, w).map_err(|e| e.to_string())
}

impl Harness {
    /// Creates a harness with in-process extensions (agent, fs, shell).
    ///
    /// `eager_session_id` is the session that pre-warm (AGENTS.md + skill
    /// discovery) targets, and is also where `events.jsonl` lands. Subsequent
    /// prompts for *other* session ids lazy-init.
    pub(crate) fn new(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
    ) -> Result<Self, HarnessError> {
        Self::new_with_agent(
            state_dir,
            dirs,
            default_agent_runner,
            false,
            eager_session_id,
        )
    }

    pub(crate) fn new_with_agent(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        agent_runner: AgentRunner,
        include_echo: bool,
        eager_session_id: &str,
    ) -> Result<Self, HarnessError> {
        let state_dir = state_dir.into();
        let (tx, rx) = mpsc::channel();
        let mut bus =
            EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(
                PolicyStore::open(policy_store_path_from(&state_dir))?,
            )));
        let store = SessionStore::open(&state_dir)?;

        let own_pid = std::process::id();
        let mut _next_instance_counter: u64 = 0;

        let mut extensions = Vec::new();
        // Agent
        let (conn_id, thread) =
            spawn_in_process("agent", ClientKind::Agent, agent_runner, &mut bus, &tx)?;
        let agent_connection_id = conn_id.clone();
        let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
        _next_instance_counter += 1;
        extensions.push(ExtensionEntry {
            name: "agent".to_owned(),
            instance_id: iid,
            connection_id: conn_id,
            kind: ClientKind::Agent,
            pid: Some(own_pid),
            in_process_thread: Some(thread),
            supervised_config: None,
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        });

        // Shell and filesystem tools
        let (conn_id, thread) = spawn_in_process(
            "shell",
            ClientKind::Tool,
            move |r, w| tau_ext_shell::run(r, w, include_echo).map_err(|e| e.to_string()),
            &mut bus,
            &tx,
        )?;
        let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
        _next_instance_counter += 1;
        extensions.push(ExtensionEntry {
            name: "shell".to_owned(),
            instance_id: iid,
            connection_id: conn_id,
            kind: ClientKind::Tool,
            pid: Some(own_pid),
            in_process_thread: Some(thread),
            supervised_config: None,
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        });

        let (available_models, selected_model, model_registry, harness_settings) =
            load_model_list(&dirs);
        let selected_effort = selected_effort_for_model(
            &dirs,
            &harness_settings,
            &model_registry,
            selected_model.as_str(),
        );

        let default_conversation_id = ConversationId::new("default");
        let default_head = store.session(eager_session_id).and_then(|tree| tree.head());
        let mut conversations = std::collections::HashMap::new();
        conversations.insert(
            default_conversation_id.clone(),
            Conversation::new(
                default_conversation_id.clone(),
                eager_session_id.into(),
                tau_proto::PromptOriginator::User,
                default_head,
                None,
            ),
        );

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            current_session_id: eager_session_id.into(),
            pending_request_sessions: VecDeque::new(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tool_names: std::collections::HashMap::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            _next_instance_counter,
            next_session_prompt_id: 0,
            next_synthetic_call_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            conversations,
            default_conversation_id,
            turn_state: TurnState::Idle,
            pending_prompts: VecDeque::new(),
            debug_log: None,
            available_models,
            selected_model,
            selected_effort,
            selected_thinking_summary: tau_proto::ThinkingSummary::Auto,
            context_input_tokens: None,
            context_cached_tokens: None,
            context_percent_used: None,
            model_registry,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            dirs,
        };

        // Debug log lives next to the eager-init session's events file
        // so the session dir stays self-contained: `events.cbor` +
        // `events.jsonl` + `meta.json` + `lock`.
        let _ = harness.enable_debug_log(&state_dir.join(eager_session_id))?;
        // Record cwd in meta.json so `-r` (resume most recent for this
        // cwd) can find this session even before it has any log entries.
        // Also acquires the flock on `<state_dir>/<eager_session_id>/lock`.
        harness
            .store
            .record_session_meta(eager_session_id, std::env::current_dir().ok())?;

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        harness.register_harness_tools();
        harness.check_config_exists();
        harness.check_config_parses();

        // Eager session init for the default session. INTENTIONAL —
        // do NOT "simplify" this to lazy-on-first-prompt.
        //
        // Reasons this is a design choice, not dead weight:
        //
        // 1. **Pre-warm AGENTS.md and skill discovery.** The default session is the
        //    fallback when a caller (embedded or socket) doesn't specify one, and even
        //    when callers pick their own `chat-<ts>` id they still benefit: ext-shell
        //    has already walked `~/.agents/` + the cwd ancestor chain once, so the
        //    second init is cache-warm.
        //
        // 2. **Surface discovery before the first prompt.** The CLI prints "loaded: …"
        //    as events arrive; doing this at startup gives the user visible
        //    confirmation that their AGENTS.md was found — before they type anything —
        //    instead of bundling that feedback into the first agent response.
        //
        // 3. **Fail loudly at startup, not mid-first-turn.** If a provider hangs or the
        //    discovery logic panics, the process hits `StartupTimeout` here rather than
        //    appearing to accept the first prompt and then silently stalling.
        //
        // Every past agent that touched this code has "noticed" that
        // the CLI uses `chat-<ts>` session ids and concluded the eager
        // init is wasted work. It isn't. Please resist the urge.
        harness.start_session_init(
            eager_session_id.into(),
            tau_proto::SessionStartReason::Initial,
        );
        harness.wait_for_session_init()?;
        Ok(harness)
    }

    /// Creates a harness from configuration, spawning real child processes.
    pub(crate) fn from_config(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
    ) -> Result<Self, HarnessError> {
        let state_dir = state_dir.into();
        let (tx, rx) = mpsc::channel();
        let mut bus =
            EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(
                PolicyStore::open(policy_store_path_from(&state_dir))?,
            )));
        let store = SessionStore::open(&state_dir)?;

        let mut extensions = Vec::new();
        let mut _next_instance_counter: u64 = 0;
        let mut agent_connection_id = None;

        for ext_config in &config.extensions {
            let kind = match ext_config.role.as_deref() {
                Some("agent") => ClientKind::Agent,
                _ => ClientKind::Tool,
            };

            let log_path =
                extension_stderr_log_path(&state_dir, eager_session_id, &ext_config.name);
            let (conn_id, child_pid) =
                spawn_supervised(ext_config, kind.clone(), Some(log_path), &mut bus, &tx)?;

            if kind == ClientKind::Agent {
                agent_connection_id = Some(conn_id.clone());
            }
            let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
            _next_instance_counter += 1;
            extensions.push(ExtensionEntry {
                name: ext_config.name.clone(),
                instance_id: iid,
                connection_id: conn_id,
                kind: kind.clone(),
                pid: Some(child_pid),
                in_process_thread: None,
                supervised_config: Some(ext_config.clone()),
                restart_attempt: 0,
                state: ExtensionState::Spawning,
                last_acked: tau_proto::LogEventId::default(),
            });
        }

        let agent_connection_id = agent_connection_id.ok_or(HarnessError::NoAgentConfigured)?;

        let (available_models, selected_model, model_registry, harness_settings) =
            load_model_list(&dirs);
        let selected_effort = selected_effort_for_model(
            &dirs,
            &harness_settings,
            &model_registry,
            selected_model.as_str(),
        );

        let default_conversation_id = ConversationId::new("default");
        let default_head = store.session(eager_session_id).and_then(|tree| tree.head());
        let mut conversations = std::collections::HashMap::new();
        conversations.insert(
            default_conversation_id.clone(),
            Conversation::new(
                default_conversation_id.clone(),
                eager_session_id.into(),
                tau_proto::PromptOriginator::User,
                default_head,
                None,
            ),
        );

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            current_session_id: eager_session_id.into(),
            pending_request_sessions: VecDeque::new(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tool_names: std::collections::HashMap::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            _next_instance_counter,
            next_session_prompt_id: 0,
            next_synthetic_call_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            conversations,
            default_conversation_id,
            turn_state: TurnState::Idle,
            pending_prompts: VecDeque::new(),
            debug_log: None,
            available_models,
            selected_model,
            selected_effort,
            selected_thinking_summary: tau_proto::ThinkingSummary::Auto,
            context_input_tokens: None,
            context_cached_tokens: None,
            context_percent_used: None,
            model_registry,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            dirs,
        };

        let _ = harness.enable_debug_log(&state_dir.join(eager_session_id))?;
        // Record cwd in meta.json so `-r` (resume most recent for this
        // cwd) can find this session even before it has any log entries.
        // Also acquires the flock on `<state_dir>/<eager_session_id>/lock`.
        harness
            .store
            .record_session_meta(eager_session_id, std::env::current_dir().ok())?;

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        harness.register_harness_tools();
        harness.check_config_exists();
        harness.check_config_parses();

        harness.start_session_init(
            eager_session_id.into(),
            tau_proto::SessionStartReason::Initial,
        );
        harness.wait_for_session_init()?;
        Ok(harness)
    }

    fn log_event(&mut self, harness_event: &HarnessEvent) {
        if let Some(log) = &mut self.debug_log {
            log.log_harness_event(harness_event);
        }
    }

    /// Session id of the conversation that owns a given in-flight
    /// prompt, or `None` if the prompt id is unknown.
    fn session_id_for_prompt(&self, spid: &SessionPromptId) -> Option<SessionId> {
        let cid = self.prompt_conversations.get(spid)?;
        self.conversations.get(cid).map(|c| c.session_id.clone())
    }

    /// Session id of the conversation that owns a given in-flight
    /// tool call, or `None` if the call id is unknown.
    fn session_id_for_tool_call(&self, call_id: &ToolCallId) -> Option<SessionId> {
        let cid = self.tool_conversations.get(call_id)?;
        self.conversations.get(cid).map(|c| c.session_id.clone())
    }

    /// Conversation id that owns a given tool call, if any.
    #[allow(dead_code)] // used by the ext-query path wired in a follow-up step
    fn conversation_for_tool_call(&self, call_id: &ToolCallId) -> Option<ConversationId> {
        self.tool_conversations.get(call_id).cloned()
    }

    /// Conversation id that owns a given in-flight prompt, if any.
    fn conversation_for_prompt(&self, spid: &SessionPromptId) -> Option<ConversationId> {
        self.prompt_conversations.get(spid).cloned()
    }

    /// Publishes an event for a specific conversation, ensuring the
    /// session tree's head is positioned at that conversation's local
    /// cursor first. If the tree head currently belongs to another
    /// conversation, an explicit `UiNavigateTree` is emitted to bounce
    /// it back so the new event parents on the right node.
    ///
    /// After publishing, the conversation's local head is refreshed
    /// from the tree's new head — wherever the just-published event
    /// landed becomes the conversation's next-parent slot.
    ///
    /// This helper is what makes branching prompts work: the default
    /// (user) conversation can keep advancing while a side conversation
    /// from an extension grows its own branch off some earlier node;
    /// each side publish brackets its own navigate-then-append.
    fn publish_for_conversation(&mut self, cid: &ConversationId, event: Event) {
        let session_id = self
            .conversations
            .get(cid)
            .map(|c| c.session_id.clone())
            .expect("publish_for_conversation: unknown conversation id");
        let want_head = self.conversations.get(cid).and_then(|c| c.head);
        let have_head = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.head());
        if want_head != have_head {
            match want_head {
                Some(target) => self.publish_event(
                    None,
                    Event::UiNavigateTree(tau_proto::UiNavigateTree {
                        session_id: session_id.clone(),
                        node_id: target.0,
                    }),
                ),
                None => {
                    self.emit_info(&format!(
                        "conversation `{cid}` has no stored head but session `{}` is non-empty; \
                         appending at current tree head",
                        session_id.as_str()
                    ));
                }
            }
        }
        self.publish_event(None, event);
        let new_head = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.head());
        if let Some(c) = self.conversations.get_mut(cid) {
            c.head = new_head;
        }
    }

    /// Publishes an event to both the event bus and the event log.
    fn publish_event(&mut self, source: Option<&str>, event: Event) {
        let transient = event.defaults_to_transient();
        self.publish_event_with_transient(source, event, transient);
    }

    fn publish_event_with_transient(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
    ) {
        self.persist_session_event(source, &event, transient);
        let seq = self
            .event_log
            .append(source.map(tau_proto::ConnectionId::from), event.clone());
        // Wrap in a `LogEvent` envelope so subscribers get the id and
        // can ack after processing. Receivers that don't care (UIs)
        // call `peel_log()` and discard the id.
        let log_event = Event::LogEvent(tau_proto::LogEvent {
            id: tau_proto::LogEventId::new(seq),
            event: Box::new(event),
        });
        let _ = self.bus.publish_from(source, log_event);
    }

    fn persist_session_event(&mut self, source: Option<&str>, event: &Event, transient: bool) {
        if transient || event.is_transient() {
            return;
        }
        let Some(session_id) = self.session_id_for_event(event) else {
            return;
        };
        let source = source.map(tau_proto::ConnectionId::from);
        let _ = self
            .store
            .append_session_event(session_id.as_str(), source, event.clone());
    }

    fn session_id_for_event(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::UiPromptSubmitted(prompt) => Some(prompt.session_id.clone()),
            Event::UiShellCommand(command) => Some(command.session_id.clone()),
            Event::UiSwitchSession(req) => Some(req.new_session_id.clone()),
            Event::UiTreeRequest(req) => Some(req.session_id.clone()),
            Event::UiNavigateTree(req) => Some(req.session_id.clone()),
            Event::SessionPromptQueued(queued) => Some(queued.session_id.clone()),
            Event::SessionStarted(started) => Some(started.session_id.clone()),
            Event::SessionShutdown(shutdown) => Some(shutdown.session_id.clone()),
            Event::SessionPromptCreated(created) => Some(created.session_id.clone()),
            Event::SessionUserMessageInjected(injected) => Some(injected.session_id.clone()),
            Event::AgentPromptSubmitted(submitted) => {
                self.session_id_for_prompt(&submitted.session_prompt_id)
            }
            Event::AgentResponseUpdated(updated) => {
                self.session_id_for_prompt(&updated.session_prompt_id)
            }
            Event::AgentResponseFinished(finished) => {
                self.session_id_for_prompt(&finished.session_prompt_id)
            }
            Event::ToolRequest(request) => self.session_id_for_tool_call(&request.call_id),
            Event::ToolResult(result) => self.session_id_for_tool_call(&result.call_id),
            Event::ToolError(error) => self.session_id_for_tool_call(&error.call_id),
            Event::ToolProgress(progress) => self.session_id_for_tool_call(&progress.call_id),
            Event::ShellCommandFinished(finished) => Some(finished.session_id.clone()),
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_) => {
                Some(self.current_session_id.clone())
            }
            Event::ExtensionEvent(event) => event.session_id.clone(),
            _ => None,
        }
    }

    fn enable_debug_log(&mut self, dir: &Path) -> Result<PathBuf, HarnessError> {
        let log = DebugEventLog::open(dir)?;
        let path = log.path().to_path_buf();
        self.debug_log = Some(log);
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // Startup
    // -----------------------------------------------------------------------

    /// Drives the event loop until the in-flight session initialization
    /// completes (turn state returns to `Idle`). Called at harness
    /// startup after the eager `start_session_init` for the default
    /// session — see that call site for the design rationale.
    fn wait_for_session_init(&mut self) -> Result<(), HarnessError> {
        if self.turn_state.is_idle() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.turn_state.is_idle() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let event = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    self.handle_extension_event(&connection_id, event)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    self.handle_disconnect(&connection_id);
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
        Ok(())
    }

    /// Drives the event loop until every configured extension reaches
    /// `ExtensionState::Ready`. Replaces the old `wait_for_startup(n)`:
    /// state transitions are tracked per-extension so the same predicate
    /// can also gate runtime dispatch in `dispatch_blocked`.
    fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        if self.extensions_all_ready() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.extensions_all_ready() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let event = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    self.handle_extension_event(&connection_id, event)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let name = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| connection_id.to_string());
                    self.handle_disconnect(&connection_id);
                    return Err(HarnessError::Participant(format!(
                        "{name} disconnected during startup"
                    )));
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Main event loop (daemon mode)
    // -----------------------------------------------------------------------

    pub(crate) fn run_event_loop(
        &mut self,
        max_clients: Option<usize>,
        mut exit_on_disconnect: bool,
    ) -> Result<(), HarnessError> {
        let mut served_clients = 0_usize;
        let mut ever_attached = false;
        loop {
            if max_clients.is_some_and(|max| served_clients >= max) {
                break;
            }
            // `exit_on_disconnect`: once at least one UI has been
            // attached, exiting the moment the last one leaves lets
            // `tau run` behave like a normal foreground command.
            // Before any UI attaches we wait — otherwise a slightly
            // late first connect would race us into immediate exit.
            if exit_on_disconnect && ever_attached && self.client_writers.is_empty() {
                break;
            }
            let Ok(event) = self.rx.recv() else { break };
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    let origin = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.origin.clone());
                    match origin {
                        Some(ConnectionOrigin::Socket) => {
                            // `/detach` → stay alive even after this
                            // UI leaves; a later `tau run --attach`
                            // can pick up right here.
                            if matches!(event, Event::UiDetachRequest(_)) {
                                exit_on_disconnect = false;
                            }
                            let keep = self.handle_client_event(&connection_id, event)?;
                            if !keep {
                                let _ = self.bus.disconnect(&connection_id);
                                served_clients += 1;
                            }
                        }
                        Some(_) => self.handle_extension_event(&connection_id, event)?,
                        None => {} // already disconnected
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let is_agent = connection_id == self.agent_connection_id;
                    let was_socket = self
                        .bus
                        .connection(&connection_id)
                        .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
                    self.handle_disconnect(&connection_id);
                    if was_socket {
                        served_clients += 1;
                    }
                    if is_agent {
                        return Err(HarnessError::Participant("agent disconnected".to_owned()));
                    }
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                    ever_attached = true;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Client acceptance
    // -----------------------------------------------------------------------

    fn accept_client(&mut self, stream: UnixStream) -> Result<(), HarnessError> {
        let write_stream = stream.try_clone()?;
        let writer_tx = spawn_writer_thread(write_stream, WriterShutdown::CloseStream);
        let writer_tx_for_follower = writer_tx.clone();
        let conn_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: tau_proto::ConnectionId::default(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        self.client_writers
            .insert(conn_id.clone(), writer_tx_for_follower);
        spawn_reader_thread(conn_id, stream, self.tx.clone());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_extension_event(
        &mut self,
        source_id: &str,
        event: Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::Ack(ack) => {
                // Cumulative ack: advance the cursor if it moves
                // forward, ignore otherwise (duplicates, late acks).
                if let Some(entry) = self
                    .extensions
                    .iter_mut()
                    .find(|e| e.connection_id.as_str() == source_id)
                {
                    if ack.up_to.get() > entry.last_acked.get() {
                        entry.last_acked = ack.up_to;
                    }
                }
            }
            Event::LifecycleHello(hello) => {
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.publish_event(Some(source_id), Event::LifecycleHello(hello));
                self.send_lifecycle_configure(source_id);
            }
            Event::LifecycleConfigError(err) => {
                let name = self
                    .extensions
                    .iter()
                    .find(|e| e.connection_id.as_str() == source_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| "extension".to_owned());
                self.emit_info_important(&format!(
                    "extension {name} rejected its config: {}\nthe value of \
                     `extensions.{name}.config` in harness.json5 is being ignored",
                    err.message,
                ));
            }
            Event::LifecycleSubscribe(subscribe) => {
                self.bus
                    .set_subscriptions(source_id, subscribe.selectors.clone())?;
                self.publish_event(Some(source_id), Event::LifecycleSubscribe(subscribe));
            }
            Event::LifecycleReady(ready) => {
                self.emit_extension_ready(source_id);
                self.publish_event(Some(source_id), Event::LifecycleReady(ready));
                self.set_extension_state(source_id, ExtensionState::Ready);
                self.try_advance_queue();
            }
            Event::EmitEvent(emit) => {
                self.publish_event_with_transient(Some(source_id), *emit.event, emit.transient);
            }
            Event::ToolRegister(ToolRegister { tool }) => {
                let _ = self.registry.register(source_id, tool);
            }
            Event::ToolRequest(request) => {
                // Track session attribution before publishing — the
                // publish path's `session_id_for_event` reads
                // `pending_tool_sessions` to attach the persisted
                // record to the right session.
                self.track_tool_request_session(&request);
                self.publish_event(Some(source_id), Event::ToolRequest(request.clone()));
                match self
                    .registry
                    .route_tool_request(&mut self.bus, source_id, request.clone())
                {
                    Ok(route) => {
                        self.pending_tool_providers
                            .insert(request.call_id.clone(), route.provider_connection_id);
                    }
                    Err(ToolRouteError::NoProvider { tool_name }) => {
                        let call_id = request.call_id.to_string();
                        let owning_cid = self.tool_conversations.get(&request.call_id).cloned();
                        let error = ToolError {
                            call_id: request.call_id,
                            tool_name,
                            message: "no live provider available".to_owned(),
                            details: None,
                        };
                        self.publish_event(None, Event::ToolError(error));
                        if let Some(cid) = owning_cid {
                            self.sync_conversation_head(&cid);
                        }
                        self.clear_tool_call_tracking(&call_id);
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(result) => {
                if let Some(cid) = self.tool_conversations.get(&result.call_id).cloned() {
                    let call_id = result.call_id.to_string();
                    self.publish_event(Some(source_id), Event::ToolResult(result));
                    self.sync_conversation_head(&cid);
                    self.clear_tool_call_tracking(&call_id);
                    self.on_tool_call_complete(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool result for call_id={}",
                        result.call_id
                    ));
                }
            }
            Event::ToolError(error) => {
                if let Some(cid) = self.tool_conversations.get(&error.call_id).cloned() {
                    let call_id = error.call_id.to_string();
                    self.publish_event(Some(source_id), Event::ToolError(error));
                    self.sync_conversation_head(&cid);
                    self.clear_tool_call_tracking(&call_id);
                    self.on_tool_call_complete(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool error for call_id={}",
                        error.call_id
                    ));
                }
            }
            Event::ToolProgress(progress) => {
                self.publish_event(Some(source_id), Event::ToolProgress(progress));
            }
            Event::ShellCommandProgress(progress) => {
                // Pass-through: the UI renders chunks as they arrive.
                self.publish_event(Some(source_id), Event::ShellCommandProgress(progress));
            }
            Event::ShellCommandFinished(finished) => {
                // Publish first so the UI finalizes its render block
                // regardless of whether we inject into history.
                self.publish_event(
                    Some(source_id),
                    Event::ShellCommandFinished(finished.clone()),
                );
                if finished.include_in_context {
                    self.inject_user_shell_output(&finished);
                }
            }
            Event::ExtSkillAvailable(ref skill) => {
                self.discovered_skills.insert(
                    skill.name.clone(),
                    DiscoveredSkill {
                        source_id: source_id.into(),
                        description: skill.description.clone(),
                        file_path: std::path::PathBuf::from(&skill.file_path),
                        add_to_prompt: skill.add_to_prompt,
                    },
                );
                self.publish_event(Some(source_id), event);
            }
            Event::ExtAgentsMdAvailable(ref agents) => {
                let file_path = PathBuf::from(&agents.file_path);
                if let Some(existing) = self.discovered_agents_files.iter_mut().find(|existing| {
                    existing.source_id == source_id && existing.file_path == file_path
                }) {
                    existing.content = agents.content.clone();
                } else {
                    self.discovered_agents_files.push(DiscoveredAgentsFile {
                        source_id: source_id.into(),
                        file_path,
                        content: agents.content.clone(),
                    });
                }
                self.publish_event(Some(source_id), event);
            }
            Event::ExtensionContextReady(ready) => {
                self.publish_event(Some(source_id), Event::ExtensionContextReady(ready.clone()));
                self.handle_extension_context_ready(source_id, ready)?;
            }
            Event::ExtAgentQuery(query) => {
                self.handle_ext_agent_query(source_id, query)?;
            }
            Event::AgentPromptSubmitted(_) | Event::AgentResponseUpdated(_) => {
                self.publish_event(Some(source_id), event);
            }
            Event::AgentResponseFinished(response) => {
                self.handle_agent_response_finished(response)?;
            }
            other => {
                self.publish_event(Some(source_id), other);
            }
        }
        Ok(())
    }

    fn handle_client_event(&mut self, client_id: &str, event: Event) -> Result<bool, HarnessError> {
        match event {
            Event::LifecycleHello(hello) => {
                self.publish_event(Some(client_id), Event::LifecycleHello(hello));
                Ok(true)
            }
            Event::LifecycleSubscribe(subscribe) => {
                // Policy check via the bus.
                match self
                    .bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())
                {
                    Ok(()) => {
                        let selectors_for_replay = subscribe.selectors.clone();
                        self.publish_event(Some(client_id), Event::LifecycleSubscribe(subscribe));
                        self.replay_session_events(client_id, &selectors_for_replay);
                        self.replay_harness_info(client_id, &selectors_for_replay);
                        Ok(true)
                    }
                    Err(RouteError::SubscriptionDenied { reason, .. }) => {
                        let _ = self.bus.send_to(
                            client_id,
                            None,
                            Event::LifecycleDisconnect(LifecycleDisconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            }),
                        );
                        Ok(false)
                    }
                    Err(other) => Err(HarnessError::Route(other)),
                }
            }
            Event::UiModelSelect(select) => {
                if self.available_models.contains(&select.model) {
                    let was_empty = self.selected_model.is_empty();
                    self.selected_model = select.model.clone();
                    self.selected_effort = selected_effort_for_model(
                        &self.dirs,
                        &load_harness_settings_or_warn(&self.dirs),
                        &self.model_registry,
                        self.selected_model.as_str(),
                    );
                    save_harness_state(
                        &self.dirs,
                        self.selected_model.as_str(),
                        self.selected_effort,
                    );
                    self.context_input_tokens = None;
                    self.context_cached_tokens = None;
                    self.context_percent_used = None;
                    self.publish_event(
                        None,
                        Event::HarnessModelSelected(HarnessModelSelected {
                            model: self.selected_model.clone(),
                            context_window: model_context_window(
                                &self.model_registry,
                                self.selected_model.as_str(),
                            ),
                        }),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                            input_tokens: self.context_input_tokens,
                            cached_tokens: self.context_cached_tokens,
                            percent_used: self.context_percent_used,
                        }),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
                            level: self.selected_effort,
                        }),
                    );
                    // Levels depend on the new model's provider.
                    let levels =
                        efforts_for_model(&self.model_registry, self.selected_model.as_str());
                    self.publish_event(
                        None,
                        Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                            levels,
                        }),
                    );
                    // If we just went from no-model to having one,
                    // drain queued prompts.
                    if was_empty && self.turn_state.is_idle() {
                        self.try_advance_queue();
                    }
                } else {
                    self.publish_event(
                        None,
                        Event::HarnessInfo(tau_proto::HarnessInfo {
                            message: format!("unknown model: {}", select.model),

                            level: tau_proto::HarnessInfoLevel::Normal,
                        }),
                    );
                }
                Ok(true)
            }
            Event::UiSetEffort(req) => {
                let levels = efforts_for_model(&self.model_registry, self.selected_model.as_str());
                self.selected_effort = clamp_effort(req.level, &levels);
                save_harness_state(
                    &self.dirs,
                    self.selected_model.as_str(),
                    self.selected_effort,
                );
                self.publish_event(
                    None,
                    Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
                        level: self.selected_effort,
                    }),
                );
                Ok(true)
            }
            Event::UiPromptSubmitted(prompt) => {
                let submission =
                    self.submit_user_prompt(prompt.session_id.clone(), prompt.text.clone())?;
                if matches!(submission, PromptSubmission::Queued) {
                    self.publish_event(
                        None,
                        Event::SessionPromptQueued(SessionPromptQueued {
                            session_id: prompt.session_id.clone(),
                            text: prompt.text.clone(),
                        }),
                    );
                    if self.selected_model.is_empty() {
                        self.emit_info("no model selected — use /model to pick one");
                    }
                }
                Ok(true)
            }
            Event::UiSwitchSession(req) => {
                self.publish_event(Some(client_id), Event::UiSwitchSession(req.clone()));
                self.switch_session(req.new_session_id, req.reason)?;
                Ok(true)
            }
            Event::UiTreeRequest(req) => {
                self.publish_event(Some(client_id), Event::UiTreeRequest(req.clone()));
                self.handle_tree_request(&req.session_id);
                Ok(true)
            }
            Event::UiNavigateTree(req) => {
                // Validate the target node exists in *this* harness's
                // bound session before publishing — `apply_event` for
                // `UiNavigateTree` is also a no-op for unknown ids,
                // but we want a user-visible error message rather
                // than a silent drop.
                if self.handle_navigate_tree(&req.session_id, req.node_id) {
                    self.publish_event(Some(client_id), Event::UiNavigateTree(req));
                }
                Ok(true)
            }
            Event::LifecycleDisconnect(_) => Ok(false),
            other => {
                self.publish_event(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
        self.remove_discovered_context(connection_id);
        self.maybe_complete_session_init_for_disconnect(connection_id);
        self.fail_pending_tool_calls_for_connection(connection_id);
        self.set_extension_state(connection_id, ExtensionState::Disconnected);
        self.client_writers
            .remove(&tau_proto::ConnectionId::from(connection_id));
        let Some(meta) = self.bus.disconnect(connection_id) else {
            return;
        };
        if meta.origin == ConnectionOrigin::Supervised || meta.origin == ConnectionOrigin::InMemory
        {
            let _ = self.registry.unregister_connection(connection_id);
            self.emit_extension_exited(&meta.name);
        }
        if meta.origin == ConnectionOrigin::Supervised {
            if let Err(error) = self.try_respawn_supervised_extension(connection_id) {
                self.emit_info(&format!(
                    "failed to respawn extension {}: {error}",
                    meta.name
                ));
            }
        }
    }

    fn fail_pending_tool_calls_for_connection(&mut self, connection_id: &str) {
        let failed_call_ids: Vec<ToolCallId> = self
            .pending_tool_providers
            .iter()
            .filter_map(|(call_id, provider_id)| {
                if provider_id.as_str() == connection_id {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();

        for call_id in failed_call_ids {
            let tool_name = self
                .pending_tool_names
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| ToolName::from("unknown_tool"));
            let error = ToolError {
                call_id: call_id.clone(),
                tool_name,
                message: "tool provider disconnected".to_owned(),
                details: None,
            };
            // Publish first so `session_id_for_event` can still attribute
            // the persisted record via `pending_tool_sessions`. Then drop
            // the call's tracking maps.
            self.publish_event(None, Event::ToolError(error));
            self.clear_tool_call_tracking(call_id.as_str());
            self.on_tool_call_complete(call_id.as_str());
        }
    }

    fn try_respawn_supervised_extension(
        &mut self,
        connection_id: &str,
    ) -> Result<(), HarnessError> {
        let Some(index) = self
            .extensions
            .iter()
            .position(|e| e.connection_id.as_str() == connection_id)
        else {
            return Ok(());
        };
        let Some(config) = self.extensions[index].supervised_config.clone() else {
            return Ok(());
        };
        if self.extensions[index].kind == ClientKind::Agent {
            return Ok(());
        }

        self.extensions[index].restart_attempt += 1;
        let attempt = self.extensions[index].restart_attempt;
        let instance_id = self.extensions[index].instance_id;
        let name = self.extensions[index].name.clone();
        self.publish_event(
            Some("harness"),
            Event::ExtensionRestarting(tau_proto::ExtensionRestarting {
                instance_id,
                extension_name: name.clone().into(),
                pid: None,
                attempt,
                reason: Some("unexpected disconnect".to_owned()),
            }),
        );

        let kind = self.extensions[index].kind.clone();
        let log_path = extension_stderr_log_path(
            &self.dirs_state_dir(),
            self.current_session_id.as_str(),
            &config.name,
        );
        let (new_connection_id, child_pid) =
            spawn_supervised(&config, kind, Some(log_path), &mut self.bus, &self.tx)?;
        self.extensions[index].connection_id = new_connection_id;
        self.extensions[index].pid = Some(child_pid);
        self.extensions[index].state = ExtensionState::Spawning;
        self.extensions[index].last_acked = tau_proto::LogEventId::default();
        self.emit_extension_starting(&name);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Tool-call session bookkeeping
    // -----------------------------------------------------------------------
    //
    // Persistence of tool activity into the session tree is handled
    // automatically by the publish path: every published `ToolRequest`
    // / `ToolResult` / `ToolError` flows through
    // `persist_session_event`, which writes the event to the durable
    // per-session log and applies the same event to the in-memory
    // tree. The helpers below only maintain the runtime maps that
    // `session_id_for_event` reads to attribute incoming results back
    // to the originating session.

    /// Records that an extension-originated `ToolRequest` belongs to
    /// the harness's *default* conversation (the user's UI thread).
    /// Extensions don't currently carry an owning conversation on
    /// their tool requests; future work could extend the protocol so
    /// extension-side tools also attribute to a specific conversation.
    /// Must run *before* the request is published, so
    /// `session_id_for_event` can attribute the corresponding
    /// persisted event.
    fn track_tool_request_session(&mut self, request: &ToolRequest) {
        // Drain the legacy queue (always empty in current code) for
        // future-compat, then assign to the default conversation.
        let _ = self.pending_request_sessions.pop_front();
        self.tool_conversations.insert(
            request.call_id.clone(),
            self.default_conversation_id.clone(),
        );
        self.pending_tool_names
            .insert(request.call_id.clone(), request.tool_name.clone());
    }

    /// Releases the conversation/name/provider mappings for a
    /// completed tool call. Must run *after* the result/error event
    /// has been published, otherwise `session_id_for_event` would no
    /// longer be able to attribute the durable record.
    fn clear_tool_call_tracking(&mut self, call_id: &str) {
        self.tool_conversations.remove(call_id);
        self.pending_tool_names.remove(call_id);
        self.pending_tool_providers.remove(call_id);
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    fn find_extension_by_name(&self, name: &str) -> Option<&ExtensionEntry> {
        self.extensions.iter().find(|e| e.name == name)
    }

    fn find_extension_by_connection(&self, connection_id: &str) -> Option<&ExtensionEntry> {
        self.extensions
            .iter()
            .find(|e| e.connection_id == connection_id)
    }

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} starting"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionStarting(tau_proto::ExtensionStarting {
                instance_id: iid,
                extension_name: extension_name.into(),
                pid,
            }),
        );
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(ext) = self.find_extension_by_connection(connection_id) else {
            return;
        };
        let name = ext.name.clone();
        let iid = ext.instance_id;
        let pid = ext.pid;
        self.lifecycle_messages
            .push(format!("extension {name} ready"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionReady(tau_proto::ExtensionReady {
                instance_id: iid,
                extension_name: name.into(),
                pid,
            }),
        );
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} exited"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionExited(tau_proto::ExtensionExited {
                instance_id: iid,
                extension_name: extension_name.into(),
                pid,
                exit_code: None,
                signal: None,
            }),
        );
    }

    fn check_config_exists(&mut self) {
        if let Some(dir) = tau_config::settings::config_dir() {
            if !dir.join("harness.json5").exists() {
                self.emit_info_important(
                    "no config found; run `tau init` to create sample config files",
                );
            }
        }
    }

    /// Re-parse `harness.json5`. If parsing fails the harness has
    /// already fallen back to defaults (with a stderr warning), but
    /// stderr is easy to miss when the TUI takes over the terminal
    /// right after startup. Surface the error through `HarnessInfo`
    /// so it shows up as a system info block inline in the UI.
    fn check_config_parses(&mut self) {
        if let Err(error) = tau_config::settings::load_harness_settings_in(&self.dirs) {
            self.emit_info_important(&format!(
                "harness.json5 failed to parse — extensions and model selection from it are being IGNORED.\n{error}"
            ));
        }
    }

    /// Push the configured `config` value (from `harness.json5`) to
    /// the just-said-Hello extension. Sends point-to-point so it
    /// arrives even if the extension hasn't subscribed to the
    /// `lifecycle` category yet. In-process extensions don't carry
    /// a `supervised_config` so they get the empty default — they
    /// already accept configuration via constructor parameters.
    fn send_lifecycle_configure(&mut self, source_id: &str) {
        let config_json = self
            .extensions
            .iter()
            .find(|e| e.connection_id.as_str() == source_id)
            .and_then(|e| e.supervised_config.as_ref())
            .map(|cfg| cfg.config.clone())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let _ = self.bus.send_to(
            source_id,
            None,
            Event::LifecycleConfigure(tau_proto::LifecycleConfigure {
                config: tau_proto::json_to_cbor(&config_json),
            }),
        );
    }

    pub(crate) fn emit_info(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::HarnessInfoLevel::Normal);
    }

    fn emit_info_important(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::HarnessInfoLevel::Important);
    }

    fn emit_info_with_level(&mut self, message: &str, level: tau_proto::HarnessInfoLevel) {
        self.publish_event(
            Some("harness"),
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: message.to_owned(),
                level,
            }),
        );
    }

    fn replay_session_events(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let Ok(events) = self.store.session_events(self.current_session_id.as_str()) else {
            return;
        };
        for entry in events {
            if selector_matches_event(selectors, &entry.event) {
                let event = Event::LogEvent(tau_proto::LogEvent {
                    id: entry.id,
                    event: Box::new(entry.event),
                });
                let _ = self.bus.send_to(client_id, entry.source.as_deref(), event);
            }
        }
    }

    /// Replays harness info and extension lifecycle events to a
    /// late-joining client.
    ///
    /// Events that are persisted to the durable per-session log
    /// (`ExtAgentsMdAvailable`, `ExtensionContextReady`, …) are
    /// intentionally NOT replayed here — `replay_session_events`
    /// already delivers them from the durable log on the same
    /// subscribe. Including them here too caused the CLI to render
    /// each "loaded: …" / "session context ready" line twice.
    fn replay_harness_info(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let mut cursor = 0;
        while let Some(entry) = self.event_log.get_next_from(cursor) {
            cursor = entry.seq + 1;
            let dominated = matches!(
                entry.event,
                Event::HarnessInfo(_)
                    | Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
            );
            if dominated && selector_matches_event(selectors, &entry.event) {
                let _ = self
                    .bus
                    .send_to(client_id, entry.source.as_deref(), entry.event);
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            let _ = self.bus.send_to(client_id, None, models_event);
        }
        let selected_event = Event::HarnessModelSelected(HarnessModelSelected {
            model: self.selected_model.clone(),
            context_window: model_context_window(
                &self.model_registry,
                self.selected_model.as_str(),
            ),
        });
        if selector_matches_event(selectors, &selected_event) {
            let _ = self.bus.send_to(client_id, None, selected_event);
        }
        let context_event = Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: self.context_input_tokens,
            cached_tokens: self.context_cached_tokens,
            percent_used: self.context_percent_used,
        });
        if selector_matches_event(selectors, &context_event) {
            let _ = self.bus.send_to(client_id, None, context_event);
        }
        let effort_event = Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
            level: self.selected_effort,
        });
        if selector_matches_event(selectors, &effort_event) {
            let _ = self.bus.send_to(client_id, None, effort_event);
        }
        let levels = efforts_for_model(&self.model_registry, self.selected_model.as_str());
        let levels_event =
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable { levels });
        if selector_matches_event(selectors, &levels_event) {
            let _ = self.bus.send_to(client_id, None, levels_event);
        }
    }

    fn remove_discovered_context(&mut self, source_id: &str) {
        self.discovered_skills
            .retain(|_, skill| skill.source_id != source_id);
        self.discovered_agents_files
            .retain(|file| file.source_id != source_id);
    }

    fn session_init_provider_ids(&self) -> std::collections::HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "probe".into(),
            reason: tau_proto::SessionStartReason::Initial,
        });
        self.bus
            .connections()
            .into_iter()
            .filter(|connection| {
                connection.kind == ClientKind::Tool
                    && connection.origin != ConnectionOrigin::Socket
                    && self
                        .bus
                        .subscriptions(connection.id.as_str())
                        .is_some_and(|selectors| selector_matches_event(selectors, &event))
            })
            .map(|connection| connection.id)
            .collect()
    }

    fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        debug_assert_eq!(
            self.conversations[&self.default_conversation_id].session_id, session_id,
            "dispatch_user_prompt only valid for the default conversation",
        );
        let cid = self.default_conversation_id.clone();
        self.dispatch_prompt_for_conversation(&cid, text)
    }

    /// Dispatches one prompt for `cid`: publishes the
    /// `UiPromptSubmitted` event (head-bounced via
    /// `publish_for_conversation` so it lands on the conversation's
    /// branch), enters `AgentThinking`, and asks the agent for a
    /// completion.
    ///
    /// Used for both interactive user prompts on the default
    /// conversation and side-query prompts spawned by extensions.
    fn dispatch_prompt_for_conversation(
        &mut self,
        cid: &ConversationId,
        text: String,
    ) -> Result<(), HarnessError> {
        let (session_id, originator) = match self.conversations.get(cid) {
            Some(c) => (c.session_id.clone(), c.originator.clone()),
            None => {
                return Err(HarnessError::Participant(format!(
                    "dispatch_prompt_for_conversation: unknown conversation `{cid}`"
                )));
            }
        };
        self.publish_for_conversation(
            cid,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id: session_id.clone(),
                text,
                originator,
            }),
        );
        self.turn_state = TurnState::AgentThinking {
            _session_id: session_id,
            conversation_id: cid.clone(),
        };
        self.send_prompt_to_agent_for(cid);
        Ok(())
    }

    fn sync_default_conversation_head(&mut self) {
        let cid = self.default_conversation_id.clone();
        self.sync_conversation_head(&cid);
    }

    /// Refresh a conversation's local head from the session tree's
    /// current head. Used after publishing an event that grew the tree
    /// on this conversation's behalf via `publish_event` (which, unlike
    /// `publish_for_conversation`, doesn't update `c.head`). Without
    /// this, the next `publish_for_conversation` call would see a stale
    /// `want_head` and emit a `UiNavigateTree` that bounces the tree
    /// head backward, orphaning the just-published node.
    fn sync_conversation_head(&mut self, cid: &ConversationId) {
        let Some(session_id) = self.conversations.get(cid).map(|c| c.session_id.clone()) else {
            return;
        };
        let head = self
            .store
            .session(session_id.as_str())
            .and_then(|tree| tree.head());
        if let Some(conv) = self.conversations.get_mut(cid) {
            conv.head = head;
        }
    }

    fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    /// Spawn a fresh side conversation for an extension's
    /// [`tau_proto::ExtAgentQuery`] and dispatch it (or queue it if
    /// the agent slot is busy). The conversation branches off the
    /// user's *default* conversation's current head, so the side
    /// prompt + response land as a real branch in the session tree;
    /// the user's main head stays put.
    fn handle_ext_agent_query(
        &mut self,
        source_id: &str,
        query: tau_proto::ExtAgentQuery,
    ) -> Result<(), HarnessError> {
        let extension_name = self
            .extensions
            .iter()
            .find(|e| e.connection_id.as_str() == source_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| source_id.to_owned());

        // Mint a unique conversation id. Format kept human-readable
        // for /tree and debug logs.
        let cid = ConversationId::new(format!("extq-{}-{}", extension_name, query.query_id));
        if self.conversations.contains_key(&cid) {
            self.emit_info(&format!(
                "ignoring duplicate ext-query `{}` from `{}` — already in flight",
                query.query_id, extension_name
            ));
            return Ok(());
        }

        let session_id = self
            .conversations
            .get(&self.default_conversation_id)
            .map(|c| c.session_id.clone())
            .expect("default conversation always present");
        let branch_root = self
            .conversations
            .get(&self.default_conversation_id)
            .and_then(|c| c.head);

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.into(),
            query_id: query.query_id.clone(),
        };
        self.conversations.insert(
            cid.clone(),
            Conversation::new(
                cid.clone(),
                session_id,
                originator,
                branch_root,
                Some(source_id.into()),
            ),
        );

        // Queue the instruction; `try_advance_queue` dispatches when
        // the agent slot is idle and the model is ready.
        self.pending_prompts.push_back((cid, query.instruction));
        self.try_advance_queue();
        Ok(())
    }

    /// Resync the session tree's head to the default conversation's
    /// local head via a `UiNavigateTree` event. Called after a side
    /// conversation finishes so the user's next interactive prompt
    /// parents off the main branch instead of the side branch tip.
    fn snap_to_default_conversation(&mut self) {
        let session_id = self
            .conversations
            .get(&self.default_conversation_id)
            .map(|c| c.session_id.clone())
            .expect("default conversation always present");
        let want = self
            .conversations
            .get(&self.default_conversation_id)
            .and_then(|c| c.head);
        let have = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.head());
        if want != have
            && let Some(target) = want
        {
            self.publish_event(
                None,
                Event::UiNavigateTree(tau_proto::UiNavigateTree {
                    session_id,
                    node_id: target.0,
                }),
            );
        }
    }

    /// Queue a prompt when it cannot be sent directly yet, or dispatch
    /// it immediately when the session is initialized and the harness is
    /// ready to talk to the agent.
    ///
    /// Rejects prompts whose `session_id` doesn't match the harness's
    /// bound session — one harness owns one session, period. Switching
    /// sessions is a separate (future) operation that tears down +
    /// respawns extensions, not a silent fan-out.
    fn submit_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(PromptSubmission::Rejected { reason });
        }

        let cid = self.default_conversation_id.clone();
        if self.dispatch_blocked() || !self.session_initialized(&session_id) {
            self.pending_prompts.push_back((cid, text));
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }

        self.dispatch_user_prompt(session_id, text)?;
        Ok(PromptSubmission::Dispatched)
    }

    /// Broadcasts `SessionStarted` for `session_id` and enters
    /// `InitializingSession` until every subscribed tool extension has
    /// acknowledged with `ExtensionContextReady` (or all of them have
    /// disconnected). When the wait set drains, AGENTS.md content is
    /// injected into the session log and any queued user prompts are
    /// dispatched.
    /// Renders the session tree as one `harness.info` line per node.
    /// Bound-session-only: refuses if `session_id` doesn't match.
    fn handle_tree_request(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "tree request for `{}` ignored; harness is bound to `{}`",
                session_id.as_str(),
                self.current_session_id.as_str()
            ));
            return;
        }
        let lines: Vec<String> = match self.store.session(session_id.as_str()) {
            Some(tree) if !tree.nodes().is_empty() => {
                let head = tree.head();
                tree.nodes()
                    .iter()
                    .map(|node| {
                        let marker = if Some(node.id) == head { '*' } else { ' ' };
                        let parent = node
                            .parent_id
                            .map(|p| format!("<- {}", p.0))
                            .unwrap_or_else(|| "(root)".to_owned());
                        let preview = render_entry_preview(&node.entry);
                        format!("  {:>3} {} {:>8}  {}", node.id.0, marker, parent, preview)
                    })
                    .collect()
            }
            _ => {
                self.emit_info(&format!(
                    "session `{}` has no entries yet",
                    session_id.as_str()
                ));
                return;
            }
        };
        for line in lines {
            self.emit_info(&line);
        }
    }

    /// Validates a `UiNavigateTree` request against the bound session.
    /// Returns `true` if the request should be published (and the
    /// resulting `apply_event` will move the head); `false` if it
    /// should be dropped with a user-visible info message. The store
    /// itself has no imperative `set_head`; head moves come from
    /// folding the published `UiNavigateTree` event.
    fn handle_navigate_tree(&mut self, session_id: &SessionId, node_id: u64) -> bool {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "navigate ignored: harness is bound to `{}`",
                self.current_session_id.as_str()
            ));
            return false;
        }
        let valid = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.node(tau_core::NodeId(node_id)))
            .is_some();
        if !valid {
            self.emit_info(&format!("no node `{node_id}` in session"));
            return false;
        }
        self.emit_info(&format!("navigated to node {node_id}"));
        true
    }

    /// Tear down the current session and bind the harness to a new one.
    ///
    /// Pi-style: emit `SessionShutdown` for the old, drop in-flight
    /// prompts, swap the bound id, then run a fresh `start_session_init`
    /// for the new id with the given reason. Extension processes are
    /// kept across sessions (they're not respawned); extensions that
    /// hold per-session state subscribe to `session.shutdown` to
    /// flush/clean up.
    fn switch_session(
        &mut self,
        new_session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        if new_session_id == self.current_session_id {
            self.emit_info(&format!("already on session `{}`", new_session_id.as_str()));
            return Ok(());
        }

        let old_id = self.current_session_id.clone();
        self.publish_event(
            None,
            Event::SessionShutdown(tau_proto::SessionShutdown { session_id: old_id }),
        );

        // Drop in-flight work bound to the old session. Pending prompts
        // for it are abandoned (the user explicitly switched away).
        self.turn_state = TurnState::Idle;
        self.pending_prompts.clear();
        self.pending_request_sessions.clear();
        self.pending_tool_invocations.clear();
        self.tool_conversations.clear();
        self.pending_tool_names.clear();
        self.pending_tool_providers.clear();
        self.prompt_conversations.clear();

        // Rebind the default conversation to the new session and drop
        // any side conversations that were tied to the old one. Without
        // this, the next `dispatch_user_prompt` would assert because
        // `conversations[default].session_id` still points at the old
        // session id.
        let default_id = self.default_conversation_id.clone();
        let new_head = self
            .store
            .session(new_session_id.as_str())
            .and_then(|t| t.head());
        self.conversations.clear();
        self.conversations.insert(
            default_id.clone(),
            Conversation::new(
                default_id,
                new_session_id.clone(),
                tau_proto::PromptOriginator::User,
                new_head,
                None,
            ),
        );

        self.current_session_id = new_session_id.clone();

        // Record cwd + acquire flock on the new session dir before
        // anyone tries to write to its log.
        self.store
            .record_session_meta(new_session_id.as_str(), std::env::current_dir().ok())?;

        // Send the new debug log to the new session's dir, so each
        // session is self-contained.
        let _ = self.enable_debug_log(&self.dirs_state_dir().join(new_session_id.as_str()));

        self.start_session_init(new_session_id, reason);
        Ok(())
    }

    fn dirs_state_dir(&self) -> PathBuf {
        // The harness doesn't currently store the state dir directly;
        // derive it from the session store's location. SessionStore
        // exposes its root via the existing `state_dir()` accessor.
        self.store.state_dir().to_path_buf()
    }

    fn start_session_init(&mut self, session_id: SessionId, reason: tau_proto::SessionStartReason) {
        let waiting_on = self.session_init_provider_ids();
        if waiting_on.is_empty() {
            if let Err(error) = self.complete_session_init(session_id) {
                self.emit_info(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
            return;
        }

        for source_id in &waiting_on {
            self.remove_discovered_context(source_id.as_str());
        }

        self.turn_state = TurnState::InitializingSession {
            session_id: session_id.clone(),
            waiting_on,
        };
        self.publish_event(
            None,
            Event::SessionStarted(tau_proto::SessionStarted { session_id, reason }),
        );
    }

    fn handle_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                waiting_on,
            } if *session_id == ready.session_id => {
                waiting_on.remove(source_id);
                waiting_on.is_empty().then(|| session_id.clone())
            }
            _ => None,
        };

        if let Some(session_id) = completed_session {
            self.complete_session_init(session_id)?;
        }

        Ok(())
    }

    fn maybe_complete_session_init_for_disconnect(&mut self, connection_id: &str) {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                waiting_on,
            } => {
                let removed = waiting_on.remove(connection_id);
                if removed && waiting_on.is_empty() {
                    Some(session_id.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(session_id) = completed_session {
            if let Err(error) = self.complete_session_init(session_id) {
                self.emit_info(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
        }
    }

    fn complete_session_init(&mut self, session_id: SessionId) -> Result<(), HarnessError> {
        self.ensure_agents_context_inserted(session_id.as_str())?;
        if session_id == self.current_session_id {
            self.sync_default_conversation_head();
        }
        self.initialized_sessions.insert(session_id);
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    fn ensure_agents_context_inserted(&mut self, session_id: &str) -> Result<(), HarnessError> {
        if self.discovered_agents_files.is_empty() {
            return Ok(());
        }

        let text = render_agents_context_message(self.discovered_agents_files.iter());
        // Publish the injection as an event so it reaches the durable
        // session log and folds into the SessionTree the same way
        // every other entry does. No direct store mutation — the
        // event is the source of truth.
        self.publish_event(
            None,
            Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
                session_id: session_id.into(),
                text,
            }),
        );
        Ok(())
    }

    /// Persist a user-initiated `!` shell command's output as a
    /// tagged user message so the agent sees it in the next prompt.
    ///
    /// The XML-ish `<user_shell>` envelope lets the model reliably
    /// distinguish output the user pasted vs. output from its own
    /// tool calls, and survives round-tripping through conversation
    /// assembly.
    fn inject_user_shell_output(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let exit = finished
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| if finished.cancelled { "cancelled" } else { "?" }.to_owned());
        let text = format!(
            "<user_shell command={:?} exit_code={:?}>\n{}\n</user_shell>",
            finished.command, exit, finished.output,
        );
        self.publish_event(
            None,
            Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
                session_id: finished.session_id.clone(),
                text,
            }),
        );
    }

    /// Convenience wrapper that dispatches a prompt for the harness's
    /// default (user) conversation. Used by tests that want a quick
    /// "send the next prompt" without going through the full
    /// dispatch pipeline.
    #[cfg(test)]
    fn send_prompt_to_agent(&mut self, session_id: &str) -> SessionPromptId {
        debug_assert_eq!(
            self.conversations[&self.default_conversation_id]
                .session_id
                .as_str(),
            session_id,
            "send_prompt_to_agent only valid for the default conversation; \
             use send_prompt_to_agent_for() for side conversations",
        );
        let cid = self.default_conversation_id.clone();
        self.send_prompt_to_agent_for(&cid)
    }

    /// Mints a new `SessionPromptId`, registers it with `cid`'s
    /// conversation, and dispatches a `SessionPromptCreated` to the
    /// agent. Reads `system_prompt` / `messages` / `tools` from the
    /// conversation's session tree.
    ///
    /// Linear-prefix invariant: each subsequent prompt for the same
    /// session must be a strict byte-prefix extension of the prior
    /// one. Provider prompt caches (OpenAI, Anthropic, etc.) key
    /// entirely off the prefix bytes, so any per-turn churn in
    /// `system_prompt`, `tools`, or earlier messages busts the cache.
    /// See `linear_session_prompts_strictly_extend_previous_messages`.
    fn send_prompt_to_agent_for(&mut self, cid: &ConversationId) -> SessionPromptId {
        let conv = self
            .conversations
            .get(cid)
            .expect("send_prompt_to_agent_for: unknown conversation id");
        let session_id = conv.session_id.clone();
        let originator = conv.originator.clone();

        let tree = self.store.session(session_id.as_str());
        let messages = tree.map(assemble_conversation).unwrap_or_default();
        let tools = self.gather_tool_definitions();
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "(unknown)".to_owned());
        let session_prompt_id: SessionPromptId =
            format!("sp-{}", self.next_session_prompt_id).into();
        self.next_session_prompt_id += 1;
        self.prompt_conversations
            .insert(session_prompt_id.clone(), cid.clone());
        if let Some(c) = self.conversations.get_mut(cid) {
            c.in_flight_prompt = Some(session_prompt_id.clone());
        }

        // Publish SessionPromptCreated — both the agent and UI see it.
        let model = if self.selected_model.is_empty() {
            None
        } else {
            Some(self.selected_model.clone())
        };
        let event = Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id,
            system_prompt: build_system_prompt(&tools, &self.discovered_skills, &cwd),
            messages,
            tools,
            model,
            effort: self.selected_effort,
            thinking_summary: self.selected_thinking_summary,
            originator,
        });
        self.publish_event(None, event);

        session_prompt_id
    }

    fn gather_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .all_tools()
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters: spec.parameters.clone(),
            })
            .collect()
    }

    fn handle_agent_response_finished(
        &mut self,
        response: AgentResponseFinished,
    ) -> Result<(), HarnessError> {
        if response.input_tokens.is_some() || response.cached_tokens.is_some() {
            self.update_context_usage(response.input_tokens, response.cached_tokens);
        }
        // Dedupe: under at-least-once delivery the agent may resend a
        // finished-response after a reconnect. The first delivery
        // removed the entry from `prompt_conversations`; later ones
        // must be ignored rather than fall through to the "default"
        // session fallback, which would silently misroute the
        // duplicate.
        let Some(cid) = self.conversation_for_prompt(&response.session_prompt_id) else {
            self.emit_info(&format!(
                "discarding duplicate agent response for session_prompt_id={}",
                response.session_prompt_id
            ));
            return Ok(());
        };
        let session_id = self
            .conversations
            .get(&cid)
            .expect("conversation_for_prompt returned a known id")
            .session_id
            .clone();

        // `publish_event` flows the AgentResponseFinished into the
        // durable per-session log; the SessionTree fold appends an
        // `AgentMessage` carrying both `text` and `thinking` (when
        // text is present). Sync the owning conversation's head so
        // a subsequent tool dispatch via `publish_for_conversation`
        // doesn't navigate the tree backward to the pre-response head.
        self.publish_event(None, Event::AgentResponseFinished(response.clone()));
        self.sync_conversation_head(&cid);
        self.prompt_conversations
            .remove(response.session_prompt_id.as_str());
        if let Some(conv) = self.conversations.get_mut(&cid) {
            conv.in_flight_prompt = None;
        }
        self.completed_prompts
            .insert(response.session_prompt_id.clone());

        // Side-conversation handling: if this prompt originated from
        // an extension via ExtAgentQuery, route the final text back
        // to the requesting extension as ExtAgentQueryResult and
        // tear down the side conversation. The agent is single-flight
        // in v1, so no further tool dispatch can happen for this
        // conversation; pending_tool_invocations entries (if any) have
        // already been emitted into the bus and will complete normally.
        if let tau_proto::PromptOriginator::Extension {
            ref name,
            ref query_id,
        } = response.originator
            && response.tool_calls.is_empty()
        {
            let source = self
                .conversations
                .get(&cid)
                .and_then(|c| c.source_connection.clone());
            let result = tau_proto::ExtAgentQueryResult {
                query_id: query_id.clone(),
                text: response.text.clone().unwrap_or_default(),
                error: None,
            };
            if let Some(source) = source {
                let _ = self
                    .bus
                    .send_to(source.as_str(), None, Event::ExtAgentQueryResult(result));
            } else {
                // Should never happen — `source_connection` is set in
                // `handle_ext_agent_query` when the conversation is
                // spawned. Surface it via `harness.info` rather than
                // silently dropping so a future regression is visible.
                self.emit_info(&format!(
                    "ext-query result for `{}` (extension `{}`) had no source connection — \
                     dropping",
                    query_id, name
                ));
            }
            // Snap the tree head back to the default conversation's
            // local head so the user's next interactive prompt
            // continues on the main branch instead of the side branch.
            self.snap_to_default_conversation();
            self.conversations.remove(&cid);
            self.dispatch_next_or_idle(session_id.as_str());
            return Ok(());
        }

        if !response.tool_calls.is_empty() {
            // Tool calls to execute — agent stays busy. After all
            // tools complete, maybe_complete_agent_turn will send
            // a new prompt with the results.
            //
            // Future: check the steering queue here and inject any
            // steering messages into the next prompt alongside the
            // tool results, allowing the user to redirect the agent
            // mid-turn.
            // Normalize empty call_ids to a synthetic one. Models
            // sometimes emit hallucinated tool calls with both a
            // missing name *and* a missing id; an empty id would
            // collide with itself in `in_flight_tool_kinds` /
            // `pending_tool_sessions`, and would later render into
            // conversation history as an empty `call_id` which the
            // OpenAI Responses API rejects with
            // `input[N].call_id: empty string`. Fix it at the boundary.
            let normalized_calls: Vec<(AgentToolCall, tau_proto::ToolSideEffects)> = response
                .tool_calls
                .iter()
                .map(|call| {
                    let mut call = call.clone();
                    if call.id.as_str().is_empty() {
                        call.id = self.synthesize_call_id();
                    }
                    let kind = self.resolve_tool_kind(call.name.as_str());
                    (call, kind)
                })
                .collect();

            let remaining_calls: Vec<ToolCallId> = normalized_calls
                .iter()
                .map(|(call, _)| call.id.clone())
                .collect();
            self.turn_state = TurnState::ToolsRunning {
                session_id: session_id.clone(),
                conversation_id: cid.clone(),
                remaining_calls,
            };
            // Enqueue in the order the agent emitted them. Dispatch is
            // done by `drain_pending_tool_invocations`, which respects
            // the pure-vs-mutating ordering rule.
            for (call, kind) in normalized_calls {
                self.pending_tool_invocations
                    .push_back((cid.clone(), call, kind));
            }
            self.drain_pending_tool_invocations()?;
        } else {
            // No tool calls — turn is done. Dispatch next queued
            // prompt if any, otherwise mark agent as idle.
            self.dispatch_next_or_idle(&session_id);
        }

        Ok(())
    }

    fn update_context_usage(&mut self, input_tokens: Option<u64>, cached_tokens: Option<u64>) {
        let context_window =
            model_context_window(&self.model_registry, self.selected_model.as_str());
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if self.context_input_tokens == input_tokens
            && self.context_cached_tokens == cached_tokens
            && self.context_percent_used == percent_used
        {
            return;
        }
        self.context_input_tokens = input_tokens;
        self.context_cached_tokens = cached_tokens;
        self.context_percent_used = percent_used;
        self.publish_event(
            None,
            Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                input_tokens,
                cached_tokens,
                percent_used,
            }),
        );
    }

    /// Advances the front of the prompt queue when possible.
    ///
    /// Session initialization happens before prompt dispatch, so a fresh
    /// `chat-*` session can discover AGENTS.md and skills before the
    /// agent sees the first user message.
    fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle() || !self.extensions_all_ready() {
            return;
        }

        let Some((cid, _)) = self.pending_prompts.front() else {
            return;
        };
        let session_id = match self.conversations.get(cid) {
            Some(c) => c.session_id.clone(),
            None => {
                // Stale entry — conversation already torn down. Drop it
                // and try the next one on a re-entry.
                self.pending_prompts.pop_front();
                return;
            }
        };

        if !self.session_initialized(&session_id) {
            // Reachable only if the bound session somehow lost its
            // `initialized_sessions` entry; treat as a re-init.
            self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
            return;
        }

        if self.selected_model.is_empty() {
            return;
        }

        if let Some((cid, text)) = self.pending_prompts.pop_front() {
            if let Err(error) = self.dispatch_prompt_for_conversation(&cid, text) {
                self.emit_info(&format!("failed to dispatch queued prompt: {error}"));
                self.turn_state = TurnState::Idle;
            }
        }
    }

    /// True when a fresh user prompt should *not* be sent to the agent.
    ///
    /// Three conditions can block dispatch:
    /// - no model selected (handled by the existing /model UI flow);
    /// - the agent is mid-turn (`turn_state != Idle`);
    /// - some configured extension is not in `ExtensionState::Ready`.
    ///
    /// In-flight turns are *not* affected — only fresh dispatch.
    fn dispatch_blocked(&self) -> bool {
        self.selected_model.is_empty() || !self.turn_state.is_idle() || !self.extensions_all_ready()
    }

    /// True iff every configured extension has either reached `Ready`
    /// or dropped permanently.
    ///
    /// `Disconnected` counts as "no longer blocking": a dead extension
    /// may be on its way to being respawned, but the old connection is
    /// gone and should not wedge fresh prompt dispatch.
    /// Session initialization for a still-live session with a dead
    /// provider still completes correctly — `handle_disconnect`
    /// removes the entry from the `waiting_on` set.
    fn extensions_all_ready(&self) -> bool {
        self.extensions.iter().all(|e| {
            matches!(
                e.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            )
        })
    }

    /// Update an extension's lifecycle state, looked up by connection id.
    /// No-op if no entry matches (e.g. for socket clients).
    fn set_extension_state(&mut self, connection_id: &str, new_state: ExtensionState) {
        if let Some(entry) = self
            .extensions
            .iter_mut()
            .find(|e| e.connection_id.as_str() == connection_id)
        {
            entry.state = new_state;
        }
    }

    /// Dispatches the next queued prompt or marks the agent as idle.
    fn dispatch_next_or_idle(&mut self, _completed_session_id: &str) {
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
    }

    /// Mint a fresh synthetic `ToolCallId` for a hallucinated tool
    /// call that arrived with an empty id.
    ///
    /// The id has to be non-empty for two reasons:
    /// - the harness uses it as a map key in `in_flight_tool_kinds` /
    ///   `pending_tool_sessions`, and two empty ids would collide;
    /// - the next prompt we send to the model includes the rejection as a
    ///   `tool_use`/`tool_result` pair, and the OpenAI Responses API rejects
    ///   empty `call_id` strings outright.
    fn synthesize_call_id(&mut self) -> ToolCallId {
        let id = format!("harness-synth-{}", self.next_synthetic_call_id);
        self.next_synthetic_call_id += 1;
        id.into()
    }

    /// Returns the side-effect class of a tool name.
    ///
    /// Falls back to `Mutating` for unknown tools so an unregistered
    /// name does not accidentally parallelize.
    fn resolve_tool_kind(&self, name: &str) -> tau_proto::ToolSideEffects {
        self.registry
            .resolve_provider(name)
            .map(|provider| provider.tool.side_effects)
            .unwrap_or(tau_proto::ToolSideEffects::Mutating)
    }

    /// Whether any currently in-flight tool call is `Mutating`.
    fn has_mutating_in_flight(&self) -> bool {
        self.in_flight_tool_kinds
            .values()
            .any(|kind| matches!(kind, tau_proto::ToolSideEffects::Mutating))
    }

    /// State-machine drain: dispatch queued tool invocations in FIFO
    /// order while the in-flight set allows them through.
    ///
    /// Rule:
    /// - `Pure` head may dispatch when no `Mutating` is in-flight.
    /// - `Mutating` head may dispatch when the in-flight set is empty.
    ///
    /// Because the queue is FIFO and new calls are only enqueued from
    /// `handle_agent_response_finished` (one agent turn at a time),
    /// this gives the agent a sequential read-after-write view even
    /// though individual `Pure` calls still run concurrently.
    ///
    /// Call this after enqueuing new work or after any in-flight call
    /// completes.
    fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        while let Some((_, _, kind)) = self.pending_tool_invocations.front() {
            let compatible = match *kind {
                tau_proto::ToolSideEffects::Pure => !self.has_mutating_in_flight(),
                tau_proto::ToolSideEffects::Mutating => self.in_flight_tool_kinds.is_empty(),
            };
            if !compatible {
                break;
            }
            let (session_id, call, kind) = self
                .pending_tool_invocations
                .pop_front()
                .expect("front just peeked");
            let call_id: ToolCallId = call.id.clone().into();
            self.in_flight_tool_kinds.insert(call_id.clone(), kind);
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) = self.execute_agent_tool_call(&session_id, &call) {
                self.in_flight_tool_kinds.remove(&call_id);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    fn on_tool_call_complete(&mut self, call_id: &str) {
        let owned: ToolCallId = call_id.to_owned().into();
        self.in_flight_tool_kinds.remove(&owned);
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_info(&format!("queued tool dispatch failed: {error}"));
        }
        self.maybe_complete_agent_turn(call_id);
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let should_send = if let TurnState::ToolsRunning {
            remaining_calls, ..
        } = &mut self.turn_state
        {
            remaining_calls.retain(|id| id != completed_call_id);
            remaining_calls.is_empty()
        } else {
            false
        };
        if should_send {
            let (session_id, conversation_id) = if let TurnState::ToolsRunning {
                session_id,
                conversation_id,
                ..
            } = &self.turn_state
            {
                (session_id.clone(), conversation_id.clone())
            } else {
                unreachable!("just checked")
            };
            self.turn_state = TurnState::AgentThinking {
                _session_id: session_id,
                conversation_id: conversation_id.clone(),
            };
            self.send_prompt_to_agent_for(&conversation_id);
        }
    }

    fn execute_agent_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        // Agent output is untrusted — hallucinated or streaming-
        // artifact tool calls can arrive with empty or otherwise
        // invalid names. The wire type `ToolNameMaybe` preserves both
        // classes; here we pick the validated arm for the happy path
        // and route everything else to `reject_invalid_tool_call` with
        // a synthetic error the agent sees on its next turn.
        let tool_name = match &call.name {
            tau_proto::ToolNameMaybe::Valid(name) => name.clone(),
            tau_proto::ToolNameMaybe::Invalid(raw) => {
                self.reject_invalid_tool_call(
                    cid,
                    &call.id,
                    &call.arguments,
                    format!("invalid tool name {raw:?}: must be non-empty and match [a-zA-Z0-9_]+"),
                )?;
                return Ok(());
            }
        };

        // Handle harness-owned tools directly.
        if tool_name.as_str() == "skill" {
            return self.handle_skill_tool_call(cid, call);
        }

        let call_id: ToolCallId = call.id.clone().into();

        // Track conversation attribution before publishing — the
        // publish path persists the `ToolRequest` into the session
        // log and folds it into the SessionTree via `apply_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            arguments: call.arguments.clone(),
        };
        self.publish_for_conversation(cid, Event::ToolRequest(request.clone()));

        match self
            .registry
            .route_tool_request(&mut self.bus, &self.agent_connection_id, request)
        {
            Ok(route) => {
                self.pending_tool_providers
                    .insert(call_id.clone(), route.provider_connection_id);
            }
            Err(ToolRouteError::NoProvider { tool_name }) => {
                let error = ToolError {
                    call_id: call_id.clone(),
                    tool_name,
                    message: "no live provider available".to_owned(),
                    details: None,
                };
                self.publish_for_conversation(cid, Event::ToolError(error));
                self.clear_tool_call_tracking(call_id.as_str());
                self.on_tool_call_complete(&call.id);
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

        Ok(())
    }

    /// Synthesize a matched `ToolRequest` + `ToolError` pair for a
    /// tool call whose name couldn't be accepted as a `ToolName` (e.g.
    /// empty string from a hallucinated streaming response), publish
    /// both so they fold into the session tree, and drive the turn
    /// state machine forward.
    ///
    /// We use a placeholder `invalid_tool` name because
    /// `ToolError::tool_name` is a validated `ToolName`; the actual
    /// offending string is surfaced via the error message so the agent
    /// sees it in its next conversation turn.
    ///
    /// Publishing the `Requested` alongside the `Error` is
    /// load-bearing: `assemble_conversation` renders `Requested` as a
    /// `ContentBlock::ToolUse` and `Error` as a matching
    /// `ContentBlock::ToolResult`. Without the `Requested`, the next
    /// prompt would include a `function_call_output` with no
    /// corresponding `function_call`, which the OpenAI Responses API
    /// rejects with "No tool call found for function call output with
    /// call_id …".
    fn reject_invalid_tool_call(
        &mut self,
        cid: &ConversationId,
        call_id: &str,
        arguments: &CborValue,
        message: String,
    ) -> Result<(), HarnessError> {
        let placeholder: ToolName = "invalid_tool".into();
        let call_id_owned: ToolCallId = call_id.to_owned().into();
        // Seed the conversation mapping so `session_id_for_event`
        // attributes both the synthetic request and the synthetic
        // error to this conversation's session. A rejected call never
        // reached the normal dispatch path that would have inserted
        // these entries.
        self.tool_conversations
            .insert(call_id_owned.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id_owned.clone(), placeholder.clone());
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id: call_id_owned.clone(),
                tool_name: placeholder.clone(),
                arguments: arguments.clone(),
            }),
        );
        self.publish_for_conversation(
            cid,
            Event::ToolError(ToolError {
                call_id: call_id_owned,
                tool_name: placeholder,
                message,
                details: None,
            }),
        );
        self.clear_tool_call_tracking(call_id);
        self.on_tool_call_complete(call_id);
        Ok(())
    }

    /// Register harness-owned tools (e.g. `skill`).
    fn register_harness_tools(&mut self) {
        let _ = self.registry.register(
            HARNESS_CONNECTION_ID,
            tau_proto::ToolSpec {
                name: "skill".into(),
                description: Some(
                    "Discover and load skills — short, focused playbooks for \
                     specific tasks. Two actions:\n\
                     - `load`: fetch a skill's full content by exact name. Use \
                     this when you already know the skill name, e.g. one shown \
                     in <available_skills> or returned by `search`.\n\
                     - `search`: find skills whose name or description contain \
                     a keyword/phrase. Returns a list of {name, description} \
                     matches. Most skills are NOT pre-advertised, so use \
                     `search` whenever a task hints at a tool/workflow you \
                     might have a playbook for (e.g. \"git\", \"jujutsu\", \
                     \"changelog\"). Set `search_content: true` to also grep \
                     skill bodies."
                        .to_owned(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["load", "search"],
                            "description": "Which subcommand to run."
                        },
                        "name": {
                            "type": "string",
                            "description": "(action=load) Exact skill name to load."
                        },
                        "query": {
                            "type": "string",
                            "description": "(action=search) Keyword or phrase to match against skill names and descriptions (case-insensitive substring)."
                        },
                        "search_content": {
                            "type": "boolean",
                            "description": "(action=search) When true, also search the skill body. Default false."
                        }
                    },
                    "required": ["action"]
                })),
                side_effects: tau_proto::ToolSideEffects::Pure,
            },
        );
    }

    /// Handle the harness-owned `skill` tool call inline.
    ///
    /// Dispatches on the required `action` argument:
    /// - `load`: read skill body by exact name (returns name + content).
    /// - `search`: case-insensitive substring match across skill names and
    ///   descriptions; with `search_content: true`, also greps skill bodies.
    ///   Returns a list of `{name, description}` hits.
    fn handle_skill_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone().into();
        let tool_name: ToolName = "skill".into();

        // Track the conversation mapping first so the published
        // request + result both attribute to this conversation's
        // session via `session_id_for_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments: call.arguments.clone(),
            }),
        );

        let action = cbor_map_text(&call.arguments, "action");
        let result_event = match action {
            Some("load") => self.handle_skill_load(&call_id, &tool_name, &call.arguments),
            Some("search") => self.handle_skill_search(&call_id, &tool_name, &call.arguments),
            Some(other) => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: format!(
                    "unknown skill action: {other:?} (expected \"load\" or \"search\")"
                ),
                details: None,
            }),
            None => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "missing required argument: action (\"load\" or \"search\")".to_owned(),
                details: None,
            }),
        };

        // Publish, then drop the in-flight tracking — order matters:
        // `session_id_for_event` reads `tool_conversations` to
        // attribute the persisted record before we clear it.
        self.publish_for_conversation(cid, result_event);
        self.clear_tool_call_tracking(call_id.as_str());
        self.on_tool_call_complete(&call.id);

        Ok(())
    }

    fn handle_skill_load(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let Some(name) = cbor_map_text(arguments, "name") else {
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "missing required argument: name (action=load)".to_owned(),
                details: None,
            });
        };
        let Some(skill) = self.discovered_skills.get(name) else {
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: format!("unknown skill: {name}"),
                details: None,
            });
        };
        match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => {
                let body = tau_skills::strip_frontmatter(&content);
                Event::ToolResult(tau_proto::ToolResult {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: CborValue::Map(vec![
                        (
                            CborValue::Text("name".to_owned()),
                            CborValue::Text(name.to_owned()),
                        ),
                        (
                            CborValue::Text("content".to_owned()),
                            CborValue::Text(body.to_owned()),
                        ),
                    ]),
                })
            }
            Err(e) => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: format!("failed to read skill file: {e}"),
                details: None,
            }),
        }
    }

    fn handle_skill_search(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let Some(query) = cbor_map_text(arguments, "query") else {
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "missing required argument: query (action=search)".to_owned(),
                details: None,
            });
        };
        let search_content = cbor_map_bool(arguments, "search_content").unwrap_or(false);
        let needle = query.to_lowercase();
        if needle.is_empty() {
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "query must not be empty".to_owned(),
                details: None,
            });
        }

        let mut hits: Vec<(&tau_proto::SkillName, &DiscoveredSkill)> = self
            .discovered_skills
            .iter()
            .filter(|(name, skill)| {
                if name.as_str().to_lowercase().contains(&needle)
                    || skill.description.to_lowercase().contains(&needle)
                {
                    return true;
                }
                if search_content {
                    if let Ok(content) = std::fs::read_to_string(&skill.file_path) {
                        return content.to_lowercase().contains(&needle);
                    }
                }
                false
            })
            .collect();
        hits.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

        let matches = CborValue::Array(
            hits.into_iter()
                .map(|(name, skill)| {
                    CborValue::Map(vec![
                        (
                            CborValue::Text("name".to_owned()),
                            CborValue::Text(name.as_str().to_owned()),
                        ),
                        (
                            CborValue::Text("description".to_owned()),
                            CborValue::Text(skill.description.clone()),
                        ),
                    ])
                })
                .collect(),
        );
        Event::ToolResult(tau_proto::ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            result: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text(query.to_owned()),
                ),
                (
                    CborValue::Text("search_content".to_owned()),
                    CborValue::Bool(search_content),
                ),
                (CborValue::Text("matches".to_owned()), matches),
            ]),
        })
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    pub(crate) fn send_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        _source_id: Option<&str>,
    ) -> Result<InteractionOutcome, HarnessError> {
        // Synchronous test entrypoint: dispatch directly without going
        // through `submit_user_prompt`'s queue. The embedded test harness
        // has no model configured (nothing to select from) and no UI to
        // drain a queued prompt, so the queued-until-model path would
        // deadlock. AGENTS.md session init is exercised separately in
        // unit tests via `submit_user_prompt` / manual turn-state setup.
        self.dispatch_user_prompt(session_id.into(), text.to_owned())?;

        let started_at = Instant::now();
        let mut progress_messages = Vec::new();
        loop {
            let remaining = RESPONSE_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let event = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::ResponseTimeout)?;
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    if let Event::ToolProgress(ref progress) = event {
                        progress_messages.push(format_tool_progress(progress));
                    }
                    let is_final = matches!(
                        &event,
                        Event::AgentResponseFinished(r) if r.tool_calls.is_empty()
                    );
                    let final_text = if let Event::AgentResponseFinished(ref r) = event {
                        r.text.clone()
                    } else {
                        None
                    };
                    self.handle_extension_event(&connection_id, event)?;
                    if is_final {
                        return Ok(InteractionOutcome {
                            lifecycle_messages: Vec::new(),
                            progress_messages,
                            response: final_text.unwrap_or_default(),
                        });
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let is_agent = connection_id == self.agent_connection_id;
                    self.handle_disconnect(&connection_id);
                    if is_agent {
                        return Err(HarnessError::Participant("agent disconnected".to_owned()));
                    }
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    pub(crate) fn shutdown(&mut self) -> Result<(), HarnessError> {
        // Disconnect all extensions from the bus.  Dropping the
        // ChannelSink closes the writer channel, which triggers each
        // writer thread's shutdown sequence (send disconnect, close
        // stdin, wait/kill child).
        for ext in &self.extensions {
            let _ = self.bus.disconnect(&ext.connection_id);
        }

        // Join in-process extension threads.
        for i in 0..self.extensions.len() {
            if let Some(handle) = self.extensions[i].in_process_thread.take() {
                let name = self.extensions[i].name.clone();
                let result = handle.join().map_err(|_| HarnessError::ThreadJoin(name))?;
                result.map_err(HarnessError::Participant)?;
            }
            let name = self.extensions[i].name.clone();
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    #[cfg(test)]
    fn extension_connection_id(&self, name: &str) -> Option<&str> {
        self.extensions
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.connection_id.as_str())
    }
}

fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    // Match against the inner event for log deliveries (see the
    // matching helper in tau-core for the same reasoning).
    let target_name = match event {
        Event::LogEvent(env) => env.event.name(),
        _ => event.name(),
    };
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    })
}
