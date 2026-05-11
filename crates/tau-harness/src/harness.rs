//! [`Harness`]: the central event loop. Owns the bus, registry, session
//! store, and the live extensions; routes every event between the agent,
//! tools, and clients.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    AgentResponseFinished, AgentToolCall, CborValue, ClientKind, Disconnect, Event, EventName,
    EventSelector, ExtensionName, Frame, HarnessContextUsageChanged, HarnessModelSelected,
    HarnessModelsAvailable, InterceptAction, InterceptReply, InterceptRequest,
    InterceptionPriority, Message, ModelId, SessionId, SessionPromptCreated, SessionPromptId,
    SessionPromptQueued, ToolCallId, ToolDefinition, ToolError, ToolName, ToolRegister,
    ToolRequest, UiCancelPrompt,
};

use crate::conversation::{Conversation, ConversationId, ConversationTurnState};
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
    assemble_conversation_from, build_system_prompt, cbor_map_bool, cbor_map_text,
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
    pub(crate) client_writers: std::collections::HashMap<tau_proto::ConnectionId, Sender<Frame>>,
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
    /// Prompt ids canceled by `/cancel`. Late agent events for these
    /// prompts are ignored and never folded into session state.
    pub(crate) canceled_prompts: std::collections::HashSet<SessionPromptId>,
    /// Directory layout (config + state) the harness reads and writes.
    pub(crate) dirs: tau_config::settings::TauDirs,
}

pub(crate) type AgentRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

/// Snapshot of a publish that's currently waiting on an interceptor's
/// reply. The harness stops draining further publishes while one of
/// these is alive so the persisted log order matches publish order.
pub(crate) struct PendingIntercept {
    /// Connection that owes us an [`InterceptReply`].
    pub(crate) conn_id: String,
    /// Event sent in the [`InterceptRequest`]. Returned to the chain
    /// if the reply is `Pass(None)`, replaced if `Pass(Some(_))`.
    pub(crate) event: Event,
    /// Whether the original publisher requested transient delivery.
    /// Carried so the eventual commit honours the call site's intent.
    pub(crate) transient: bool,
    /// Original source connection id from the publish call (for log
    /// persistence + bus broadcast).
    pub(crate) source: Option<String>,
    /// If `true`, an interceptor returning `Drop` is overridden:
    /// `tracing::warn!` and continue with the original event.
    pub(crate) must_pass: bool,
    /// Conversation that originated this publish, if any. When the
    /// event eventually commits, the harness syncs this
    /// conversation's `head` to the post-fold `tree.head()`. Set
    /// only by `publish_for_conversation*`; `publish_event` leaves
    /// it `None`.
    pub(crate) sync_head_for: Option<ConversationHeadSync>,
    /// Cursor for the next interceptor lookup *after* this reply
    /// resolves. Set to the registration we just dispatched to, so
    /// the chain advances strictly past it.
    pub(crate) cursor: (InterceptionPriority, String),
}

/// A publish that arrived while another publish was in interception
/// limbo. Replayed through the normal entry point once the in-flight
/// interception resolves.
pub(crate) struct DeferredPublish {
    pub(crate) source: Option<String>,
    pub(crate) event: Event,
    pub(crate) transient: bool,
    pub(crate) must_pass: bool,
    pub(crate) sync_head_for: Option<ConversationHeadSync>,
}

/// Carried on a publish so that, once the event commits and the
/// `SessionTree` fold advances `tree.head()`, the harness can sync
/// the originating conversation's cached `head` to the new node.
/// Replaces the old "publish then read `tree.head()`" idiom which
/// breaks when an interceptor parks the publish.
#[derive(Clone)]
pub(crate) struct ConversationHeadSync {
    pub(crate) cid: ConversationId,
    /// Retained for back-compatibility with callers building a
    /// [`ConversationHeadSync`] from a session id; the sync logic in
    /// `commit_event` now reads the just-folded node id directly from
    /// [`SessionStore::append_session_event_at`]'s outcome rather than
    /// looking up the tree by session.
    #[allow(dead_code)]
    pub(crate) session_id: SessionId,
}

