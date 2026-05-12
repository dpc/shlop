//! [`Harness`]: the central event loop. Owns the bus, registry, session
//! store, and the live extensions; routes every event between the agent,
//! tools, and clients.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, DefaultSubscriptionPolicy, EventBus,
    PolicyStore, RouteError, SessionStore, ToolRegistry, ToolRouteError,
};
use tau_proto::{
    AgentResponseFinished, AgentTokenUsage, AgentToolCall, CborValue, ClientKind, Disconnect,
    Event, EventSelector, ExtensionName, Frame, HarnessContextUsageChanged, HarnessModelSelected,
    Message, ModelId, SessionId, SessionPromptCreated, SessionPromptId, SessionPromptQueued,
    TokenUsageStats, ToolCallId, ToolDefinition, ToolError, ToolName, ToolRegister, ToolRequest,
    UiCancelPrompt,
};

use crate::conversation::{Conversation, ConversationId, ConversationTurnState};
use crate::daemon::InteractionOutcome;
use crate::debug_log::DebugEventLog;
use crate::dedup::{
    DEFAULT_THRESHOLD_BYTES, build_pointer_error_message, build_pointer_value,
    encode_error_for_hash, encode_for_hash, hash_truncated,
};
use crate::dirs::policy_store_path_from;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
use crate::error::HarnessError;
use crate::event::{
    ChannelSink, HarnessEvent, WriterShutdown, spawn_reader_thread, spawn_writer_thread,
};
use crate::event_log::EventLog;
use crate::extension::{
    ExtensionEntry, ExtensionState, extension_stderr_log_path, spawn_in_process, spawn_supervised,
};
use crate::format::{format_tool_progress, render_entry_preview};
use crate::harness::interception::{
    ConversationHeadSync, DeferredPublish, InterceptorRegistry, PendingIntercept,
};
use crate::model::{
    clamp_effort, clamp_thinking_summary, clamp_verbosity, context_percent_used, efforts_for_model,
    load_model_list, model_context_window, save_harness_state, selected_params_for_model,
    thinking_summaries_for_model, verbosities_for_model,
};
use crate::prompt::{
    assemble_conversation_from, build_system_prompt, cbor_map_bool, render_agents_context_message,
};
use crate::settings::{Config, load_harness_settings_or_warn};
use crate::turn::{PromptSubmission, TurnState};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests;

mod dispatch;
mod interception;
mod replay;
mod skill_tool;

/// Connection ID used for harness-owned tools (e.g. the `skill` tool).
pub(crate) const HARNESS_CONNECTION_ID: &str = "__harness__";

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
    pub(crate) client_writers: std::collections::HashMap<tau_proto::ConnectionId, Sender<Frame>>,
    /// Buffered human-readable lifecycle messages (extension init,
    /// model changes, etc.) surfaced to the UI as part of the next
    /// `InteractionOutcome`.
    pub(crate) lifecycle_messages: Vec<String>,
    /// Every spawned or in-process extension, keyed by current
    /// `ConnectionId`. Supervises restart, shutdown, and per-extension
    /// ack state. Lookups by connection id (the hot per-event path —
    /// every `Ack`, `Hello`, `Ready`, `Disconnected`) are O(1).
    pub(crate) extensions: std::collections::HashMap<tau_proto::ConnectionId, ExtensionEntry>,
    /// Spawn-order list of connection ids into `extensions`. Drives
    /// the deterministic "start every extension" and shutdown loops
    /// that a `HashMap` alone can't supply, and is updated in place
    /// whenever a supervised extension respawns with a fresh id.
    pub(crate) extension_order: Vec<tau_proto::ConnectionId>,
    /// Connection id assigned to the agent extension. Other code paths
    /// branch on this to special-case agent traffic (e.g. tool-call
    /// emission, session prompt routing).
    pub(crate) agent_connection_id: tau_proto::ConnectionId,
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
    /// Global harness state. Currently only tracks per-session init
    /// (waiting on extensions to announce skills + AGENTS.md). Agent
    /// turn state is per-conversation; multiple conversations may have
    /// in-flight prompts simultaneously and the agent extension
    /// serializes its own consumption of `SessionPromptCreated`.
    pub(crate) turn_state: TurnState,
    /// Append-only event debug log.
    pub(crate) debug_log: Option<DebugEventLog>,
    /// Event emission interceptors, exact name first and prefix fallback.
    pub(crate) interceptors: InterceptorRegistry,
    /// Currently in-flight interception. While `Some(_)`, no new
    /// publishes commit — they queue onto `deferred_publishes` until
    /// the awaited [`InterceptReply`] arrives (or the awaited
    /// connection disconnects, treated as `Pass(None)`).
    pub(crate) pending_intercept: Option<PendingIntercept>,
    /// Publishes that arrived while `pending_intercept` was active.
    /// Drained in FIFO order once the pending intercept resolves.
    pub(crate) deferred_publishes: VecDeque<DeferredPublish>,
    /// Conversations whose just-published `UiPromptSubmitted` (or
    /// equivalent user-message event) has not yet committed because
    /// it is parked in the interception chain. Each entry triggers
    /// a `send_prompt_to_agent_for` call once the next
    /// user-message-bearing event commits — that's when the
    /// `SessionTree` reflects the prompt and the assembled message
    /// list will actually contain it. Without this, the agent
    /// receives a stale message list (the "Ready" loop bug).
    pub(crate) pending_user_prompt_dispatches: VecDeque<ConversationId>,
    /// All available models.
    pub(crate) available_models: Vec<ModelId>,
    /// Currently selected model. `None` means no model is selected
    /// yet (no providers configured, or the user hasn't picked one).
    pub(crate) selected_model: Option<ModelId>,
    /// Currently selected per-prompt model knobs. Stamped onto every
    /// outgoing [`tau_proto::SessionPromptCreated`]; mutated by
    /// `UiSetEffort` / `UiSetVerbosity` / `UiSetThinkingSummary` and
    /// reseeded on every `UiModelSelect` from
    /// [`selected_params_for_model`].
    pub(crate) selected_params: tau_proto::ModelParams,
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
    /// Current-session token usage totals.
    pub(crate) token_usage: TokenUsageStats,
    /// Provider/model for each prompt sent to the agent, used to
    /// attribute the corresponding finished response even if the user
    /// switches models while it is in flight.
    pub(crate) prompt_models: std::collections::HashMap<SessionPromptId, ModelId>,
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
    /// Prompt ids canceled by `/cancel`. Late agent events for these
    /// prompts are ignored and never folded into session state.
    pub(crate) canceled_prompts: std::collections::HashSet<SessionPromptId>,
    /// Directory layout (config + state) the harness reads and writes.
    pub(crate) dirs: tau_config::settings::TauDirs,
}

pub(crate) type AgentRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

/// One in-process tool extension to spawn alongside the agent during
/// [`Harness::new_with_agent`]. Callers (the embedded helper, the echo
/// test path) supply these explicitly so the harness library doesn't
/// hard-wire any specific tool implementation.
pub(crate) struct InProcessTool {
    pub(crate) name: &'static str,
    pub(crate) runner: fn(UnixStream, UnixStream) -> Result<(), String>,
}

pub(crate) fn default_agent_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    tau_agent::run(r, w).map_err(|e| e.to_string())
}