/// Event types where a `Drop` reply from an interceptor is
/// overridden into `Pass(None)` with a `tracing::warn!`.
///
/// These events carry state changes the harness can't reasonably
/// continue without — silently dropping a `UiPromptSubmitted`, for
/// example, would leave the UI staring at a half-typed prompt while
/// the harness believes nothing happened. Interceptors that try to
/// drop one of these are almost certainly buggy.
const MUST_PASS_BY_DEFAULT: &[EventName] = &[
    // User-message-bearing events: dropping any of these would
    // make the user's input vanish silently while the harness
    // believes the prompt was delivered.
    EventName::UI_PROMPT_SUBMITTED,
    EventName::SESSION_USER_MESSAGE_INJECTED,
    EventName::SESSION_PROMPT_STEERED,
    // Agent prompt life-cycle: the agent extension consumes
    // `SessionPromptCreated` to know when to talk to the LLM.
    // Dropping it wedges the conversation.
    EventName::SESSION_PROMPT_CREATED,
    // Agent response: dropping this would wedge `c.head` /
    // `prompt_conversations` bookkeeping and the conversation
    // would never advance.
    EventName::AGENT_RESPONSE_FINISHED,
    // Tool round-trip closure: a missing `tool.result`/`tool.error`
    // for a tool that was actually invoked leaves the agent waiting
    // forever.
    EventName::TOOL_RESULT,
    EventName::TOOL_ERROR,
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterceptorRegistration {
    priority: InterceptionPriority,
    component_name: ExtensionName,
    connection_id: tau_proto::ConnectionId,
}

impl Ord for InterceptorRegistration {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| {
                self.component_name
                    .as_str()
                    .cmp(other.component_name.as_str())
            })
            .then_with(|| {
                self.connection_id
                    .as_str()
                    .cmp(other.connection_id.as_str())
            })
    }
}

impl PartialOrd for InterceptorRegistration {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(crate) struct InterceptorRegistry {
    exact: BTreeMap<tau_proto::EventName, BTreeSet<InterceptorRegistration>>,
    prefix: BTreeMap<String, BTreeSet<InterceptorRegistration>>,
}

impl InterceptorRegistry {
    fn replace_for_connection(
        &mut self,
        connection_id: &str,
        component_name: ExtensionName,
        selectors: Vec<EventSelector>,
        priority: InterceptionPriority,
    ) {
        self.remove_connection(connection_id);
        let registration = InterceptorRegistration {
            priority,
            component_name,
            connection_id: connection_id.into(),
        };
        for selector in selectors {
            match selector {
                EventSelector::Exact(name) => {
                    self.exact
                        .entry(name)
                        .or_default()
                        .insert(registration.clone());
                }
                EventSelector::Prefix(prefix) => {
                    self.prefix
                        .entry(prefix)
                        .or_default()
                        .insert(registration.clone());
                }
            }
        }
    }

    fn remove_connection(&mut self, connection_id: &str) {
        for registrations in self.exact.values_mut() {
            registrations.retain(|r| r.connection_id.as_str() != connection_id);
        }
        self.exact
            .retain(|_, registrations| !registrations.is_empty());
        for registrations in self.prefix.values_mut() {
            registrations.retain(|r| r.connection_id.as_str() != connection_id);
        }
        self.prefix
            .retain(|_, registrations| !registrations.is_empty());
    }

    fn next_for(
        &self,
        event: &Event,
        cursor: Option<(InterceptionPriority, &str)>,
    ) -> Option<InterceptorRegistration> {
        let name = event.name();
        if let Some(next) = self.next_in_set(self.exact.get(&name), cursor) {
            return Some(next);
        }
        self.prefix
            .iter()
            .filter(|(prefix, _)| name.matches_prefix(prefix))
            .filter_map(|(_, registrations)| self.next_in_set(Some(registrations), cursor))
            .min()
    }

    fn next_in_set(
        &self,
        registrations: Option<&BTreeSet<InterceptorRegistration>>,
        cursor: Option<(InterceptionPriority, &str)>,
    ) -> Option<InterceptorRegistration> {
        registrations?
            .iter()
            .find(|registration| {
                cursor.is_none_or(|(priority, connection_id)| {
                    priority < registration.priority
                        || (priority == registration.priority
                            && connection_id < registration.connection_id.as_str())
                })
            })
            .cloned()
    }
}

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
        crate::session_cleanup::spawn_session_cleanup(
            state_dir.clone(),
            harness_settings.session_retention(),
        );
        let selected_effort = selected_effort_for_model(
            &dirs,
            &harness_settings,
            &model_registry,
            selected_model.as_str(),
        );

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
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
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
        let mut _next_instance_counter: u64 = 0;
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

        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "loading model list");
        let (available_models, selected_model, model_registry, harness_settings) =
            load_model_list(&dirs);
        tracing::debug!(target: "tau_harness::startup", selected_model = %selected_model, elapsed_ms = startup_started_at.elapsed().as_millis(), "model list loaded");
        crate::session_cleanup::spawn_session_cleanup(
            state_dir.clone(),
            harness_settings.session_retention(),
        );
        let selected_effort = selected_effort_for_model(
            &dirs,
            &harness_settings,
            &model_registry,
            selected_model.as_str(),
        );

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
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
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

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "extensions ready");
        harness.register_harness_tools();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness tools registered");
        harness.check_config_exists();
        harness.check_config_parses();
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

    /// Conversation id that owns a given tool call, if any.
    #[allow(dead_code)] // used by the ext-query path wired in a follow-up step
    fn conversation_for_tool_call(&self, call_id: &ToolCallId) -> Option<ConversationId> {
        self.tool_conversations.get(call_id).cloned()
    }

    /// Conversation id that owns a given in-flight prompt, if any.
    fn conversation_for_prompt(&self, spid: &SessionPromptId) -> Option<ConversationId> {
        self.prompt_conversations.get(spid).cloned()
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
    fn publish_for_conversation(&mut self, cid: &ConversationId, event: Event) {
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

    /// Like [`Harness::publish_event`] but the publish is marked
    /// `must_pass`: an interceptor that returns
    /// [`InterceptAction::Drop`] for this event is overridden — the
    /// harness `tracing::warn!`s and continues with the original.
    /// Reserved for events whose silent disappearance would corrupt
    /// the harness's own bookkeeping (e.g. session-prompt life-cycle).
    #[allow(dead_code)] // wired up at must-pass call sites in a later phase
    fn publish_event_must_pass(&mut self, source: Option<&str>, event: Event) {
        let transient = event.defaults_to_transient();
        self.enqueue_publish(source, event, transient, true, None);
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
        let Some(session_id) = self.conversations.get(cid).map(|c| c.session_id.clone()) else {
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
        };
        let transient = event.defaults_to_transient();
        let sync = Some(ConversationHeadSync {
            cid: cid.clone(),
            session_id,
        });
        self.enqueue_publish(source, event, transient, false, sync);
    }

    /// Entry point for any publish call. Defers if interception is
    /// in flight; otherwise drives the publish through the
    /// interception chain and into the bus.
    fn enqueue_publish(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        if self.pending_intercept.is_some() {
            self.deferred_publishes.push_back(DeferredPublish {
                source: source.map(str::to_owned),
                event,
                transient,
                must_pass,
                sync_head_for,
            });
            return;
        }
        self.dispatch_publish_step(
            source.map(str::to_owned),
            event,
            transient,
            must_pass,
            sync_head_for,
            None,
        );
    }

    /// One step through the interception chain for a single publish.
    ///
    /// `cursor` is `None` on the first dispatch and `Some((priority,
    /// connection_id))` on subsequent steps so the lookup advances
    /// strictly past the interceptor that just replied. If a matching
    /// interceptor is found, an [`InterceptRequest`] is sent and the
    /// publish parks in `pending_intercept` waiting for its reply.
    /// If no further interceptor matches, the event commits.
    fn dispatch_publish_step(
        &mut self,
        source: Option<String>,
        event: Event,
        transient: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
        cursor: Option<(InterceptionPriority, &str)>,
    ) {
        if let Some(interceptor) = self.interceptors.next_for(&event, cursor) {
            tracing::debug!(
                target: "tau_harness::interception",
                event = %event.name(),
                priority = interceptor.priority.0,
                component = %interceptor.component_name,
                connection_id = %interceptor.connection_id,
                "intercepting event emission"
            );
            let conn_id = interceptor.connection_id.as_str().to_owned();
            let _ = self.bus.send_to(
                &conn_id,
                None,
                Frame::Message(Message::InterceptRequest(InterceptRequest {
                    event: Box::new(event.clone()),
                    transient,
                })),
            );
            self.pending_intercept = Some(PendingIntercept {
                conn_id: conn_id.clone(),
                event,
                transient,
                source,
                must_pass,
                sync_head_for,
                cursor: (interceptor.priority, conn_id),
            });
            return;
        }
        self.commit_event(source.as_deref(), event, transient, sync_head_for);
    }

    /// Final commit: persist (when applicable), append to the event
    /// log, and broadcast on the bus. Does not consult interception
    /// state — the caller is responsible for getting here only when
    /// the chain has resolved. Triggers any post-commit reactions
    /// (currently: deferred agent dispatches that were waiting on
    /// this user-message-bearing event to land in the tree, plus
    /// per-publish conversation `head` syncs).
    fn commit_event(
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
        let folded_node_id = self.persist_session_event(source, &event, transient, parent_for_fold);
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
        }
        let seq = self
            .event_log
            .append(source.map(tau_proto::ConnectionId::from), event.clone());
        // Wrap in a `LogEvent` message envelope so subscribers get the
        // id and can ack after processing. Receivers that don't care
        // (UIs) call `Frame::peel_log()` and discard the id.
        let log_frame = Frame::Message(Message::LogEvent(tau_proto::LogEvent {
            id: tau_proto::LogEventId::new(seq),
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

    /// Resolve a parked interception with the extension's reply.
    /// Advances the chain (next interceptor, or commit), then drains
    /// any publishes that arrived while we were waiting.
    fn handle_intercept_reply(&mut self, conn_id: &str, reply: InterceptReply) {
        let Some(pending) = self.pending_intercept.take() else {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = conn_id,
                "InterceptReply received without a pending intercept; ignoring",
            );
            return;
        };
        if pending.conn_id != conn_id {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = conn_id,
                expected = %pending.conn_id,
                "InterceptReply from unexpected connection; ignoring and \
                 continuing to wait",
            );
            // Restore — we're still waiting on the original responder.
            self.pending_intercept = Some(pending);
            return;
        }
        self.advance_pending_intercept(pending, reply.action);
        self.drain_deferred_publishes();
    }

    /// Resolve a pending intercept whose responder disconnected.
    /// Defaults to `Pass(None)` so the original event still flows —
    /// extensions cannot wedge the harness by going away mid-reply.
    fn fail_pending_intercept_for_disconnect(&mut self, conn_id: &str) {
        let Some(pending) = self.pending_intercept.take() else {
            return;
        };
        if pending.conn_id != conn_id {
            self.pending_intercept = Some(pending);
            return;
        }
        tracing::warn!(
            target: "tau_harness::interception",
            connection_id = conn_id,
            "interceptor disconnected mid-reply; treating as Pass(None)",
        );
        self.advance_pending_intercept(pending, InterceptAction::Pass(None));
        self.drain_deferred_publishes();
    }

    /// Apply an [`InterceptAction`] to a pending intercept and drive
    /// the next chain step (or commit, or drop).
    fn advance_pending_intercept(&mut self, pending: PendingIntercept, action: InterceptAction) {
        let PendingIntercept {
            conn_id: _,
            event: original_event,
            transient,
            source,
            must_pass,
            sync_head_for,
            cursor,
        } = pending;

        let event_name = original_event.name();
        let next_event = match action {
            InterceptAction::Pass(None) => Some(original_event),
            InterceptAction::Pass(Some(boxed)) => {
                let new_event = *boxed;
                if new_event.name() != event_name {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        original = %event_name,
                        replacement = %new_event.name(),
                        "interceptor returned a different event type; \
                         falling back to the original",
                    );
                    Some(original_event)
                } else {
                    Some(new_event)
                }
            }
            InterceptAction::Drop => {
                let must_pass_default = MUST_PASS_BY_DEFAULT.contains(&event_name);
                if must_pass || must_pass_default {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        event = %event_name,
                        must_pass_caller = must_pass,
                        must_pass_default = must_pass_default,
                        "interceptor tried to Drop a must-pass event; \
                         publishing original instead",
                    );
                    Some(original_event)
                } else {
                    tracing::debug!(
                        target: "tau_harness::interception",
                        event = %event_name,
                        "interceptor dropped event",
                    );
                    None
                }
            }
        };

        let Some(event) = next_event else {
            return;
        };

        self.dispatch_publish_step(
            source,
            event,
            transient,
            must_pass,
            sync_head_for,
            Some((cursor.0, cursor.1.as_str())),
        );
    }

    /// Drain `deferred_publishes` until either it's empty or one of
    /// them parks a new intercept.
    fn drain_deferred_publishes(&mut self) {
        while self.pending_intercept.is_none() {
            let Some(deferred) = self.deferred_publishes.pop_front() else {
                break;
            };
            self.dispatch_publish_step(
                deferred.source,
                deferred.event,
                deferred.transient,
                deferred.must_pass,
                deferred.sync_head_for,
                None,
            );
        }
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
    ) -> Option<tau_proto::NodeId> {
        if transient || event.is_transient() {
            return None;
        }
        let session_id = self.session_id_for_event(event)?;
        let source = source.map(tau_proto::ConnectionId::from);
        self.store
            .append_session_event_at(session_id.as_str(), source, parent_node_id, event.clone())
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
            Message::Hello(_hello) => {
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.send_lifecycle_configure(source_id);
            }
            Message::ConfigError(err) => {
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
                let transient = emit.transient || emit.event.is_transient();
                self.enqueue_publish(Some(source_id), *emit.event, transient, false, None);
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
            Event::ToolResult(result) => {
                if let Some(cid) = self.tool_conversations.get(&result.call_id).cloned() {
                    let call_id = result.call_id.to_string();
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
            Event::ToolError(error) => {
                if let Some(cid) = self.tool_conversations.get(&error.call_id).cloned() {
                    let call_id = error.call_id.to_string();
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
                .unwrap_or_else(|| ToolName::from("unknown_tool"));
            let error = ToolError {
                call_id: call_id.clone(),
                tool_name,
                message: "tool provider disconnected".to_owned(),
                details: None,
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

    fn replay_session_events(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let Ok(events) = self.store.session_events(self.current_session_id.as_str()) else {
            return;
        };
        for entry in events {
            if selector_matches_event(selectors, &entry.event)
                && should_replay_session_event_to_late_subscriber(&entry.event)
            {
                let frame = Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                    id: entry.id,
                    event: Box::new(entry.event),
                }));
                let _ = self.bus.send_to(client_id, entry.source.as_deref(), frame);
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
                    | Event::HarnessSessionDir(_)
                    | Event::HarnessUiDir(_)
                    | Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
            );
            if dominated && selector_matches_event(selectors, &entry.event) {
                let _ = self.bus.send_to(
                    client_id,
                    entry.source.as_deref(),
                    Frame::Event(entry.event),
                );
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(models_event));
        }
        let selected_event = Event::HarnessModelSelected(HarnessModelSelected {
            model: self.selected_model.clone(),
            context_window: model_context_window(
                &self.model_registry,
                self.selected_model.as_str(),
            ),
        });
        if selector_matches_event(selectors, &selected_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(selected_event));
        }
        let context_event = Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: self.context_input_tokens,
            cached_tokens: self.context_cached_tokens,
            percent_used: self.context_percent_used,
        });
        if selector_matches_event(selectors, &context_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(context_event));
        }
        let effort_event = Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
            level: self.selected_effort,
        });
        if selector_matches_event(selectors, &effort_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(effort_event));
        }
        let levels = efforts_for_model(&self.model_registry, self.selected_model.as_str());
        let levels_event =
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable { levels });
        if selector_matches_event(selectors, &levels_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(levels_event));
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
                ctx_id: None,
            }),
        );
        if self.pending_intercept.is_some() || !self.deferred_publishes.is_empty() {
            // Publish parked in interception (or queued behind one
            // that is). Defer the agent dispatch until the user-
            // prompt event actually commits — see
            // `react_to_committed_event` for the drain.
            self.pending_user_prompt_dispatches.push_back(cid.clone());
        } else {
            // Publish committed inline. Safe to dispatch the agent
            // prompt now: the SessionTree already reflects the new
            // user message, so the message list assembled inside
            // `send_prompt_to_agent_for` will include it.
            self.send_prompt_to_agent_for(cid);
        }
        Ok(())
    }

    fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    /// Spawn a fresh side conversation for an extension's
    /// [`tau_proto::ExtAgentQuery`] and dispatch it. The harness has no
    /// global agent slot — the side conversation publishes its own
    /// `SessionPromptCreated` immediately, and the agent extension
    /// serializes consumption from the event log.
    ///
    /// Every sub-agent starts with a *fresh* context: only its
    /// delegated instruction, no inherited messages from the parent
    /// (no user framing, no completed prior turns, no in-flight tool
    /// blocks). The parent agent is responsible for putting everything
    /// the sub-agent needs into the `prompt`. This applies uniformly
    /// at any nesting depth — a top-level delegate and a recursive
    /// delegate get the same isolation, so deeper sub-agents can't
    /// see (and restage) ancestor task framing.
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

        // Resolve the session id from the conversation that issued
        // the tool call, so the side branch lands in the right
        // session. We deliberately do NOT inherit the parent's
        // tree head: `branch_root = None` means the side conversation
        // starts at the tree root and `send_prompt_to_agent_for`
        // assembles a message list containing only the new
        // `UiPromptSubmitted` we publish below.
        let parent_cid = query
            .tool_call_id
            .as_ref()
            .and_then(|call_id| self.tool_conversations.get(call_id))
            .cloned()
            .unwrap_or_else(|| self.default_conversation_id.clone());
        let session_id = self
            .conversations
            .get(&parent_cid)
            .map(|c| c.session_id.clone())
            .expect("parent conversation always present");

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.clone().into(),
            query_id: query.query_id.clone(),
        };
        let mut conv = Conversation::new(
            cid.clone(),
            session_id.clone(),
            originator,
            None,
            Some(source_id.into()),
        );
        // For tool-backed extensions (currently just `delegate`)
        // record the parent call id and task name so subsequent
        // sub-agent state changes can be surfaced to the user under
        // that tool block via `DelegateProgress`.
        conv.parent_tool_call_id = parent_call_id;
        conv.task_name = task_name;
        self.conversations.insert(cid.clone(), conv);

        // Emit the initial progress snapshot (`tools: 0/0`, no ctx
        // info yet) so the parent's tool block flips from `…` to the
        // structured form as soon as the side conversation exists,
        // without waiting for the sub-agent's first event.
        self.emit_delegate_progress(&cid);

        // Publish the UiPromptSubmitted on the side conversation's
        // branch (head=None → folds at root) and dispatch the agent.
        // `send_prompt_to_agent_for` reads `conv.head`, which after
        // the commit points at the just-folded UserMessage, so the
        // assembled message list is exactly the one user turn
        // carrying `query.instruction`.
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
        let ctx_window = model_context_window(&self.model_registry, self.selected_model.as_str());
        let progress = tau_proto::DelegateProgress {
            call_id,
            task_name,
            ctx_percent: conv.context_percent_used,
            ctx_input_tokens: conv.context_input_tokens,
            ctx_window,
            tools_in_flight: conv.tools_in_flight,
            tools_total: conv.tools_total,
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
        // for it are abandoned (the user explicitly switched away), and
        // each conversation's per-turn state is reset.
        self.turn_state = TurnState::Idle;
        for conv in self.conversations.values_mut() {
            conv.pending_prompts.clear();
            conv.in_flight_prompt = None;
            conv.turn_state = ConversationTurnState::Idle;
        }
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
    fn send_prompt_to_agent_for(&mut self, cid: &ConversationId) -> SessionPromptId {
        let conv = self
            .conversations
            .get(cid)
            .expect("send_prompt_to_agent_for: unknown conversation id");
        let session_id = conv.session_id.clone();
        let originator = conv.originator.clone();
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
        let model = if self.selected_model.is_empty() {
            None
        } else {
            Some(self.selected_model.clone())
        };
        let event = Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id,
            system_prompt: build_system_prompt(&self.discovered_skills, &cwd),
            messages,
            tools,
            model,
            effort: self.selected_effort,
            thinking_summary: self.selected_thinking_summary,
            originator,
            ctx_id,
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
        if self.canceled_prompts.remove(&response.session_prompt_id) {
            self.prompt_conversations
                .remove(response.session_prompt_id.as_str());
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
        // Publish via the owning conversation's branch — when text is
        // present the SessionTree fold appends an `AgentMessage` as a
        // child of `tree.head`, so an unsnapped publish would land on
        // whichever branch happened to be at `tree.head` (e.g. after
        // a sibling side conv's teardown ran `snap_to_default`).
        // `publish_for_conversation` snaps and updates `c.head`.
        self.publish_for_conversation(&cid, Event::AgentResponseFinished(response.clone()));
        self.prompt_conversations
            .remove(response.session_prompt_id.as_str());
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
        let context_window =
            model_context_window(&self.model_registry, self.selected_model.as_str());
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

    /// Drains every runnable conversation's pending prompt queue.
    ///
    /// There is no global agent slot — the agent extension serializes
    /// its own consumption of `SessionPromptCreated`. The harness emits
    /// one prompt per runnable conversation (Idle turn state, non-empty
    /// queue) and routes responses back via `prompt_conversations`.
    ///
    /// Session initialization still happens before prompt dispatch, so
    /// a fresh `chat-*` session can discover AGENTS.md and skills before
    /// the agent sees the first user message.
    fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || self.selected_model.is_empty()
        {
            return;
        }

        while let Some(cid) = self.next_runnable_conversation() {
            let session_id = self
                .conversations
                .get(&cid)
                .map(|c| c.session_id.clone())
                .expect("runnable conversation exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let text = self
                .conversations
                .get_mut(&cid)
                .and_then(|c| c.pending_prompts.pop_front())
                .expect("runnable conversation has a prompt");
            if let Err(error) = self.dispatch_prompt_for_conversation(&cid, text) {
                self.emit_info(&format!("failed to dispatch queued prompt: {error}"));
                // Reset the conversation so it doesn't wedge as
                // AgentThinking with no in-flight prompt.
                if let Some(conv) = self.conversations.get_mut(&cid) {
                    conv.in_flight_prompt = None;
                    conv.turn_state = ConversationTurnState::Idle;
                }
            }
        }
    }

    fn next_runnable_conversation(&self) -> Option<ConversationId> {
        self.conversations
            .iter()
            .find(|(_, conv)| {
                !conv.pending_prompts.is_empty()
                    && matches!(conv.turn_state, ConversationTurnState::Idle)
            })
            .map(|(cid, _)| cid.clone())
    }

    /// True when a fresh prompt for `cid` should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: no model selected, harness mid-init, extensions not yet
    ///   `Ready`;
    /// - per-conversation: that conversation already has a prompt in flight or
    ///   is waiting on tool results.
    fn dispatch_blocked_for(&self, cid: &ConversationId) -> bool {
        if self.selected_model.is_empty()
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            return true;
        }
        match self.conversations.get(cid) {
            Some(conv) => !matches!(conv.turn_state, ConversationTurnState::Idle),
            None => true,
        }
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
    fn on_tool_call_complete(&mut self, call_id: &str) {
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
    fn bump_tools_started_for(&mut self, cid: &ConversationId) {
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
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        self.on_tool_call_complete(call_id);
        self.clear_tool_call_tracking(call_id);
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
                     specific tasks. The user has likely curated skills for \
                     workflows they care about, so reach for this tool early: \
                     before tackling any request that touches a tool, command, \
                     framework, or domain you are not deeply familiar with — or \
                     anything the user might have an opinionated way of doing — \
                     run `search` first. Most skills are NOT pre-advertised in \
                     <available_skills>, so a missing entry there is no reason \
                     to skip the search. When a task could plausibly map to \
                     several names (\"commit\", \"git commit\", \"version \
                     control\"), pass them all as a `query` array — hits are \
                     merged and ranked by how many terms matched. Use \
                     `action: load` once you have an exact skill name."
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
                            "type": ["string", "array"],
                            "items": {"type": "string"},
                            "description": "(action=search) One or more keywords matched case-insensitively against skill names and descriptions. Single string or array of strings."
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
        let call_id: ToolCallId = call.id.clone();
        let tool_name: ToolName = "skill".into();

        // Track the conversation mapping first so the published
        // request + result both attribute to this conversation's
        // session via `session_id_for_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        self.bump_tools_started_for(cid);
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments: call.arguments.clone(),
                originator: tau_proto::PromptOriginator::User,
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
                originator: tau_proto::PromptOriginator::User,
            }),
            None => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "missing required argument: action (\"load\" or \"search\")".to_owned(),
                details: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        };

        // Publish, then drop the in-flight tracking — order matters:
        // `session_id_for_event` reads `tool_conversations` to
        // attribute the persisted record before we clear it.
        self.publish_for_conversation(cid, result_event);
        self.on_tool_call_complete(&call.id);
        self.clear_tool_call_tracking(call_id.as_str());

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
                originator: tau_proto::PromptOriginator::User,
            });
        };
        let Some(skill) = self.discovered_skills.get(name) else {
            // Same agent that asked for `dpc-rust-code-style` very likely
            // wanted one of the skills containing "rust" or "style", so
            // run a free search using the requested name split into
            // word-like tokens. Returning the hits in `details` lets the
            // agent pick the right name on a follow-up call without
            // having to issue an explicit `search` first; the
            // surrounding event is still an error so it can't be
            // mistaken for a successful load.
            let needles = split_skill_name_into_needles(name);
            let matches = if needles.is_empty() {
                Vec::new()
            } else {
                self.search_discovered_skills(&needles, false)
            };
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: format!("unknown skill: {name}"),
                details: Some(skill_load_not_found_details(name, &needles, &matches)),
                originator: tau_proto::PromptOriginator::User,
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
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(e) => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: format!("failed to read skill file: {e}"),
                details: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        }
    }

    fn handle_skill_search(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let needles = match extract_skill_search_queries(arguments) {
            Ok(needles) => needles,
            Err(message) => {
                return Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                });
            }
        };
        let search_content = cbor_map_bool(arguments, "search_content").unwrap_or(false);
        let hits = self.search_discovered_skills(&needles, search_content);

        let matches = CborValue::Array(
            hits.into_iter()
                .map(|(hit_count, name, description)| {
                    CborValue::Map(vec![
                        (CborValue::Text("name".to_owned()), CborValue::Text(name)),
                        (
                            CborValue::Text("description".to_owned()),
                            CborValue::Text(description),
                        ),
                        (
                            CborValue::Text("hit_count".to_owned()),
                            CborValue::Integer((hit_count as u64).into()),
                        ),
                    ])
                })
                .collect(),
        );
        let queries_echo =
            CborValue::Array(needles.iter().map(|n| CborValue::Text(n.clone())).collect());
        Event::ToolResult(tau_proto::ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            result: CborValue::Map(vec![
                (CborValue::Text("queries".to_owned()), queries_echo),
                (
                    CborValue::Text("search_content".to_owned()),
                    CborValue::Bool(search_content),
                ),
                (CborValue::Text("matches".to_owned()), matches),
            ]),
            originator: tau_proto::PromptOriginator::User,
        })
    }

    /// Score each discovered skill by how many of `needles` match its
    /// name, description, and (when `search_content`) body. A skill
    /// that matches more terms is more likely the right answer when
    /// the agent fired several plausible spellings at the same time
    /// ("commit", "git commit", "version control"). Returns
    /// `(hit_count, name, description)` rows sorted by descending
    /// hit count, with ties broken by name for deterministic output.
    ///
    /// Needles are expected to already be lowercased.
    fn search_discovered_skills(
        &self,
        needles: &[String],
        search_content: bool,
    ) -> Vec<(usize, String, String)> {
        let mut hits: Vec<(usize, &tau_proto::SkillName, &DiscoveredSkill)> = self
            .discovered_skills
            .iter()
            .filter_map(|(name, skill)| {
                let lower_name = name.as_str().to_lowercase();
                let lower_desc = skill.description.to_lowercase();
                // Read the body at most once across all needles, and
                // only when at least one needle didn't match in the
                // name or description and the caller opted in.
                let mut body: Option<String> = None;
                let hit_count = needles
                    .iter()
                    .filter(|needle| {
                        if lower_name.contains(needle.as_str())
                            || lower_desc.contains(needle.as_str())
                        {
                            return true;
                        }
                        if !search_content {
                            return false;
                        }
                        let body = body.get_or_insert_with(|| {
                            std::fs::read_to_string(&skill.file_path)
                                .map(|s| s.to_lowercase())
                                .unwrap_or_default()
                        });
                        body.contains(needle.as_str())
                    })
                    .count();
                (hit_count > 0).then_some((hit_count, name, skill))
            })
            .collect();
        hits.sort_by(|(ac, an, _), (bc, bn, _)| {
            bc.cmp(ac).then_with(|| an.as_str().cmp(bn.as_str()))
        });
        hits.into_iter()
            .map(|(hit_count, name, skill)| {
                (
                    hit_count,
                    name.as_str().to_owned(),
                    skill.description.clone(),
                )
            })
            .collect()
    }
}