/// Returns a closure that mints monotonic `ExtensionInstanceId`s starting
/// at zero. Used during harness construction so each extension entry gets
/// a distinct id without a manually managed counter that's easy to leave
/// dangling when extensions are added or removed.
fn instance_id_factory() -> impl FnMut() -> tau_proto::ExtensionInstanceId {
    let mut counter: u64 = 0;
    move || {
        let iid = tau_proto::ExtensionInstanceId::new(counter);
        counter += 1;
        iid
    }
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
            Vec::new(),
            eager_session_id,
        )
    }

    pub(crate) fn new_with_agent(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        agent_runner: AgentRunner,
        tools: Vec<InProcessTool>,
        eager_session_id: &str,
    ) -> Result<Self, HarnessError> {
        let state_dir = state_dir.into();
        let (tx, rx) = mpsc::channel();
        let mut bus =
            EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(
                PolicyStore::open(policy_store_path_from(&state_dir))?,
            )));
        // Lazy: only the eager session's tree is needed up front
        // (loaded below via `store.load_session`); other sessions
        // load on first access. Avoids a startup walk over every
        // historical session dir.
        let store = SessionStore::open_lazy(&state_dir)?;

        let own_pid = std::process::id();
        let mut next_iid = instance_id_factory();

        let mut extensions = Vec::new();
        // Agent
        let (conn_id, thread) =
            spawn_in_process("agent", ClientKind::Agent, agent_runner, &mut bus, &tx)?;
        let agent_connection_id = conn_id.clone();
        extensions.push(ExtensionEntry {
            name: "agent".to_owned(),
            instance_id: next_iid(),
            connection_id: conn_id,
            kind: ClientKind::Agent,
            pid: Some(own_pid),
            in_process_thread: Some(thread),
            supervised_config: None,
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        });

        // Caller-supplied in-process tools.
        for tool in tools {
            let (conn_id, thread) =
                spawn_in_process(tool.name, ClientKind::Tool, tool.runner, &mut bus, &tx)?;
            extensions.push(ExtensionEntry {
                name: tool.name.to_owned(),
                instance_id: next_iid(),
                connection_id: conn_id,
                kind: ClientKind::Tool,
                pid: Some(own_pid),
                in_process_thread: Some(thread),
                supervised_config: None,
                restart_attempt: 0,
                state: ExtensionState::Spawning,
                last_acked: tau_proto::LogEventId::default(),
            });
        }

        let crate::model::LoadedModelList {
            available: available_models,
            selected: selected_model,
            model_registry,
            harness_settings,
            harness_settings_error,
            models_error,
        } = load_model_list(&dirs);
        crate::session_cleanup::spawn_session_cleanup(
            state_dir.clone(),
            harness_settings.session_retention(),
        );
        let selected_params = selected_model
            .as_ref()
            .map(|m| selected_params_for_model(&dirs, &harness_settings, &model_registry, m))
            .unwrap_or_default();

        let default_conversation_id = ConversationId::new("default");
        let mut store = store;
        let default_head = store
            .load_session(eager_session_id)?
            .and_then(|tree| tree.head());
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

        let extension_order: Vec<tau_proto::ConnectionId> =
            extensions.iter().map(|e| e.connection_id.clone()).collect();
        let extensions: std::collections::HashMap<tau_proto::ConnectionId, ExtensionEntry> =
            extensions
                .into_iter()
                .map(|e| (e.connection_id.clone(), e))
                .collect();
        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            current_session_id: eager_session_id.into(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tool_names: std::collections::HashMap::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            extension_order,
            next_session_prompt_id: 0,
            next_synthetic_call_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            conversations,
            default_conversation_id,
            turn_state: TurnState::Idle,
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
            available_models,
            selected_model,
            selected_params,
            context_input_tokens: None,
            context_cached_tokens: None,
            context_percent_used: None,
            token_usage: TokenUsageStats::default(),
            prompt_models: std::collections::HashMap::new(),
            model_registry,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            canceled_prompts: std::collections::HashSet::new(),
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

        let names: Vec<String> = harness
            .extension_order
            .iter()
            .filter_map(|id| harness.extensions.get(id).map(|e| e.name.clone()))
            .collect();
        for name in names {
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        harness.register_harness_tools();
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error, models_error);

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
        let startup_started_at = Instant::now();
        tracing::debug!(target: "tau_harness::startup", eager_session_id, "constructing harness from config");
        let state_dir = state_dir.into();
        let (tx, rx) = mpsc::channel();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening policy store");
        let policy_store = PolicyStore::open(policy_store_path_from(&state_dir))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "policy store opened");
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(policy_store),
        ));
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening session store");
        let store = SessionStore::open_lazy(&state_dir)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session store opened");

        let mut extensions = Vec::new();
        let mut next_iid = instance_id_factory();
        let mut agent_connection_id = None;

        for ext_config in config.extensions.values() {
            tracing::debug!(target: "tau_harness::startup", extension = %ext_config.name, elapsed_ms = startup_started_at.elapsed().as_millis(), "spawning extension");
            let kind = match ext_config.role.as_deref() {
                Some("agent") => ClientKind::Agent,
                _ => ClientKind::Tool,
            };

            let log_path =
                extension_stderr_log_path(&state_dir, eager_session_id, &ext_config.name);
            let (conn_id, child_pid) =
                spawn_supervised(ext_config, kind.clone(), Some(log_path), &mut bus, &tx)?;
            tracing::debug!(target: "tau_harness::startup", extension = %ext_config.name, pid = child_pid, elapsed_ms = startup_started_at.elapsed().as_millis(), "extension spawned");

            if kind == ClientKind::Agent {
                agent_connection_id = Some(conn_id.clone());
            }
            extensions.push(ExtensionEntry {
                name: ext_config.name.clone(),
                instance_id: next_iid(),
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

        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "loading model list");
        let crate::model::LoadedModelList {
            available: available_models,
            selected: selected_model,
            model_registry,
            harness_settings,
            harness_settings_error,
            models_error,
        } = load_model_list(&dirs);
        tracing::debug!(target: "tau_harness::startup", selected_model = ?selected_model, elapsed_ms = startup_started_at.elapsed().as_millis(), "model list loaded");
        crate::session_cleanup::spawn_session_cleanup(
            state_dir.clone(),
            harness_settings.session_retention(),
        );
        let selected_params = selected_model
            .as_ref()
            .map(|m| selected_params_for_model(&dirs, &harness_settings, &model_registry, m))
            .unwrap_or_default();

        let default_conversation_id = ConversationId::new("default");
        let mut store = store;
        let default_head = store
            .load_session(eager_session_id)?
            .and_then(|tree| tree.head());
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

        let extension_order: Vec<tau_proto::ConnectionId> =
            extensions.iter().map(|e| e.connection_id.clone()).collect();
        let extensions: std::collections::HashMap<tau_proto::ConnectionId, ExtensionEntry> =
            extensions
                .into_iter()
                .map(|e| (e.connection_id.clone(), e))
                .collect();
        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            current_session_id: eager_session_id.into(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tool_names: std::collections::HashMap::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            extension_order,
            next_session_prompt_id: 0,
            next_synthetic_call_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            conversations,
            default_conversation_id,
            turn_state: TurnState::Idle,
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
            available_models,
            selected_model,
            selected_params,
            context_input_tokens: None,
            context_cached_tokens: None,
            context_percent_used: None,
            token_usage: TokenUsageStats::default(),
            prompt_models: std::collections::HashMap::new(),
            model_registry,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            canceled_prompts: std::collections::HashSet::new(),
            dirs,
        };

        let _ = harness.enable_debug_log(&state_dir.join(eager_session_id))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "debug event log enabled");
        // Record cwd in meta.json so `-r` (resume most recent for this
        // cwd) can find this session even before it has any log entries.
        // Also acquires the flock on `<state_dir>/<eager_session_id>/lock`.
        harness
            .store
            .record_session_meta(eager_session_id, std::env::current_dir().ok())?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session metadata recorded");

        let names: Vec<String> = harness
            .extension_order
            .iter()
            .filter_map(|id| harness.extensions.get(id).map(|e| e.name.clone()))
            .collect();
        for name in names {
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "extensions ready");
        harness.register_harness_tools();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness tools registered");
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error, models_error);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "config checks complete");

        harness.start_session_init(
            eager_session_id.into(),
            tau_proto::SessionStartReason::Initial,
        );
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init started");
        harness.wait_for_session_init()?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init complete");
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

    /// Conversation id that owns a given in-flight prompt, if any.
    fn conversation_for_prompt(&self, spid: &SessionPromptId) -> Option<ConversationId> {
        self.prompt_conversations.get(spid).cloned()
    }

    /// If the conversation's dedup map's "built for" cursor doesn't
    /// match its current `head`, rebuild it from the assembled branch.
    /// O(branch_len) on rebuild; O(1) on the steady-state hot path
    /// where the linear-extension hook in [`Self::commit_event`] keeps
    /// `built_for` in sync after every fold.
    ///
    /// `None` is returned only if the conversation no longer exists
    /// (the caller raced its own teardown), and the caller treats that
    /// as "skip dedup, just publish".
    fn ensure_dedup_built_for_branch(&mut self, cid: &ConversationId) -> Option<()> {
        let head = self.conversations.get(cid)?.head;
        let needs = self
            .conversations
            .get(cid)
            .map(|c| c.result_dedup.needs_rebuild(head))
            .unwrap_or(false);
        if !needs {
            return Some(());
        }
        // Walk the branch under an immutable borrow of the store, then
        // hand the snapshot to the conversation under a mut borrow —
        // the branch iterator borrows the tree, so we materialize it
        // into an owned Vec first to release the tree borrow.
        let session_id = self.conversations.get(cid)?.session_id.clone();
        let branch: Vec<tau_core::SessionEntry> = self
            .store
            .session(session_id.as_str())
            .map(|t| t.branch_from(head).into_iter().cloned().collect())
            .unwrap_or_default();
        let conv = self.conversations.get_mut(cid)?;
        conv.result_dedup
            .rebuild_from_branch(branch.iter(), head, DEFAULT_THRESHOLD_BYTES);
        Some(())
    }

    /// Replace `result.result` with a pointer if a previous tool
    /// result on this conversation's branch has the same content.
    /// Mutates `result` in place; the caller publishes the (possibly
    /// modified) value, which is what gets folded into the tree and
    /// what the LLM sees on the next turn.
    fn dedup_tool_result(&mut self, cid: &ConversationId, result: &mut tau_proto::ToolResult) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_for_hash(&result.result);
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.conversations.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            // Belt-and-suspenders: refuse to point a call at itself.
            // This can't happen in practice — `tool_conversations`
            // already drops the call_id between intake and now — but
            // a future change to the tracking map could let a tool
            // result re-enter this path twice, and self-pointing is a
            // worse failure mode than just skipping the dedup.
            if original_call_id == result.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %result.tool_name,
                call_id = %result.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool result against earlier identical output"
            );
            result.result = build_pointer_value(&original_call_id, &result.tool_name);
        } else {
            conv.result_dedup.insert(hash, result.call_id.clone());
        }
    }

    /// Companion to [`Self::dedup_tool_result`] for `ToolError`s.
    /// Same semantics — collapses repeated identical errors (same
    /// message, same `details`) into a pointer back to the first
    /// occurrence on this branch.
    fn dedup_tool_error(&mut self, cid: &ConversationId, error: &mut tau_proto::ToolError) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_error_for_hash(&error.message, error.details.as_ref());
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.conversations.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            if original_call_id == error.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %error.tool_name,
                call_id = %error.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool error against earlier identical output"
            );
            error.message = build_pointer_error_message(&original_call_id, &error.tool_name);
            error.details = None;
        } else {
            conv.result_dedup.insert(hash, error.call_id.clone());
        }
    }

    /// Publishes an event for a specific conversation. The fold uses
    /// the conversation's `head` as the explicit parent — no more
    /// `UiNavigateTree` head-bouncing — and the post-commit hook in
    /// [`Harness::commit_event`] keeps `c.head` in sync with the
    /// freshly-folded node.
    ///
    /// This helper is what makes branching prompts work: the default
    /// (user) conversation can keep advancing while a side conversation
    /// from an extension grows its own branch off some earlier node;
    /// each side publish brackets its own navigate-then-append.
    pub(crate) fn publish_for_conversation(&mut self, cid: &ConversationId, event: Event) {
        self.publish_for_conversation_from(cid, None, event);
    }

    /// Like [`publish_for_conversation`] but lets the caller record an
    /// originating connection on the persisted record (for `tool.result`
    /// / `tool.error` arriving from extensions). The snap-to-`cid`-head
    /// step is what keeps cross-conversation tool activity from folding
    /// onto the wrong tree branch — without it, a sibling side conv that
    /// just navigated `tree.head` would steal the parent of the next
    /// tree-folding event.
    fn publish_for_conversation_from(
        &mut self,
        cid: &ConversationId,
        source: Option<&str>,
        event: Event,
    ) {
        // Stamp the publish with `cid`. The fold reads the
        // conversation's `head` as the explicit parent node in
        // `commit_event`, so cross-conversation publishes no longer
        // need a `UiNavigateTree` round-trip to bounce the global
        // write cursor. After the commit, the post-commit hook
        // also syncs `c.head` automatically — the trailing
        // read-tree-and-update idiom is gone entirely.
        //
        // Re-stamp tool events with the owning conversation's
        // originator so subscribers can tell main-agent tool
        // activity from sub-agent tool activity without having to
        // map `call_id` back to a conversation themselves. Construction
        // sites can leave `originator` as the default — this is the
        // single point of truth.
        let event =
            if let Some(originator) = self.conversations.get(cid).map(|c| c.originator.clone()) {
                stamp_tool_event_originator(event, originator)
            } else {
                event
            };
        self.publish_event_for_conversation(cid, source, event);
    }

    /// Publishes an event to both the event bus and the event log.
    /// Convenience wrapper that uses the event's default transience
    /// and never marks the publish as `must_pass`.
    pub(crate) fn publish_event(&mut self, source: Option<&str>, event: Event) {
        let transient = event.defaults_to_transient();
        self.enqueue_publish(source, event, transient, false, None);
    }

    /// Like [`Harness::publish_event`] but tags the publish with the
    /// originating conversation. After the event commits, the
    /// harness syncs that conversation's cached `head` to the
    /// freshly-folded `tree.head()` — so callers don't need to read
    /// the tree themselves (which would race the interception chain
    /// when a publish parks).
    fn publish_event_for_conversation(
        &mut self,
        cid: &ConversationId,
        source: Option<&str>,
        event: Event,
    ) {
        if !self.conversations.contains_key(cid) {
            // The conversation was torn down between when the
            // caller looked it up and now (e.g. side conv that
            // raced its own teardown with a late tool result).
            // Fall back to a plain publish so the event still
            // reaches the bus / log; we just can't stamp a parent
            // for it.
            tracing::warn!(
                target: "tau_harness",
                event = %event.name(),
                cid = %cid,
                "publish_event_for_conversation called with unknown cid; \
                 publishing without parent stamp",
            );
            self.publish_event(source, event);
            return;
        }
        let transient = event.defaults_to_transient();
        let sync = Some(ConversationHeadSync { cid: cid.clone() });
        self.enqueue_publish(source, event, transient, false, sync);
    }

    /// Final commit: persist (when applicable), append to the event
    /// log, and broadcast on the bus. Does not consult interception
    /// state — the caller is responsible for getting here only when
    /// the chain has resolved. Triggers any post-commit reactions
    /// (currently: deferred agent dispatches that were waiting on
    /// this user-message-bearing event to land in the tree, plus
    /// per-publish conversation `head` syncs).
    pub(crate) fn commit_event(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        // When this publish was stamped with a conversation, fold
        // the event onto that conversation's branch directly. This
        // skips the `UiNavigateTree` head-bouncing dance that
        // `publish_for_conversation_from` used to do — the explicit
        // parent in `apply_event_at` does the same job without
        // touching the global cursor.
        let parent_for_fold = if sync_head_for.as_ref().is_some_and(|s| {
            self.conversations
                .get(&s.cid)
                .is_some_and(|c| c.head.is_none())
        }) {
            Some(None)
        } else {
            sync_head_for
                .as_ref()
                .and_then(|s| self.conversations.get(&s.cid).and_then(|c| c.head))
                .map(Some)
        };
        // Stamp once and share with every downstream observer: the
        // durable record on disk, the in-memory event log entry, and
        // the wire `LogEvent` envelope. Sampling the clock three
        // separate times would let timing analyses disagree with what
        // live subscribers saw.
        let source_id = source.map(tau_proto::ConnectionId::from);
        let (seq, recorded_at) = self.event_log.append(source_id.clone(), event.clone());
        // Mirror every committed event into the JSONL debug log as a
        // `published` line. The inbound `from_connection` lines carry
        // the raw frame the agent sent us, but for events that the
        // harness enriches (notably `AgentResponseFinished`, where
        // `token_usage` is built here from session-wide state the
        // agent never sees), the enriched payload only exists on the
        // outbound copy. Offline cache/cost analysis tools that read
        // `events.jsonl` would otherwise see zeros where the running
        // session totals belong.
        if let Some(log) = &mut self.debug_log {
            log.log_published_event(source_id.as_ref(), &event, recorded_at);
        }
        let folded_node_id =
            self.persist_session_event(source, &event, transient, parent_for_fold, recorded_at);
        if let Some(sync) = sync_head_for
            && let Some(node_id) = folded_node_id
            && let Some(c) = self.conversations.get_mut(&sync.cid)
        {
            // Only advance the conversation's own branch cursor when
            // the event produced a tree node. `tree.head()` is the
            // *global* write cursor and may sit on a sibling
            // conversation's last fold; syncing to it after a
            // non-folding event (e.g. `AgentResponseFinished` with
            // only tool calls) would graft this conversation's next
            // tool request onto the wrong branch and produce orphan
            // ToolUse blocks downstream.
            c.head = Some(node_id);
            // Keep the dedup map's "built for" cursor in lockstep with
            // the just-folded linear extension. The dedup-decision
            // path already inserted any new (hash, call_id) entry
            // before the publish, so the map's contents already match
            // what a fresh rebuild from this new head would produce.
            // Bumping the cursor here lets the next tool result skip
            // the rebuild entirely (the steady-state hot path).
            //
            // We pass *every* fold through this hook, including ones
            // that didn't touch the dedup map (a `UserMessage` from
            // session re-init, an `AgentMessage`, a `ToolRequest`
            // node). [`ResultDedupMap::note_head_advanced_to`] guards
            // against the dangerous case — `built_for == None` plus a
            // non-dedup-eligible fold — by skipping the bump, so the
            // rebuild still triggers on the next dedup intake. Don't
            // gate this call on the event variant: that would re-couple
            // `commit_event` to per-tool semantics that the dedup
            // module deliberately owns.
            c.result_dedup.note_head_advanced_to(node_id);
        }
        // Wrap in a `LogEvent` message envelope so subscribers get the
        // id and can ack after processing. Receivers that don't care
        // (UIs) call `Frame::peel_log()` and discard the id.
        let log_frame = Frame::Message(Message::LogEvent(tau_proto::LogEvent {
            id: tau_proto::LogEventId::new(seq),
            recorded_at,
            event: Box::new(event.clone()),
        }));
        let _ = self.bus.publish_from(source, log_frame);
        self.react_to_committed_event(&event);
    }

    /// Post-commit reactions. Drains the deferred-agent-dispatch
    /// queue when a user-message-bearing event commits, so the
    /// agent prompt assembled in `send_prompt_to_agent_for` sees
    /// the just-folded user message. The `c.head` sync that this
    /// dispatch depends on is handled inside `commit_event` for any
    /// publish stamped via `publish_event_for_conversation`.
    fn react_to_committed_event(&mut self, event: &Event) {
        let folds_user_message = matches!(
            event,
            Event::UiPromptSubmitted(_)
                | Event::SessionUserMessageInjected(_)
                | Event::SessionPromptSteered(_)
        );
        if !folds_user_message {
            return;
        }
        let Some(cid) = self.pending_user_prompt_dispatches.pop_front() else {
            return;
        };
        if !self.conversations.contains_key(&cid) {
            // Conversation was torn down while the prompt was in
            // limbo (e.g. side query that timed out).
            return;
        }
        self.send_prompt_to_agent_for(&cid);
    }

    /// Persists `event` to the durable per-session log and folds it
    /// into the in-memory tree. Returns the id of the just-folded
    /// node when the event produced one (e.g. `UserMessage`,
    /// `ToolActivity`, an `AgentMessage` from a finished response
    /// with text), or `None` for transient / non-folding events. The
    /// caller uses the returned node id to sync a per-conversation
    /// branch cursor without consulting the tree's *global* head,
    /// which can be on a sibling conversation's branch.
    fn persist_session_event(
        &mut self,
        source: Option<&str>,
        event: &Event,
        transient: bool,
        parent_node_id: Option<Option<tau_proto::NodeId>>,
        recorded_at: tau_proto::UnixMicros,
    ) -> Option<tau_proto::NodeId> {
        if transient {
            return None;
        }
        let session_id = self.session_id_for_event(event)?;
        let source = source.map(tau_proto::ConnectionId::from);
        self.store
            .append_session_event_at(
                session_id.as_str(),
                source,
                parent_node_id,
                event.clone(),
                recorded_at,
            )
            .ok()
            .and_then(|outcome| outcome.folded_node_id)
    }

    fn session_id_for_event(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::UiPromptSubmitted(prompt) => Some(prompt.session_id.clone()),
            Event::UiShellCommand(command) => Some(command.session_id.clone()),
            Event::UiSwitchSession(req) => Some(req.new_session_id.clone()),
            Event::UiTreeRequest(req) => Some(req.session_id.clone()),
            Event::UiNavigateTree(req) => Some(req.session_id.clone()),
            Event::UiCancelPrompt(req) => Some(req.session_id.clone()),
            Event::SessionPromptQueued(queued) => Some(queued.session_id.clone()),
            Event::SessionPromptSteered(steered) => Some(steered.session_id.clone()),
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
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    self.handle_extension_event(&connection_id, *frame)?;
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
    /// can also gate runtime dispatch in `dispatch_blocked_for`.
    fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        if self.extensions_all_ready() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.extensions_all_ready() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    self.handle_extension_event(&connection_id, *frame)?;
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
            // `tau` behave like a normal foreground command.
            // Before any UI attaches we wait — otherwise a slightly
            // late first connect would race us into immediate exit.
            if exit_on_disconnect && ever_attached && self.client_writers.is_empty() {
                break;
            }
            let Ok(harness_evt) = self.rx.recv() else {
                break;
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    let origin = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.origin.clone());
                    match origin {
                        Some(ConnectionOrigin::Socket) => {
                            // `/detach` → stay alive even after this
                            // UI leaves; a later `tau --attach`
                            // can pick up right here.
                            if matches!(frame.as_ref(), Frame::Event(Event::UiDetachRequest(_))) {
                                exit_on_disconnect = false;
                            }
                            let keep = self.handle_client_event(&connection_id, *frame)?;
                            if !keep {
                                let _ = self.bus.disconnect(&connection_id);
                                served_clients += 1;
                            }
                        }
                        Some(_) => self.handle_extension_event(&connection_id, *frame)?,
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
        frame: Frame,
    ) -> Result<(), HarnessError> {
        match frame {
            Frame::Message(msg) => self.handle_extension_message(source_id, msg),
            Frame::Event(event) => self.handle_extension_event_inner(source_id, event),
        }
    }

    fn handle_extension_message(
        &mut self,
        source_id: &str,
        message: Message,
    ) -> Result<(), HarnessError> {
        match message {
            Message::Ack(ack) => {
                // Cumulative ack: advance the cursor if it moves
                // forward, ignore otherwise (duplicates, late acks).
                if let Some(entry) = self.extensions.get_mut(source_id)
                    && ack.up_to.get() > entry.last_acked.get()
                {
                    entry.last_acked = ack.up_to;
                }
            }
            Message::Hello(_hello) => {
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.send_lifecycle_configure(source_id);
            }
            Message::ConfigError(err) => {
                let name = self
                    .extensions
                    .get(source_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| "extension".to_owned());
                self.emit_info_important(&format!(
                    "extension {name} rejected its config: {}\nthe value of \
                     `extensions.{name}.config` in harness.json5 is being ignored",
                    err.message,
                ));
            }
            Message::Subscribe(subscribe) => {
                self.bus.set_subscriptions(source_id, subscribe.selectors)?;
            }
            Message::Intercept(intercept) => {
                let component_name = self
                    .bus
                    .connection(source_id)
                    .map(|m| ExtensionName::from(m.name.clone()))
                    .unwrap_or_else(|| ExtensionName::from(source_id.to_owned()));
                self.interceptors.replace_for_connection(
                    source_id,
                    component_name,
                    intercept.selectors,
                    intercept.priority,
                );
            }
            Message::Ready(_ready) => {
                self.emit_extension_ready(source_id);
                self.set_extension_state(source_id, ExtensionState::Ready);
                self.try_advance_queue();
            }
            Message::Emit(emit) => {
                self.enqueue_publish(Some(source_id), *emit.event, emit.transient, false, None);
            }
            Message::InterceptReply(reply) => {
                self.handle_intercept_reply(source_id, reply);
            }
            // Messages sent by the harness only — extensions shouldn't
            // round-trip these. Ignore silently.
            Message::Configure(_)
            | Message::Disconnect(_)
            | Message::InterceptRequest(_)
            | Message::LogEvent(_) => {}
        }
        Ok(())
    }

    fn handle_extension_event_inner(
        &mut self,
        source_id: &str,
        event: Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::ToolRegister(ToolRegister { tool }) => {
                let _ = self.registry.register(source_id, tool);
            }
            Event::ToolRequest(request) => {
                // Track session attribution before publishing — the
                // publish path's `session_id_for_event` reads
                // `pending_tool_sessions` to attach the persisted
                // record to the right session.
                self.track_tool_request_session(&request);
                // Stamp the publish with the owning conversation so
                // the fold lands on its branch. Without this, after
                // Phase 4 of the interception refactor (no global
                // head-bouncing), a sibling conversation that
                // recently appended would leave `tree.head` on its
                // own tip, and the tool-request node would fold
                // there instead.
                let owning_cid = self.tool_conversations.get(&request.call_id).cloned();
                let event = Event::ToolRequest(request.clone());
                match owning_cid {
                    Some(cid) => self.publish_event_for_conversation(&cid, Some(source_id), event),
                    None => self.publish_event(Some(source_id), event),
                }
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
                            display: None,
                            originator: tau_proto::PromptOriginator::User,
                        };
                        match owning_cid {
                            Some(cid) => self.publish_event_for_conversation(
                                &cid,
                                None,
                                Event::ToolError(error),
                            ),
                            None => self.publish_event(None, Event::ToolError(error)),
                        }
                        self.clear_tool_call_tracking(&call_id);
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(mut result) => {
                if let Some(cid) = self.tool_conversations.get(&result.call_id).cloned() {
                    let call_id = result.call_id.to_string();
                    // Collapse byte-identical large results into a
                    // pointer back to the first call_id that produced
                    // this content on this conversation's branch. See
                    // `crate::dedup` for the design.
                    self.dedup_tool_result(&cid, &mut result);
                    // Snap to the owning conversation's head before
                    // folding the result. Without this, a sibling side
                    // conv that just ran `snap_to_default_conversation`
                    // (during its teardown) leaves `tree.head` on the
                    // *parent* branch — folding the result there
                    // misplaces it and produces orphan ToolUse blocks
                    // when the parent conv is later re-prompted.
                    self.publish_for_conversation_from(
                        &cid,
                        Some(source_id),
                        Event::ToolResult(result),
                    );
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool result for call_id={}",
                        result.call_id
                    ));
                }
            }
            Event::ToolError(mut error) => {
                if let Some(cid) = self.tool_conversations.get(&error.call_id).cloned() {
                    let call_id = error.call_id.to_string();
                    self.dedup_tool_error(&cid, &mut error);
                    self.publish_for_conversation_from(
                        &cid,
                        Some(source_id),
                        Event::ToolError(error),
                    );
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
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

    fn handle_client_event(&mut self, client_id: &str, frame: Frame) -> Result<bool, HarnessError> {
        match frame {
            Frame::Message(msg) => self.handle_client_message(client_id, msg),
            Frame::Event(event) => self.handle_client_event_inner(client_id, event),
        }
    }

    fn handle_client_message(
        &mut self,
        client_id: &str,
        message: Message,
    ) -> Result<bool, HarnessError> {
        match message {
            Message::Hello(_hello) => Ok(true),
            Message::Subscribe(subscribe) => {
                // Policy check via the bus.
                match self
                    .bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())
                {
                    Ok(()) => {
                        let selectors_for_replay = subscribe.selectors;
                        self.replay_session_events(client_id, &selectors_for_replay);
                        self.replay_harness_info(client_id, &selectors_for_replay);
                        Ok(true)
                    }
                    Err(RouteError::SubscriptionDenied { reason, .. }) => {
                        let _ = self.bus.send_to(
                            client_id,
                            None,
                            Frame::Message(Message::Disconnect(Disconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            })),
                        );
                        Ok(false)
                    }
                    Err(other) => Err(HarnessError::Route(other)),
                }
            }
            Message::Disconnect(_) => Ok(false),
            // Other messages from clients are ignored (Configure, Ack,
            // LogEvent, InterceptRequest, InterceptReply, Emit,
            // ConfigError, Intercept).
            Message::Ack(_)
            | Message::Configure(_)
            | Message::ConfigError(_)
            | Message::Intercept(_)
            | Message::InterceptRequest(_)
            | Message::InterceptReply(_)
            | Message::Ready(_)
            | Message::LogEvent(_)
            | Message::Emit(_) => Ok(true),
        }
    }

    fn handle_client_event_inner(
        &mut self,
        client_id: &str,
        event: Event,
    ) -> Result<bool, HarnessError> {
        match event {
            Event::UiModelSelect(select) => {
                if self.available_models.contains(&select.model) {
                    let was_empty = self.selected_model.is_none();
                    let model = select.model.clone();
                    self.selected_model = Some(model.clone());
                    let (live_settings, _) = load_harness_settings_or_warn(&self.dirs);
                    self.selected_params = selected_params_for_model(
                        &self.dirs,
                        &live_settings,
                        &self.model_registry,
                        &model,
                    );
                    save_harness_state(&self.dirs, Some(&model), self.selected_params);
                    self.context_input_tokens = None;
                    self.context_cached_tokens = None;
                    self.context_percent_used = None;
                    let context_window = model_context_window(&self.model_registry, &model);
                    let effort_levels = efforts_for_model(&self.model_registry, &model);
                    let verbosity_levels = verbosities_for_model(&self.model_registry, &model);
                    let thinking_levels =
                        thinking_summaries_for_model(&self.model_registry, &model);
                    self.publish_event(
                        None,
                        Event::HarnessModelSelected(HarnessModelSelected {
                            model: Some(model),
                            context_window,
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
                            level: self.selected_params.effort,
                        }),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                            levels: effort_levels,
                        }),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessVerbosityChanged(tau_proto::HarnessVerbosityChanged {
                            level: self.selected_params.verbosity,
                        }),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessVerbositiesAvailable(
                            tau_proto::HarnessVerbositiesAvailable {
                                levels: verbosity_levels,
                            },
                        ),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessThinkingSummaryChanged(
                            tau_proto::HarnessThinkingSummaryChanged {
                                level: self.selected_params.thinking_summary,
                            },
                        ),
                    );
                    self.publish_event(
                        None,
                        Event::HarnessThinkingSummariesAvailable(
                            tau_proto::HarnessThinkingSummariesAvailable {
                                levels: thinking_levels,
                            },
                        ),
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
                let levels = self
                    .selected_model
                    .as_ref()
                    .map(|m| efforts_for_model(&self.model_registry, m))
                    .unwrap_or_default();
                let clamped = clamp_effort(req.level, &levels);
                if clamped != req.level {
                    let model_label = self
                        .selected_model
                        .as_ref()
                        .map(ModelId::to_string)
                        .unwrap_or_else(|| "(no model)".to_owned());
                    self.publish_event(
                        None,
                        Event::HarnessInfo(tau_proto::HarnessInfo {
                            message: format!(
                                "effort `{}` not supported by `{model_label}`; using `{}` instead",
                                req.level.as_str(),
                                clamped.as_str(),
                            ),
                            level: tau_proto::HarnessInfoLevel::Normal,
                        }),
                    );
                }
                self.selected_params.effort = clamped;
                save_harness_state(
                    &self.dirs,
                    self.selected_model.as_ref(),
                    self.selected_params,
                );
                self.publish_event(
                    None,
                    Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
                        level: self.selected_params.effort,
                    }),
                );
                Ok(true)
            }
            Event::UiSetVerbosity(req) => {
                let levels = self
                    .selected_model
                    .as_ref()
                    .map(|m| verbosities_for_model(&self.model_registry, m))
                    .unwrap_or_default();
                let clamped = clamp_verbosity(req.level, &levels);
                if clamped != req.level {
                    let model_label = self
                        .selected_model
                        .as_ref()
                        .map(ModelId::to_string)
                        .unwrap_or_else(|| "(no model)".to_owned());
                    self.publish_event(
                        None,
                        Event::HarnessInfo(tau_proto::HarnessInfo {
                            message: format!(
                                "verbosity `{}` not supported by `{model_label}`; using `{}` instead",
                                req.level.as_str(),
                                clamped.as_str(),
                            ),
                            level: tau_proto::HarnessInfoLevel::Normal,
                        }),
                    );
                }
                self.selected_params.verbosity = clamped;
                save_harness_state(
                    &self.dirs,
                    self.selected_model.as_ref(),
                    self.selected_params,
                );
                self.publish_event(
                    None,
                    Event::HarnessVerbosityChanged(tau_proto::HarnessVerbosityChanged {
                        level: self.selected_params.verbosity,
                    }),
                );
                Ok(true)
            }
            Event::UiSetThinkingSummary(req) => {
                let levels = self
                    .selected_model
                    .as_ref()
                    .map(|m| thinking_summaries_for_model(&self.model_registry, m))
                    .unwrap_or_default();
                let clamped = clamp_thinking_summary(req.level, &levels);
                if clamped != req.level {
                    let model_label = self
                        .selected_model
                        .as_ref()
                        .map(ModelId::to_string)
                        .unwrap_or_else(|| "(no model)".to_owned());
                    self.publish_event(
                        None,
                        Event::HarnessInfo(tau_proto::HarnessInfo {
                            message: format!(
                                "thinking summary `{}` not supported by `{model_label}`; using `{}` instead",
                                req.level.as_str(),
                                clamped.as_str(),
                            ),
                            level: tau_proto::HarnessInfoLevel::Normal,
                        }),
                    );
                }
                self.selected_params.thinking_summary = clamped;
                save_harness_state(
                    &self.dirs,
                    self.selected_model.as_ref(),
                    self.selected_params,
                );
                self.publish_event(
                    None,
                    Event::HarnessThinkingSummaryChanged(
                        tau_proto::HarnessThinkingSummaryChanged {
                            level: self.selected_params.thinking_summary,
                        },
                    ),
                );
                Ok(true)
            }
            Event::UiPromptSubmitted(prompt) => {
                // Stash the correlation tag on the default conversation
                // before submission; `send_prompt_to_agent_for` will
                // consume it when it constructs the matching
                // `SessionPromptCreated`. Queued prompts drop the tag
                // (the queue stores text only) — the daemon helper
                // only exercises the synchronous-dispatch path.
                if let Some(c) = self.conversations.get_mut(&self.default_conversation_id) {
                    c.next_ctx_id = prompt.ctx_id.clone();
                }
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
                    if self.selected_model.is_none() {
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
            Event::UiCancelPrompt(req) => {
                self.handle_cancel_prompt(&req.session_id);
                Ok(true)
            }
            other => {
                self.publish_event(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn handle_cancel_prompt(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        let cid = self.default_conversation_id.clone();
        let Some(conv) = self.conversations.get_mut(&cid) else {
            return;
        };
        let Some(prompt_id) = conv.in_flight_prompt.take() else {
            self.emit_info("no in-flight prompt to cancel");
            return;
        };
        self.canceled_prompts.insert(prompt_id.clone());
        self.prompt_conversations.remove(&prompt_id);
        conv.pending_prompts.clear();
        conv.turn_state = ConversationTurnState::Idle;

        let pending_call_ids: std::collections::HashSet<ToolCallId> = self
            .pending_tool_invocations
            .iter()
            .filter_map(|(call_cid, call, _)| {
                if call_cid == &cid {
                    Some(call.id.clone())
                } else {
                    None
                }
            })
            .collect();
        self.pending_tool_invocations
            .retain(|(call_cid, _, _)| call_cid != &cid);
        for call_id in pending_call_ids {
            self.clear_tool_call_tracking(call_id.as_str());
        }

        self.emit_info("cancelled current prompt");
        self.publish_event(
            None,
            Event::UiCancelPrompt(UiCancelPrompt {
                session_id: session_id.clone(),
            }),
        );
        self.try_advance_queue();
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
        self.remove_discovered_context(connection_id);
        self.interceptors.remove_connection(connection_id);
        self.fail_pending_intercept_for_disconnect(connection_id);
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
                .unwrap_or_else(|| ToolName::new("unknown_tool"));
            let error = ToolError {
                call_id: call_id.clone(),
                tool_name,
                message: "tool provider disconnected".to_owned(),
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            // Publish on the owning conversation's branch so the
            // synthesized failure folds onto the right node. Without
            // the snap, sibling side conversations could leave
            // `tree.head` on the wrong branch and the fold would land
            // there instead. `on_tool_call_complete` then folds the
            // failed call into its conversation's `ToolsRunning` set
            // and re-prompts the agent if the turn is now done.
            // Tracking maps are cleared *after* that read.
            if let Some(cid) = self.tool_conversations.get(call_id.as_str()).cloned() {
                self.publish_for_conversation(&cid, Event::ToolError(error));
            } else {
                // No conversation attribution — fall back to the
                // unsnapped publish so the error still reaches the
                // bus / log.
                self.publish_event(None, Event::ToolError(error));
            }
            self.on_tool_call_complete(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    fn try_respawn_supervised_extension(
        &mut self,
        connection_id: &str,
    ) -> Result<(), HarnessError> {
        let Some(entry) = self.extensions.get_mut(connection_id) else {
            return Ok(());
        };
        let Some(config) = entry.supervised_config.clone() else {
            return Ok(());
        };
        if entry.kind == ClientKind::Agent {
            return Ok(());
        }

        entry.restart_attempt += 1;
        let attempt = entry.restart_attempt;
        let instance_id = entry.instance_id;
        let name = entry.name.clone();
        let kind = entry.kind.clone();
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

        let log_path = extension_stderr_log_path(
            &self.dirs_state_dir(),
            self.current_session_id.as_str(),
            &config.name,
        );
        let (new_connection_id, child_pid) =
            spawn_supervised(&config, kind, Some(log_path), &mut self.bus, &self.tx)?;

        // Re-key the entry under the freshly-minted connection id and
        // patch its in-place state. The `extension_order` slot stays in
        // place so spawn-order semantics survive the respawn.
        let old_key = tau_proto::ConnectionId::from(connection_id);
        let mut moved = self
            .extensions
            .remove(&old_key)
            .expect("entry was present moments ago");
        moved.connection_id = new_connection_id.clone();
        moved.pid = Some(child_pid);
        moved.state = ExtensionState::Spawning;
        moved.last_acked = tau_proto::LogEventId::default();
        self.extensions.insert(new_connection_id.clone(), moved);
        if let Some(slot) = self.extension_order.iter_mut().find(|id| **id == old_key) {
            *slot = new_connection_id;
        }
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
    pub(crate) fn clear_tool_call_tracking(&mut self, call_id: &str) {
        self.tool_conversations.remove(call_id);
        self.pending_tool_names.remove(call_id);
        self.pending_tool_providers.remove(call_id);
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    fn find_extension_by_name(&self, name: &str) -> Option<&ExtensionEntry> {
        self.extensions.values().find(|e| e.name == name)
    }

    fn find_extension_by_connection(&self, connection_id: &str) -> Option<&ExtensionEntry> {
        self.extensions.get(connection_id)
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

    /// Surface settings-file parse errors captured during the initial
    /// load as `Important` `HarnessInfo`. The loaders already fell
    /// back to defaults and wrote a short stderr line, but stderr is
    /// hidden once the TUI takes over the terminal — without this the
    /// user's only symptom is "my extensions vanished" / "my provider
    /// list is empty" with no clue why.
    ///
    /// Taking the errors as parameters (instead of re-parsing each
    /// file here) keeps startup to a single parse per file and avoids
    /// a race where the user fixes the file between the two reads.
    ///
    /// `cli.json5` is intentionally not handled here: the CLI fails
    /// fast on a malformed `cli.json5` before the harness ever
    /// spawns, so there's no "silently fell back to defaults" case
    /// to surface.
    fn emit_startup_settings_errors(
        &mut self,
        harness_settings_error: Option<tau_config::settings::SettingsError>,
        models_error: Option<tau_config::settings::SettingsError>,
    ) {
        if let Some(error) = harness_settings_error {
            self.emit_info_important(&format!(
                "harness.json5 failed to parse — ignored.\n{error}"
            ));
        }
        if let Some(error) = models_error {
            self.emit_info_important(&format!("models.json5 failed to parse — ignored.\n{error}"));
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
            .get(source_id)
            .and_then(|e| e.supervised_config.as_ref())
            .map(|cfg| cfg.config.clone())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let _ = self.bus.send_to(
            source_id,
            None,
            Frame::Message(Message::Configure(tau_proto::Configure {
                config: tau_proto::json_to_cbor(&config_json),
            })),
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

    pub(crate) fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    /// Spawn a fresh side conversation for an extension's
    /// [`tau_proto::ExtAgentQuery`] and dispatch it. The harness has no
    /// global agent slot — the side conversation publishes its own
    /// `SessionPromptCreated` immediately, and the agent extension
    /// serializes consumption from the event log.
    ///
    /// Two forking modes depending on whether the query is tool-backed:
    ///
    /// - **Tool-backed (`tool_call_id: Some(...)`, e.g. `delegate`)**: the
    ///   sub-agent starts with a *fresh* context — only the delegated
    ///   instruction, no inherited messages from the parent (no user framing,
    ///   no completed prior turns, no in-flight tool blocks). The parent agent
    ///   is responsible for putting everything the sub-agent needs into the
    ///   `prompt`. This applies uniformly at any nesting depth so deeper
    ///   sub-agents can't see (and restage) ancestor task framing.
    ///
    /// - **Non-tool (`tool_call_id: None`, e.g. notifications' idle summary)**:
    ///   the side conv inherits the parent conversation's current head (and
    ///   `chain_anchor`, if any) so the assembled prompt actually contains the
    ///   user's recent history. The whole point of this flow is to summarize
    ///   what the user/agent were doing — that needs the conversation it is
    ///   summarizing. Sharing the prefix also lets prompt caching reuse the
    ///   parent's cached transcript verbatim, since the only delta is the
    ///   appended instruction.
    fn handle_ext_agent_query(
        &mut self,
        source_id: &str,
        query: tau_proto::ExtAgentQuery,
    ) -> Result<(), HarnessError> {
        let extension_name = self
            .extensions
            .get(source_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| source_id.to_owned());

        let parent_call_id = query.tool_call_id.clone();
        let task_name = query.task_name.clone();

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

        // Resolve the parent conversation: for tool-backed queries it's
        // the conv that owns the tool_call_id; for non-tool queries it's
        // the default (user) conv. Both modes share the parent's
        // session id so the side branch lands in the same session.
        let parent_cid = query
            .tool_call_id
            .as_ref()
            .and_then(|call_id| self.tool_conversations.get(call_id))
            .cloned()
            .unwrap_or_else(|| self.default_conversation_id.clone());
        let parent_conv = self
            .conversations
            .get(&parent_cid)
            .expect("parent conversation always present");
        let session_id = parent_conv.session_id.clone();
        // Tool-backed: fresh fork (head=None). Non-tool: inherit the
        // parent's branch + stateful-chain anchor.
        let (initial_head, initial_chain_anchor) = if query.tool_call_id.is_some() {
            (None, None)
        } else {
            (parent_conv.head, parent_conv.chain_anchor.clone())
        };

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.clone().into(),
            query_id: query.query_id.clone(),
        };
        let mut conv = Conversation::new(
            cid.clone(),
            session_id.clone(),
            originator,
            initial_head,
            Some(source_id.into()),
        );
        // For tool-backed extensions (currently just `delegate`)
        // record the parent call id and task name so subsequent
        // sub-agent state changes can be surfaced to the user under
        // that tool block via `DelegateProgress`.
        conv.parent_tool_call_id = parent_call_id;
        conv.task_name = task_name;
        conv.chain_anchor = initial_chain_anchor;
        self.conversations.insert(cid.clone(), conv);

        // Emit the initial progress snapshot (`tools: 0/0`, no ctx
        // info yet) so the parent's tool block flips from `…` to the
        // structured form as soon as the side conversation exists,
        // without waiting for the sub-agent's first event.
        self.emit_delegate_progress(&cid);

        // Publish the UiPromptSubmitted on the side conversation's
        // branch and dispatch the agent. The fold parents the new
        // UserMessage at `conv.head` — root for the fresh tool-backed
        // mode, or the parent conv's tip for the inherited non-tool
        // mode. `send_prompt_to_agent_for` then reads `conv.head`
        // (which the post-commit hook just snapped to the new
        // UserMessage) and `assemble_conversation_from` walks back to
        // root, yielding either `[instruction]` alone or the full
        // inherited transcript + `instruction`.
        //
        // If an interceptor is registered on `ui.prompt_submitted`
        // (e.g. `tau-ext-test-dummy`'s tao→tau corrector) the publish
        // parks in `pending_intercept` and won't commit until the
        // interceptor replies — running `send_prompt_to_agent_for`
        // synchronously then would assemble messages from a still-`None`
        // `conv.head` and the agent would call the provider with an
        // empty `input`. Mirror `dispatch_prompt_for_conversation`'s
        // pattern: when the publish defers, queue the side conversation
        // on `pending_user_prompt_dispatches` so `react_to_committed_event`
        // dispatches it after the UserMessage actually folds.
        self.publish_for_conversation(
            &cid,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id,
                text: query.instruction,
                originator: tau_proto::PromptOriginator::Extension {
                    name: extension_name.into(),
                    query_id: query.query_id,
                },
                ctx_id: None,
            }),
        );
        if self.pending_intercept.is_some() || !self.deferred_publishes.is_empty() {
            self.pending_user_prompt_dispatches.push_back(cid);
        } else {
            self.send_prompt_to_agent_for(&cid);
        }
        Ok(())
    }

    /// Publish a `DelegateProgress` snapshot for `cid` if it is a side
    /// conversation backing a `delegate` tool call. No-op for the
    /// default conversation and for non-tool ext queries.
    fn emit_delegate_progress(&mut self, cid: &ConversationId) {
        let Some(conv) = self.conversations.get(cid) else {
            return;
        };
        let (Some(call_id), Some(task_name)) =
            (conv.parent_tool_call_id.clone(), conv.task_name.clone())
        else {
            return;
        };
        let ctx_window = self
            .selected_model
            .as_ref()
            .and_then(|m| model_context_window(&self.model_registry, m));
        let display = build_delegate_progress_display(
            &task_name,
            conv.context_percent_used,
            ctx_window,
            conv.tools_total,
        );
        let progress = tau_proto::DelegateProgress {
            call_id,
            task_name,
            ctx_percent: conv.context_percent_used,
            ctx_input_tokens: conv.context_input_tokens,
            ctx_window,
            tools_in_flight: conv.tools_in_flight,
            tools_total: conv.tools_total,
            display: Some(display),
        };
        self.publish_event(None, Event::ToolDelegateProgress(progress));
    }

    /// Emit a `UiNavigateTree` to move the *UI's* view cursor back
    /// to the default conversation's tip after a side conversation
    /// finishes. Purely a UI affordance now — fold parentage no
    /// longer depends on `tree.head()` (Phase 4 of the interception
    /// refactor: each publish carries its explicit parent), so
    /// nothing in the harness's append path needs the bounce. UIs
    /// that subscribe to `ui.navigate_tree` rely on this to render
    /// the user back on their main branch when a delegated query
    /// completes.
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
                    node_id: target.get(),
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
        if self.dispatch_blocked_for(&cid) || !self.session_initialized(&session_id) {
            self.conversations
                .get_mut(&cid)
                .expect("default conversation always present")
                .pending_prompts
                .push_back(text);
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
                            .map(|p| format!("<- {}", p.get()))
                            .unwrap_or_else(|| "(root)".to_owned());
                        let preview = render_entry_preview(&node.entry);
                        format!(
                            "  {:>3} {} {:>8}  {}",
                            node.id.get(),
                            marker,
                            parent,
                            preview
                        )
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
            .and_then(|t| t.node(tau_core::NodeId::new(node_id)))
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
        // for it are abandoned (the user explicitly switched away), and
        // each conversation's per-turn state is reset.
        self.turn_state = TurnState::Idle;
        for conv in self.conversations.values_mut() {
            conv.pending_prompts.clear();
            conv.in_flight_prompt = None;
            conv.turn_state = ConversationTurnState::Idle;
        }
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
            .load_session(new_session_id.as_str())?
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
        self.start_session_init(new_session_id.clone(), reason);
        let session_status = match reason {
            tau_proto::SessionStartReason::Initial | tau_proto::SessionStartReason::New => {
                tau_proto::SessionDirStatus::New
            }
            tau_proto::SessionStartReason::Resume => tau_proto::SessionDirStatus::Resumed,
        };
        self.publish_event(
            None,
            Event::HarnessSessionDir(tau_proto::HarnessSessionDir {
                session_id: new_session_id.clone(),
                path: self.dirs_state_dir().join(new_session_id.as_str()),
                status: session_status,
            }),
        );
        Ok(())
    }

    fn dirs_state_dir(&self) -> PathBuf {
        // The harness doesn't currently store the state dir directly;
        // derive it from the session store's location. SessionStore
        // exposes its root via the existing `state_dir()` accessor.
        self.store.state_dir().to_path_buf()
    }

    pub(crate) fn start_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) {
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
        // No explicit head sync needed: when the AGENTS.md
        // injection is for `current_session_id` it's stamped with
        // `default_conversation_id` and the post-commit hook keeps
        // `c.head` aligned. For other sessions there's no
        // matching live conversation anyway.
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
        let event = Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
            session_id: session_id.into(),
            text,
        });
        // Publish the injection as an event so it reaches the durable
        // session log and folds into the SessionTree the same way
        // every other entry does. Stamp it with the default
        // conversation when the session matches, so the post-commit
        // hook syncs `c.head` automatically — replaces the old
        // `sync_default_conversation_head()` call site that raced
        // when SessionUserMessageInjected was intercepted.
        if self.current_session_id == session_id {
            let cid = self.default_conversation_id.clone();
            self.publish_event_for_conversation(&cid, None, event);
        } else {
            self.publish_event(None, event);
        }
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
        let event = Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
            session_id: finished.session_id.clone(),
            text,
        });
        // When the shell output belongs to the bound session, stamp
        // the publish with the default conversation so the fold
        // lands on the user's main branch (and the post-commit hook
        // syncs `c.head`). Other sessions: best-effort plain
        // publish; nothing on this harness instance is reading
        // their tree.
        if finished.session_id == self.current_session_id {
            let cid = self.default_conversation_id.clone();
            self.publish_event_for_conversation(&cid, None, event);
        } else {
            self.publish_event(None, event);
        }
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
    pub(crate) fn send_prompt_to_agent_for(&mut self, cid: &ConversationId) -> SessionPromptId {
        let conv = self
            .conversations
            .get(cid)
            .expect("send_prompt_to_agent_for: unknown conversation id");
        let session_id = conv.session_id.clone();
        let originator = conv.originator.clone();
        // Non-tool extension side conversations (`std-notifications`'
        // idle summary, etc.) must not call tools — their whole job
        // is to produce a one-line summary, and unfettered tool
        // access has historically caused destructive `write`/`edit`
        // calls. We keep the tools list and system prompt unchanged
        // so the prompt-cache prefix continues to match the parent
        // conv; only the per-turn `tool_choice` flips to `None`.
        let tool_choice = if matches!(
            conv.originator,
            tau_proto::PromptOriginator::Extension { .. }
        ) && conv.parent_tool_call_id.is_none()
        {
            tau_proto::ToolChoice::None
        } else {
            tau_proto::ToolChoice::Auto
        };
        // Walk the conversation's *own* branch, not whatever tree.head
        // currently points at. With multiple side conversations
        // running concurrently their tree mutations interleave, so
        // tree.head is an unreliable signal for "where this
        // conversation lives". Reading from `conv.head` keeps the
        // assembled prompt scoped to this conversation's history and
        // prevents orphan ToolUse blocks from cross-branch state.
        let head = conv.head;

        let tree = self.store.session(session_id.as_str());
        let messages = tree
            .map(|t| assemble_conversation_from(t, head))
            .unwrap_or_default();
        // Stateful-chain hint: if the prior turn for this conversation
        // produced a `response_id` AND the anchor is still consistent
        // (same model selected, anchor node still on the path to
        // current head, message_count not larger than the assembled
        // count) we let the next turn run as a delta call. Otherwise
        // drop the anchor — the chain is busted and full replay is
        // the safe fallback. We resolve this BEFORE moving on so we
        // can clear an invalidated anchor in the same pass.
        let previous_response = {
            let conv = self
                .conversations
                .get(cid)
                .expect("send_prompt_to_agent_for: unknown conversation id");
            let anchor = conv.chain_anchor.as_ref();
            let valid = anchor.is_some_and(|a| {
                let model_ok = self.selected_model.as_ref() == Some(&a.model);
                let count_ok = a.message_count <= messages.len();
                let tree_ok = tree.is_some_and(|t| anchor_is_ancestor(t, a.head, conv.head));
                model_ok && count_ok && tree_ok
            });
            if valid {
                anchor.map(|a| tau_proto::PreviousResponseRef {
                    id: a.response_id.clone(),
                    message_index: a.message_count,
                })
            } else {
                None
            }
        };
        if previous_response.is_none() {
            // Drop a stale anchor so we don't keep re-checking it.
            if let Some(conv) = self.conversations.get_mut(cid) {
                conv.chain_anchor = None;
            }
        }
        let tools = self.gather_tool_definitions();
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "(unknown)".to_owned());
        let session_prompt_id: SessionPromptId =
            format!("sp-{}", self.next_session_prompt_id).into();
        self.next_session_prompt_id += 1;
        self.prompt_conversations
            .insert(session_prompt_id.clone(), cid.clone());
        let ctx_id = self
            .conversations
            .get_mut(cid)
            .and_then(|c| c.next_ctx_id.take());
        if let Some(c) = self.conversations.get_mut(cid) {
            c.in_flight_prompt = Some(session_prompt_id.clone());
            c.turn_state = ConversationTurnState::AgentThinking {
                session_prompt_id: session_prompt_id.clone(),
            };
        }

        // Publish SessionPromptCreated — both the agent and UI see it.
        let model = self.selected_model.clone();
        if let Some(model) = model.as_ref() {
            self.token_usage.start_request(model);
            self.prompt_models
                .insert(session_prompt_id.clone(), model.clone());
        }
        let event = Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id,
            system_prompt: build_system_prompt(&self.discovered_skills, &cwd),
            messages,
            tools,
            model,
            model_params: self.selected_params,
            tool_choice,
            originator,
            ctx_id,
            previous_response,
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
        mut response: AgentResponseFinished,
    ) -> Result<(), HarnessError> {
        if self.canceled_prompts.remove(&response.session_prompt_id) {
            self.prompt_conversations
                .remove(response.session_prompt_id.as_str());
            self.prompt_models.remove(&response.session_prompt_id);
            return Ok(());
        }
        if response.input_tokens.is_some() || response.cached_tokens.is_some() {
            self.update_context_usage(response.input_tokens, response.cached_tokens);
        }
        // Per-conversation usage: separate from the global tracker
        // because side conversations shouldn't clobber the user's
        // status bar, but the harness still needs their context %
        // to surface via `DelegateProgress`.
        if let Some(cid) = self.conversation_for_prompt(&response.session_prompt_id) {
            self.update_conversation_context_usage(&cid, response.input_tokens);
            self.emit_delegate_progress(&cid);
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
        // Save the model that ran this turn before the
        // `prompt_models` entry is consumed below — we'll need it
        // again to anchor the stateful-chain state, and re-reading
        // `selected_model` later would lie if the user switched
        // models mid-turn.
        let turn_model = self.prompt_models.remove(&response.session_prompt_id);
        if let Some(ref model) = turn_model {
            let sent_tokens = response.input_tokens.unwrap_or(0);
            let cached_tokens = response.cached_tokens.unwrap_or(0);
            let received_tokens = response.output_tokens.unwrap_or(0);
            self.token_usage.add_sent(model, sent_tokens, cached_tokens);
            self.token_usage.add_received(model, received_tokens);
            response.token_usage = Some(AgentTokenUsage {
                model: Some(model.clone()),
                prompt_sent_tokens: sent_tokens,
                prompt_cached_tokens: cached_tokens,
                response_received_tokens: received_tokens,
                stats: self.token_usage.clone(),
            });
        }
        // Stamp the live-header `display` descriptor on each tool
        // call so renderers don't need per-tool string knowledge.
        // Calls whose name failed to validate (`ToolNameMaybe::Invalid`)
        // get no descriptor — they fail synchronously at dispatch time
        // and never produce a running block.
        for call in &mut response.tool_calls {
            if call.display.is_some() {
                continue;
            }
            if let tau_proto::ToolNameMaybe::Valid(ref name) = call.name {
                call.display = build_tool_args_display(name.as_str(), &call.arguments);
            }
        }
        // Publish via the owning conversation's branch — when text is
        // present the SessionTree fold appends an `AgentMessage` as a
        // child of `tree.head`, so an unsnapped publish would land on
        // whichever branch happened to be at `tree.head` (e.g. after
        // a sibling side conv's teardown ran `snap_to_default`).
        // `publish_for_conversation` snaps and updates `c.head`.
        self.publish_for_conversation(&cid, Event::AgentResponseFinished(response.clone()));
        self.prompt_conversations
            .remove(response.session_prompt_id.as_str());
        // Stateful-chain anchor: set only when the agent supplied a
        // `response_id` (i.e. the upstream backend exposed one — the
        // Responses API does, Chat Completions doesn't). The anchor
        // pins this conversation's current head + assembled message
        // count so the next `send_prompt_to_agent_for` can send a
        // delta instead of replaying the full transcript.
        if let (Some(response_id), Some(model)) = (response.response_id.clone(), turn_model) {
            let (conv_head, conv_session) = self
                .conversations
                .get(&cid)
                .map(|c| (c.head, c.session_id.clone()))
                .unzip();
            let message_count = conv_session
                .as_ref()
                .and_then(|sid| self.store.session(sid.as_str()))
                .map(|tree| assemble_conversation_from(tree, conv_head.flatten()).len())
                .unwrap_or(0);
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.chain_anchor = Some(crate::conversation::ChainAnchor {
                    response_id,
                    head: conv_head.flatten(),
                    model,
                    message_count,
                });
            }
        }
        if let Some(conv) = self.conversations.get_mut(&cid) {
            conv.in_flight_prompt = None;
            conv.turn_state = ConversationTurnState::Idle;
        }
        self.completed_prompts
            .insert(response.session_prompt_id.clone());

        // Side-conversation handling: if this prompt originated from
        // an extension via ExtAgentQuery, route the final text back
        // to the requesting extension as ExtAgentQueryResult and
        // tear down the side conversation. The harness routes tool
        // calls per-conversation, so any in-flight
        // pending_tool_invocations entries for this side conversation
        // have already been emitted into the bus and will complete
        // normally even after teardown.
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
                let _ = self.bus.send_to(
                    source.as_str(),
                    None,
                    Frame::Event(Event::ExtAgentQueryResult(result)),
                );
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
            self.try_advance_queue();
            return Ok(());
        }

        if !response.tool_calls.is_empty() {
            // Tool calls to execute — agent stays busy. After all
            // tools complete, maybe_complete_agent_turn drains any
            // prompts queued via `pending_prompts` (publishing one
            // `SessionPromptSteered` each, which folds them as
            // `UserMessage` entries onto this conversation's branch)
            // and sends a new prompt with the results plus those
            // steering messages.
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
                    let kind = self.resolve_tool_kind_for_call(&call);
                    (call, kind)
                })
                .collect();

            let remaining_calls: Vec<ToolCallId> = normalized_calls
                .iter()
                .map(|(call, _)| call.id.clone())
                .collect();
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.turn_state = ConversationTurnState::ToolsRunning { remaining_calls };
            }
            // Enqueue in the order the agent emitted them. Dispatch is
            // done by `drain_pending_tool_invocations`, which respects
            // the pure-vs-mutating ordering rule.
            for (call, kind) in normalized_calls {
                self.pending_tool_invocations
                    .push_back((cid.clone(), call, kind));
            }
            self.drain_pending_tool_invocations()?;
        } else {
            // No tool calls — this conversation's turn is done. Drain
            // any queued prompts (on this or other conversations) that
            // are now eligible to dispatch.
            self.try_advance_queue();
        }

        Ok(())
    }

    /// Update one conversation's `context_input_tokens` /
    /// `context_percent_used` from a finished agent response. Mirrors
    /// `update_context_usage` but scoped to a single conversation —
    /// the global tracker is intentionally only fed by the user's
    /// default conversation so the status bar stays stable while side
    /// conversations run.
    fn update_conversation_context_usage(
        &mut self,
        cid: &ConversationId,
        input_tokens: Option<u64>,
    ) {
        let context_window = self
            .selected_model
            .as_ref()
            .and_then(|m| model_context_window(&self.model_registry, m));
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if let Some(conv) = self.conversations.get_mut(cid) {
            if input_tokens.is_some() {
                conv.context_input_tokens = input_tokens;
            }
            if percent_used.is_some() {
                conv.context_percent_used = percent_used;
            }
        }
    }

    fn update_context_usage(&mut self, input_tokens: Option<u64>, cached_tokens: Option<u64>) {
        let context_window = self
            .selected_model
            .as_ref()
            .and_then(|m| model_context_window(&self.model_registry, m));
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

    /// True iff every configured extension has either reached `Ready`
    /// or dropped permanently.
    ///
    /// `Disconnected` counts as "no longer blocking": a dead extension
    /// may be on its way to being respawned, but the old connection is
    /// gone and should not wedge fresh prompt dispatch.
    /// Session initialization for a still-live session with a dead
    /// provider still completes correctly — `handle_disconnect`
    /// removes the entry from the `waiting_on` set.
    pub(crate) fn extensions_all_ready(&self) -> bool {
        self.extensions.values().all(|e| {
            matches!(
                e.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            )
        })
    }

    /// Update an extension's lifecycle state, looked up by connection id.
    /// No-op if no entry matches (e.g. for socket clients).
    fn set_extension_state(&mut self, connection_id: &str, new_state: ExtensionState) {
        if let Some(entry) = self.extensions.get_mut(connection_id) {
            entry.state = new_state;
        }
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

    /// Same as [`resolve_tool_kind`] but lets a tool override its
    /// registered kind per-call by inspecting the call arguments.
    ///
    /// `delegate` registers as `Mutating` (the safe default) but
    /// accepts a `read_only: bool` argument; when set, the agent is
    /// asserting the sub-task does no mutation and the harness can
    /// schedule it as `Pure` — letting two read-only delegations from
    /// the same turn dispatch concurrently rather than serializing.
    fn resolve_tool_kind_for_call(&self, call: &AgentToolCall) -> tau_proto::ToolSideEffects {
        let registered = self.resolve_tool_kind(call.name.as_str());
        if call.name.as_str() == "delegate"
            && cbor_map_bool(&call.arguments, "read_only").unwrap_or(false)
        {
            return tau_proto::ToolSideEffects::Pure;
        }
        registered
    }

    /// Whether any in-flight tool call belonging to `cid` is `Mutating`.
    /// The pure-vs-mutating ordering rule is per-conversation: tools
    /// running for a *different* conversation are an independent thread
    /// of execution and must not gate this one. (Most importantly: the
    /// parent agent's mid-flight `delegate` call is `Mutating`, but it
    /// must not block the sub-agent it spawned from running its own
    /// `Pure` tools — otherwise delegate deadlocks itself.)
    fn has_mutating_in_flight_for(&self, cid: &ConversationId) -> bool {
        self.in_flight_tool_kinds.iter().any(|(call_id, kind)| {
            matches!(kind, tau_proto::ToolSideEffects::Mutating)
                && self
                    .tool_conversations
                    .get(call_id)
                    .is_some_and(|owner| owner == cid)
        })
    }

    /// Whether `cid` has any tool call currently in flight.
    fn any_in_flight_for(&self, cid: &ConversationId) -> bool {
        self.in_flight_tool_kinds.keys().any(|call_id| {
            self.tool_conversations
                .get(call_id)
                .is_some_and(|owner| owner == cid)
        })
    }

    /// State-machine drain: dispatch queued tool invocations while the
    /// per-conversation in-flight set allows them through.
    ///
    /// Rule (per conversation):
    /// - `Pure` may dispatch when no same-conversation `Mutating` is in flight.
    /// - `Mutating` may dispatch when no same-conversation call is in flight at
    ///   all.
    ///
    /// The queue can interleave entries from multiple conversations
    /// (parent + side conversations spawned mid-turn). We scan it and
    /// dispatch the first entry whose conversation is currently
    /// unblocked, repeating until no further progress can be made.
    /// Within a single conversation the per-turn FIFO order — and
    /// therefore the read-after-write ordering of Pure-then-Mutating —
    /// is preserved, because we never skip an entry that is already
    /// blocked behind an earlier same-conversation entry.
    ///
    /// Call this after enqueuing new work or after any in-flight call
    /// completes.
    fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        loop {
            let mut blocked_convs: std::collections::HashSet<ConversationId> =
                std::collections::HashSet::new();
            let mut next_idx: Option<usize> = None;
            for (idx, (cid, _call, kind)) in self.pending_tool_invocations.iter().enumerate() {
                if blocked_convs.contains(cid) {
                    continue;
                }
                let compatible = match *kind {
                    tau_proto::ToolSideEffects::Pure => !self.has_mutating_in_flight_for(cid),
                    tau_proto::ToolSideEffects::Mutating => !self.any_in_flight_for(cid),
                };
                if compatible {
                    next_idx = Some(idx);
                    break;
                }
                // Preserve per-conversation FIFO: anything later in the
                // queue from this conversation must wait behind this
                // entry, so don't consider those entries either.
                blocked_convs.insert(cid.clone());
            }
            let Some(idx) = next_idx else {
                return Ok(());
            };
            let (cid, call, kind) = self
                .pending_tool_invocations
                .remove(idx)
                .expect("index just located");
            let call_id: ToolCallId = call.id.clone();
            self.in_flight_tool_kinds.insert(call_id.clone(), kind);
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) = self.execute_agent_tool_call(&cid, &call) {
                self.in_flight_tool_kinds.remove(&call_id);
                return Err(error);
            }
        }
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    pub(crate) fn on_tool_call_complete(&mut self, call_id: &str) {
        let owned: ToolCallId = call_id.to_owned().into();
        self.in_flight_tool_kinds.remove(&owned);
        // `tool_conversations` is still populated here: the call
        // sites clear it *after* this function returns. Decrement
        // the conversation's in-flight counter and surface the new
        // state to any UI watching this delegate flow before the
        // mapping is cleared.
        let owner = self.tool_conversations.get(call_id).cloned();
        if let Some(cid) = owner.as_ref()
            && let Some(conv) = self.conversations.get_mut(cid)
        {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        if let Some(cid) = owner {
            self.emit_delegate_progress(&cid);
        }
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_info(&format!("queued tool dispatch failed: {error}"));
        }
        self.maybe_complete_agent_turn(call_id);
        self.try_advance_queue();
    }

    /// Bump the per-conversation tool counters for a freshly-started
    /// tool call. Always emits a `DelegateProgress` snapshot when the
    /// conversation is a delegate side conversation (no-op otherwise),
    /// so the UI updates the moment the sub-agent starts a new call
    /// rather than waiting for completion.
    pub(crate) fn bump_tools_started_for(&mut self, cid: &ConversationId) {
        if let Some(conv) = self.conversations.get_mut(cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_add(1);
            conv.tools_total = conv.tools_total.saturating_add(1);
        }
        self.emit_delegate_progress(cid);
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let Some(cid) = self.tool_conversations.get(completed_call_id).cloned() else {
            return;
        };
        let should_send = if let Some(conv) = self.conversations.get_mut(&cid) {
            if let ConversationTurnState::ToolsRunning { remaining_calls } = &mut conv.turn_state {
                remaining_calls.retain(|id| id.as_str() != completed_call_id);
                if remaining_calls.is_empty() {
                    conv.turn_state = ConversationTurnState::Idle;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if should_send {
            self.fold_pending_prompts_as_steered(&cid);
            // If folding the steered prompts parked any of them in
            // interception (e.g. an extension intercepting
            // `session.prompt_steered`), defer the agent dispatch
            // until they commit — same pattern as
            // `dispatch_prompt_for_conversation`. Without this, the
            // next-round `SessionPromptCreated` would assemble its
            // message list from a stale `c.head`. When no
            // interceptor matched, the publishes committed inline
            // and we can dispatch immediately.
            if self.pending_intercept.is_some() || !self.deferred_publishes.is_empty() {
                self.pending_user_prompt_dispatches.push_back(cid.clone());
            } else {
                self.send_prompt_to_agent_for(&cid);
            }
        }
    }

    /// Drain any user prompts queued on `cid` while the agent was in
    /// flight, and publish a `SessionPromptSteered` event for each. The
    /// folder in `SessionTree::apply_event` appends them as
    /// `UserMessage` entries on this conversation's branch, so the
    /// next-round `SessionPromptCreated` (about to be emitted by the
    /// caller) picks them up alongside the tool results without any
    /// extra wiring on the prompt-assembly side.
    ///
    /// Called from `maybe_complete_agent_turn` only — fresh prompts
    /// arriving on an idle conversation go through
    /// `dispatch_prompt_for_conversation`, which already publishes its
    /// own `UiPromptSubmitted`. Folding here exists specifically to
    /// give a queued prompt a chance to ride the next per-round prompt
    /// rather than waiting for the whole turn to terminate.
    fn fold_pending_prompts_as_steered(&mut self, cid: &ConversationId) {
        let session_id = match self.conversations.get(cid) {
            Some(c) => c.session_id.clone(),
            None => return,
        };
        let pending: Vec<String> = self
            .conversations
            .get_mut(cid)
            .map(|c| c.pending_prompts.drain(..).collect())
            .unwrap_or_default();
        for text in pending {
            self.publish_for_conversation(
                cid,
                Event::SessionPromptSteered(tau_proto::SessionPromptSteered {
                    session_id: session_id.clone(),
                    text,
                }),
            );
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

        let call_id: ToolCallId = call.id.clone();

        // Track conversation attribution before publishing — the
        // publish path persists the `ToolRequest` into the session
        // log and folds it into the SessionTree via `apply_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        self.bump_tools_started_for(cid);
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            arguments: call.arguments.clone(),
            originator: tau_proto::PromptOriginator::User,
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
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                };
                self.publish_for_conversation(cid, Event::ToolError(error));
                self.on_tool_call_complete(&call.id);
                self.clear_tool_call_tracking(call_id.as_str());
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
        let placeholder = ToolName::new("invalid_tool");
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
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        self.publish_for_conversation(
            cid,
            Event::ToolError(ToolError {
                call_id: call_id_owned,
                tool_name: placeholder,
                message,
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        self.on_tool_call_complete(call_id);
        self.clear_tool_call_tracking(call_id);
        Ok(())
    }
}

impl Harness {
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
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::ResponseTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    if let Frame::Event(Event::ToolProgress(progress)) = frame.as_ref() {
                        progress_messages.push(format_tool_progress(progress));
                    }
                    let is_final = matches!(
                        frame.as_ref(),
                        Frame::Event(Event::AgentResponseFinished(r))
                            if r.tool_calls.is_empty() && r.originator.is_user()
                    );
                    let final_text =
                        if let Frame::Event(Event::AgentResponseFinished(r)) = frame.as_ref() {
                            r.text.clone()
                        } else {
                            None
                        };
                    self.handle_extension_event(&connection_id, *frame)?;
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

    pub(crate) fn dump_initial_prompt(
        out_path: &Path,
        user_message: &str,
    ) -> Result<(), HarnessError> {
        let tempdir = tempfile::TempDir::new()?;
        let state_dir = tempdir.path().join("state");
        let mut harness = Self::new_with_agent(
            &state_dir,
            tau_config::settings::TauDirs::default(),
            default_agent_runner,
            Vec::new(),
            "s1",
        )?;
        harness.selected_model = Some("test/model".parse().expect("model id"));

        let cid = harness.default_conversation_id.clone();
        harness.publish_event_for_conversation(
            &cid,
            None,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id: "s1".into(),
                text: user_message.to_owned(),
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        );

        let prompt_id = harness.send_prompt_to_agent_for(&cid);
        let prompt = harness.read_session_prompt_created(&prompt_id)?;
        let mut out = String::new();
        out.push_str("================ MODEL / EFFORT ================\n");
        out.push_str(&format!(
            "model:  {}\n",
            prompt
                .model
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "(none)".to_owned())
        ));
        out.push_str(&format!("params: {:?}\n\n", prompt.model_params));

        out.push_str("================ SYSTEM PROMPT ================\n");
        out.push_str(&prompt.system_prompt);
        if !prompt.system_prompt.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');

        out.push_str("================ MESSAGES ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.messages)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push_str("\n\n");

        out.push_str("================ TOOLS ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.tools)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push('\n');

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out_path, out)?;
        harness.shutdown()?;
        Ok(())
    }

    fn read_session_prompt_created(
        &self,
        prompt_id: &SessionPromptId,
    ) -> Result<SessionPromptCreated, HarnessError> {
        let mut cursor = 0;
        loop {
            let entry = self.event_log.get_next_from(cursor).ok_or_else(|| {
                HarnessError::Participant("prompt event missing from log".to_owned())
            })?;
            cursor = entry.seq + 1;
            if let Event::SessionPromptCreated(prompt) = entry.event
                && &prompt.session_prompt_id == prompt_id
            {
                return Ok(prompt);
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
        // stdin, wait/kill child). Walk `extension_order` so shutdown
        // honours spawn order.
        for id in &self.extension_order {
            let _ = self.bus.disconnect(id);
        }

        // Join in-process extension threads.
        let order = self.extension_order.clone();
        for id in &order {
            let Some(entry) = self.extensions.get_mut(id) else {
                continue;
            };
            let name = entry.name.clone();
            if let Some(handle) = entry.in_process_thread.take() {
                let result = handle
                    .join()
                    .map_err(|_| HarnessError::ThreadJoin(name.clone()))?;
                result.map_err(HarnessError::Participant)?;
            }
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    #[cfg(test)]
    fn extension_connection_id(&self, name: &str) -> Option<&str> {
        self.extensions
            .values()
            .find(|e| e.name == name)
            .map(|e| e.connection_id.as_str())
    }
}

/// Pre-render the live-header descriptor for a tool call so the
/// CLI (and any future renderer) can paint the running block without
/// per-tool string knowledge. The descriptor carries the tool's
/// args label (e.g. `"foo" in src` for grep, `[task]` for delegate)
/// and is stamped with [`tau_proto::ToolDisplayStatus::InProgress`] /
/// `"…"` so subscribers render the running ellipsis uniformly.
///
/// Tools without a known label shape return `None`; the renderer
/// falls back to a name-only block.
/// True iff `anchor` is on the path from the tree root to
/// `descendant`. Used to check whether a previously-captured
/// stateful-chain anchor is still consistent with the conversation's
/// current head — branch switches (via `UiNavigateTree`) leave the
/// anchor stranded on a sibling branch, in which case the chain
/// should be invalidated and the next turn replays the full
/// transcript.
fn anchor_is_ancestor(
    tree: &tau_core::SessionTree,
    anchor: Option<tau_core::NodeId>,
    descendant: Option<tau_core::NodeId>,
) -> bool {
    // An empty-tree anchor matches an empty-tree descendant: both
    // sit at the root sentinel. Anything else with `anchor == None`
    // would be a malformed anchor (chain pinned to nothing).
    let Some(anchor) = anchor else {
        return descendant.is_none();
    };
    let mut current = descendant;
    while let Some(id) = current {
        if id == anchor {
            return true;
        }
        current = tree.node(id).and_then(|node| node.parent_id);
    }
    false
}

fn build_tool_args_display(
    tool_name: &str,
    arguments: &tau_proto::CborValue,
) -> Option<tau_proto::ToolDisplay> {
    use tau_proto::{ToolDisplayStatus, cbor_bool_field, cbor_text_field};

    let args = match tool_name {
        "shell" => cbor_text_field(arguments, "command").unwrap_or_default(),
        "read" | "write" | "edit" => cbor_text_field(arguments, "path").unwrap_or_default(),
        "find" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            format!("{pattern} in {path}")
        }
        "grep" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            let mut args = format!("{pattern:?} in {path}");
            if let Some(glob) = cbor_text_field(arguments, "glob") {
                args.push_str(&format!(" [{glob}]"));
            }
            args
        }
        "ls" => cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned()),
        "delegate" => match cbor_text_field(arguments, "task_name") {
            Some(name) if !name.is_empty() => format!("[{name}]"),
            _ => String::new(),
        },
        "skill" => match cbor_text_field(arguments, "action").as_deref() {
            Some("search") => {
                let query = cbor_text_field(arguments, "query").unwrap_or_default();
                let scope = if cbor_bool_field(arguments, "search_content").unwrap_or(false) {
                    " [content]"
                } else {
                    ""
                };
                format!("search: {query}{scope}")
            }
            // Default to load semantics for `action: "load"` and for
            // legacy / malformed calls without an action.
            _ => cbor_text_field(arguments, "name").unwrap_or_default(),
        },
        _ => return None,
    };
    Some(tau_proto::ToolDisplay {
        args,
        status: ToolDisplayStatus::InProgress,
        status_text: "…".to_owned(),
        ..Default::default()
    })
}

/// Build the [`ToolDisplay`] descriptor the renderer paints for a
/// running `delegate` tool block. Carries the sub-task name as the
/// args label and two progress counters (context and tools), with
/// the trailing chip set to [`ToolDisplayStatus::InProgress`] so the
/// renderer paints the `…` running indicator.
fn build_delegate_progress_display(
    task_name: &str,
    ctx_percent: Option<u8>,
    ctx_window: Option<u64>,
    tools_total: u32,
) -> tau_proto::ToolDisplay {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolDisplayStatus};

    let mut counters: Vec<ProgressCounter> = Vec::new();
    if ctx_percent.is_some() || ctx_window.is_some() {
        counters.push(ProgressCounter {
            label: Some("ctx".to_owned()),
            unit: ProgressUnit::Percent,
            current: ctx_percent.map(u64::from),
            total: ctx_window,
        });
    }
    counters.push(ProgressCounter {
        label: Some("tools".to_owned()),
        unit: ProgressUnit::Count,
        current: Some(u64::from(tools_total)),
        total: None,
    });
    tau_proto::ToolDisplay {
        args: format!("[{task_name}]"),
        progress_counters: counters,
        status: ToolDisplayStatus::InProgress,
        status_text: "…".to_owned(),
        ..Default::default()
    }
}

/// Replace the `originator` on a tool-related event with the owning
/// conversation's originator. Non-tool events pass through unchanged.
fn stamp_tool_event_originator(event: Event, originator: tau_proto::PromptOriginator) -> Event {
    match event {
        Event::ToolRequest(mut e) => {
            e.originator = originator;
            Event::ToolRequest(e)
        }
        Event::ToolResult(mut e) => {
            e.originator = originator;
            Event::ToolResult(e)
        }
        Event::ToolError(mut e) => {
            e.originator = originator;
            Event::ToolError(e)
        }
        other => other,
    }
}

pub(crate) fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    let target_name = event.name();
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    })
}