/// Split a skill name into lowercased word-like needles by treating
/// `-` and `_` as separators. Used when an agent's `load` request
/// names a skill that doesn't exist: searching the discovered skills
/// for these needles often surfaces the one the agent actually
/// wanted (e.g. `dpc-rust-code-style` → `[dpc, rust, code, style]`).
/// Empty parts are dropped; duplicates are removed in first-seen
/// order so a name like `foo-foo` doesn't double-count itself.
fn split_skill_name_into_needles(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in name.split(['-', '_']) {
        if part.is_empty() {
            continue;
        }
        let lower = part.to_lowercase();
        if !out.iter().any(|existing| existing == &lower) {
            out.push(lower);
        }
    }
    out
}

/// Build the `details` payload for a failed `skill` load. Mirrors
/// the shape of a successful `search` result (`query`,
/// `search_content`, `matches`) so a UI that already knows how to
/// render skill-search hits can show the suggestion count next to
/// the error, and so the agent reading the details on its next turn
/// sees a familiar structure.
fn skill_load_not_found_details(
    name: &str,
    needles: &[String],
    matches: &[(usize, String, String)],
) -> CborValue {
    let matches_cbor = CborValue::Array(
        matches
            .iter()
            .map(|(hit_count, skill_name, description)| {
                CborValue::Map(vec![
                    (
                        CborValue::Text("name".to_owned()),
                        CborValue::Text(skill_name.clone()),
                    ),
                    (
                        CborValue::Text("description".to_owned()),
                        CborValue::Text(description.clone()),
                    ),
                    (
                        CborValue::Text("hit_count".to_owned()),
                        CborValue::Integer((*hit_count as u64).into()),
                    ),
                ])
            })
            .collect(),
    );
    let queries_echo =
        CborValue::Array(needles.iter().map(|n| CborValue::Text(n.clone())).collect());
    CborValue::Map(vec![
        (
            CborValue::Text("name".to_owned()),
            CborValue::Text(name.to_owned()),
        ),
        (CborValue::Text("queries".to_owned()), queries_echo),
        (
            CborValue::Text("search_content".to_owned()),
            CborValue::Bool(false),
        ),
        (CborValue::Text("matches".to_owned()), matches_cbor),
    ])
}

/// Parse the `query` argument of a `skill` tool call's `search` action
/// into one-or-more lowercased search needles. Accepts either a single
/// string (one needle) or an array of strings (multiple needles whose
/// hits are merged and ranked by hit-count). Returns a user-facing
/// error message string on missing/empty/malformed input.
fn extract_skill_search_queries(arguments: &CborValue) -> Result<Vec<String>, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("missing required argument: query (action=search)".to_owned());
    };
    let raw = entries
        .iter()
        .find_map(|(k, v)| match k {
            CborValue::Text(k) if k == "query" => Some(v),
            _ => None,
        })
        .ok_or_else(|| "missing required argument: query (action=search)".to_owned())?;

    let needles: Vec<String> = match raw {
        CborValue::Text(s) => vec![s.to_lowercase()],
        CborValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    CborValue::Text(s) => out.push(s.to_lowercase()),
                    _ => return Err("query array entries must all be strings".to_owned()),
                }
            }
            out
        }
        _ => {
            return Err("query must be a string or an array of strings (action=search)".to_owned());
        }
    };

    let needles: Vec<String> = needles.into_iter().filter(|n| !n.is_empty()).collect();
    if needles.is_empty() {
        return Err("query must include at least one non-empty term".to_owned());
    }
    Ok(needles)
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
            false,
            "s1",
        )?;
        harness.selected_model = "test/model".into();

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
        out.push_str(&format!("effort: {:?}\n\n", prompt.effort));

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

fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    let target_name = event.name();
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    })
}

fn should_replay_session_event_to_late_subscriber(event: &Event) -> bool {
    // Replay final, durable transcript facts, not progress. In
    // particular, skip `AgentResponseUpdated` streaming chunks and
    // `SessionPromptCreated` pending markers, but keep
    // `UiPromptSubmitted` and `AgentResponseFinished` so a resumed UI
    // can reconstruct completed turns.
    match event {
        Event::UiPromptSubmitted(_)
        | Event::SessionPromptSteered(_)
        | Event::SessionUserMessageInjected(_)
        | Event::ToolRequest(_)
        | Event::ToolResult(_)
        | Event::ToolError(_)
        | Event::ShellCommandFinished(_)
        | Event::SessionStarted(_)
        | Event::SessionShutdown(_)
        | Event::ExtAgentsMdAvailable(_)
        | Event::ExtensionContextReady(_)
        | Event::ExtensionEvent(_) => true,
        Event::AgentResponseFinished(response) => response.text.is_some(),
        _ => false,
    }
}
