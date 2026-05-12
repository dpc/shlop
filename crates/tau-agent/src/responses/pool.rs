//! WebSocket connection pool for the Codex Responses backend.
//!
//! See `TODO-codex-websocket.md` §2 for the design rationale. Recap:
//!
//! - The agent's `run()` loop is single-threaded and processes prompts
//!   serially, but it *alternates* between conversations (different sessions,
//!   sub-agent delegations interleaved with the parent). The OpenAI WS endpoint
//!   only caches the *most recent* `previous_response_id` per socket, so
//!   routing A → B → A on one shared socket would flush each chain's warmth on
//!   every switch. Keep one connection per `(account, session)` so warmth
//!   survives context-switches.
//! - Single owner = the agent loop. No `Mutex`/`Arc`/`DashMap`. Take the
//!   connection *out* of the map for the duration of one turn
//!   (`HashMap::remove`), put it back on success. Connection-in-flight
//!   exclusivity is enforced by ownership.
//! - Bounded by a soft cap (env-tunable `TAU_WS_POOL_MAX`,
//!   [`DEFAULT_POOL_MAX`]). LRU eviction when full.
//! - Connections age out near the server's 60-minute hard cap so a call doesn't
//!   fail mid-turn from the server slamming the door.
//! - Bearer-mismatch on checkout means OAuth refreshed; drop the stale socket
//!   and open a new one.

#![allow(dead_code)] // wired up by the agent's `run()` loop in a later step

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::ResponsesConfig;
use super::ws::WsConn;
use crate::common::LlmError;

/// Default soft cap on simultaneously-cached WS connections.
///
/// One per `(account, session)`. A typical interactive workload runs
/// 1–3 active sessions (the user's main + any in-flight sub-agent
/// delegation). The cap exists to bound pathological growth (a
/// long-lived agent process where the user reopens many old
/// sessions), not because the normal path needs many slots.
pub(crate) const DEFAULT_POOL_MAX: usize = 10;

/// Environment variable that overrides [`DEFAULT_POOL_MAX`] at
/// `WsPool::new()` time.
pub(crate) const POOL_MAX_ENV: &str = "TAU_WS_POOL_MAX";

/// Margin under the server's 60-minute hard cap before we
/// pre-emptively reopen a connection on checkout. Five minutes is
/// safer than cutting it close — a 59-minute-old connection that
/// dies *after* we send `response.create` surfaces as a mid-stream
/// `stream error` to the user, which a `<55min ? reuse : reopen`
/// check avoids entirely.
pub(crate) const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);

/// Pool key. A connection caches the previous_response of one
/// conversation chain; different chains get different sockets so
/// alternating between them preserves each chain's warm cache.
///
/// - `base_url` + `account_id` form a "socket realm" — same bearer, same
///   server-side state. Cross-realm reuse is impossible.
/// - `session_id` is the harness's per-conversation identifier. The harness
///   stamps it on every `SessionPromptCreated`; sub-agent delegations get their
///   own session_id and therefore their own slot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    pub base_url: String,
    pub account_id: Option<String>,
    pub session_id: String,
}

impl PoolKey {
    pub fn for_request(config: &ResponsesConfig, session_id: &str) -> Self {
        Self {
            base_url: config.base_url.clone(),
            account_id: config.account_id.clone(),
            session_id: session_id.to_owned(),
        }
    }
}

/// Single-threaded pool of WS connections.
///
/// Hot path (turn N+1 on a known session): `checkout` returns the
/// existing `WsConn` (removed from the map); the caller runs the
/// turn; on success it calls `release` to put the conn back at the
/// head of the LRU queue. On error (mid-stream close, IO break),
/// the caller drops the connection — the entry is already removed
/// from the map and the LRU list resyncs lazily.
pub(crate) struct WsPool {
    conns: HashMap<PoolKey, WsConn>,
    /// Front = most recent. Pruned of stale keys on `release` /
    /// `checkout` rather than eagerly — a key in the queue without
    /// a matching map entry just means that connection died and was
    /// dropped, so we skip it next time we walk the queue.
    lru: VecDeque<PoolKey>,
    max: usize,
    stats: WsPoolStats,
}

/// Lifetime counters for the WS pool. Bumped on each interesting
/// path so an operator can grep `tau_agent` tracing output and see
/// how often the silent-reconnect machinery kicked in (or, more
/// importantly, *kept* kicking in for a session — a runaway count
/// is the signature of an upstream regression).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WsPoolStats {
    /// Fresh sockets opened (pool miss, age-out, bearer-rotate, or
    /// the silent-reconnect path below).
    pub upgrades: u64,
    /// Cached sockets that died mid-turn and triggered the silent
    /// reopen-and-replay-without-chain-id recovery.
    pub silent_reconnects: u64,
    /// Times the fresh-socket path stripped a `previous_response_id`
    /// from the outgoing request because the new socket's chain
    /// cache was empty by definition.
    pub chain_strips_on_fresh: u64,
}

impl WsPool {
    pub fn new() -> Self {
        let max = std::env::var(POOL_MAX_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_POOL_MAX);
        Self {
            conns: HashMap::new(),
            lru: VecDeque::new(),
            max,
            stats: WsPoolStats::default(),
        }
    }

    /// Snapshot the running counters. Cheap (`Copy`); intended for
    /// tracing emission and tests.
    pub fn stats(&self) -> WsPoolStats {
        self.stats
    }

    /// Look up an existing connection for `key`, validating its
    /// bearer/age against the current request. Returns:
    ///
    /// - `Some(conn)` — caller owns it for the turn, must call
    ///   [`Self::release`] on success or drop on failure.
    /// - `None` — pool miss. Caller should `connect()` a fresh `WsConn` and
    ///   insert it via [`Self::release`] after the turn.
    ///
    /// Drops the entry if its bearer has rotated (OAuth refresh) or
    /// the connection is approaching the server-side age limit.
    pub fn checkout(&mut self, key: &PoolKey, current_bearer: &str) -> Option<WsConn> {
        let conn = self.conns.remove(key)?;
        // Bearer rotation: refreshed access token means upstream
        // would reject the existing socket on the next message
        // anyway. Drop and let caller reopen with the new token.
        if conn.bearer != current_bearer {
            self.purge_key(key);
            return None;
        }
        // Age-out: a 59-minute-old socket would die mid-stream.
        // Reopen here instead, before sending anything.
        if conn.opened_at.elapsed() >= MAX_CONNECTION_AGE {
            self.purge_key(key);
            return None;
        }
        // LRU bookkeeping: take the key out — caller will put it
        // back at the front on `release`.
        self.lru.retain(|k| k != key);
        Some(conn)
    }

    /// Put a connection (newly opened or just-used) back into the
    /// pool. Inserts at the LRU front. Evicts the LRU tail when the
    /// pool was already at capacity.
    pub fn release(&mut self, key: PoolKey, conn: WsConn) {
        if self.conns.len() >= self.max && !self.conns.contains_key(&key) {
            self.evict_lru();
        }
        // Lazy-prune: if a stale copy of this key is somewhere in
        // the queue (e.g. it was age-purged earlier), drop it so we
        // don't double-count.
        self.lru.retain(|k| k != &key);
        self.lru.push_front(key.clone());
        self.conns.insert(key, conn);
    }

    /// Drop every cached connection. Cheap full reset — used when
    /// the resolver issues a token refresh and we want to invalidate
    /// every socket in one shot (alternative: per-entry bearer check
    /// on checkout, which is what [`Self::checkout`] does today).
    pub fn flush(&mut self) {
        self.conns.clear();
        self.lru.clear();
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    fn purge_key(&mut self, key: &PoolKey) {
        self.conns.remove(key);
        self.lru.retain(|k| k != key);
    }

    fn evict_lru(&mut self) {
        // Walk the LRU tail forward until we find a key still
        // backed by the map. Stale keys (entry removed earlier
        // without queue update) are silently skipped.
        while let Some(stale) = self.lru.pop_back() {
            if self.conns.remove(&stale).is_some() {
                return;
            }
        }
    }
}

impl Default for WsPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience wrapper that wires `checkout` → `WsConn::run_turn` →
/// `release` together with reopen-on-miss semantics. The agent's
/// `run()` loop calls this; tests can call it directly with a fake
/// `WsConn::connect` impl by exercising the lower-level methods.
///
/// Transparent reconnect: the Codex WS endpoint's
/// `previous_response_id` cache is **connection-local** (per the
/// OpenAI deployment-checklist WS guide). Two consequences this
/// function has to handle:
///
/// 1. A fresh socket from `WsConn::connect` has an empty chain cache, so a
///    `previous_response_id` carried in `request` would 404 on the server. We
///    strip the field before sending on any just-opened connection.
/// 2. If a cached socket dies mid-turn (server-side 60-minute reap, keepalive
///    timeout, transport reset), the in-flight chain id is gone with it. Rather
///    than surfacing the failure to the outer retry loop — which would just
///    resend the same dead chain id on the next attempt and 404 again — we
///    reopen here once and replay the turn with the chain id stripped. Only
///    persistent failures leak out.
pub(crate) fn run_turn_through_pool(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session_id: &str,
    request: &crate::common::PromptPayload<'_>,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<crate::common::StreamState, LlmError> {
    let key = PoolKey::for_request(config, session_id);

    // First attempt: prefer a warm cached connection so the
    // connection-local chain cache stays useful.
    if let Some(mut conn) = pool.checkout(&key, &config.api_key) {
        match conn.run_turn(config, request, on_update) {
            Ok(state) => {
                pool.release(key, conn);
                return Ok(state);
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                pool.stats.silent_reconnects += 1;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
                    error = %err,
                    silent_reconnects = pool.stats.silent_reconnects,
                    "Codex WS connection lost mid-turn; reopening and replaying without chain id",
                );
                drop(conn);
                // Fall through to the fresh-open path below.
            }
            Err(other) => {
                drop(conn);
                return Err(other);
            }
        }
    }

    // Fresh socket path. The chain cache here is empty by definition,
    // so the in-request chain id (if any) is invalid for this
    // connection — replay the full slice instead.
    let mut conn = WsConn::connect(config)?;
    pool.stats.upgrades += 1;
    let fresh_request = without_previous_response(request);
    if request.previous_response.is_some() {
        pool.stats.chain_strips_on_fresh += 1;
        tracing::debug!(
            target: crate::LOG_TARGET,
            session_id,
            upgrades = pool.stats.upgrades,
            chain_strips_on_fresh = pool.stats.chain_strips_on_fresh,
            "fresh Codex WS socket; stripping previous_response_id from outgoing request",
        );
    }
    match conn.run_turn(config, &fresh_request, on_update) {
        Ok(state) => {
            pool.release(key, conn);
            Ok(state)
        }
        Err(err) => {
            drop(conn);
            Err(err)
        }
    }
}

/// Errors from `WsConn::run_turn` that mean "this socket is dead,
/// but the *next* socket can probably serve the turn." Caller's job
/// is to reopen and retry once silently rather than letting the outer
/// retry loop burn a backoff on the same broken state.
///
/// Two flavors land here:
/// - Transport-level: tungstenite raised `ConnectionClosed`, `AlreadyClosed`,
///   or an IO break; or the server sent a close frame mid-stream. All surface
///   as `HttpStatus(0, "stream error: ws closed..." | "stream error: <io>")`
///   from [`super::ws::map_ws_runtime_error`].
/// - Server-level stale-chain: an `error` event whose message says the
///   `previous_response_id` we just sent doesn't exist on this socket. Same
///   root cause (the previous socket carrying that chain id is gone), just
///   surfaced through the JSON event stream instead of a TCP close.
fn is_recoverable_ws_error(err: &LlmError) -> bool {
    let LlmError::HttpStatus(0, body) = err else {
        return false;
    };
    if !body.starts_with("stream error:") {
        return false;
    }
    body.contains("ws closed")
        || body.contains("Previous response")
        || body.contains("previous_response")
        || body.contains("response not found")
}

/// Borrow `request` but blank out its `previous_response`. Used on
/// the fresh-socket path where the chain id from a prior connection
/// is guaranteed invalid (connection-local cache).
fn without_previous_response<'a>(
    request: &crate::common::PromptPayload<'a>,
) -> crate::common::PromptPayload<'a> {
    crate::common::PromptPayload {
        previous_response: None,
        system_prompt: request.system_prompt,
        messages: request.messages,
        tools: request.tools,
        params: request.params,
        tool_choice: request.tool_choice,
        originator: request.originator,
        session_id: request.session_id,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use tungstenite::Message;

    use super::*;
    use crate::common::PromptPayload;

    #[test]
    fn keys_distinguish_sessions_under_same_account() {
        let cfg = make_config("https://chatgpt.com/backend-api", Some("acc"));
        let a = PoolKey::for_request(&cfg, "session-a");
        let b = PoolKey::for_request(&cfg, "session-b");
        assert_ne!(a, b);
    }

    #[test]
    fn keys_distinguish_accounts_under_same_session() {
        let a = PoolKey::for_request(
            &make_config("https://chatgpt.com/backend-api", Some("acc-1")),
            "session",
        );
        let b = PoolKey::for_request(
            &make_config("https://chatgpt.com/backend-api", Some("acc-2")),
            "session",
        );
        assert_ne!(a, b);
    }

    /// The headline pool invariant: alternating between two sessions
    /// must NOT cause the second session's turn to flush the first
    /// session's connection. Each `(account, session)` must hold its
    /// own socket so the OpenAI connection-local
    /// `previous_response_id` cache stays warm across context
    /// switches.
    #[test]
    fn pool_routes_each_session_to_its_own_socket_and_reuses_them() {
        let (addr, server) = spawn_fake_codex_server();
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let mut on_update = |_: &str, _: Option<&str>| {};

        // Two turns on session A, interleaved with one on session B.
        // Expected: 2 upgrades total (one per session), 3 turns.
        for session in ["session-a", "session-b", "session-a"] {
            let session_id = tau_proto::SessionId::new(session);
            let request = PromptPayload {
                system_prompt: "sys",
                messages: &[],
                tools: &[],
                params: tau_proto::ModelParams::default(),
                tool_choice: tau_proto::ToolChoice::default(),
                previous_response: None,
                originator: &tau_proto::PromptOriginator::User,
                session_id: &session_id,
            };
            run_turn_through_pool(&mut pool, &config, session, &request, &mut on_update)
                .expect("turn ok");
        }

        let state = server.lock().unwrap();
        assert_eq!(
            state.upgrade_count, 2,
            "expected one upgrade per distinct session_id (alternating A/B/A — reuses A's socket)"
        );
        assert_eq!(
            state.turns_per_connection,
            vec![2, 1],
            "session-a's socket should have served two turns; session-b's, one"
        );
    }

    /// Cap the pool at 2 and exercise three sessions. The
    /// least-recently-used session's socket must get evicted; a
    /// follow-up turn on that session triggers a fresh upgrade.
    #[test]
    fn pool_evicts_lru_when_capacity_exceeded() {
        let (addr, server) = spawn_fake_codex_server();
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        pool.max = 2;
        let mut on_update = |_: &str, _: Option<&str>| {};

        // A → B → C: three different sessions, cap=2.
        // After C: A (LRU) is evicted, pool holds {B, C}.
        for session in ["a", "b", "c"] {
            run_turn(&mut pool, &config, session, &mut on_update);
        }
        assert_eq!(pool.len(), 2);
        assert_eq!(server.lock().unwrap().upgrade_count, 3);

        // Touching A again must re-upgrade (its old socket got
        // evicted on C's release).
        run_turn(&mut pool, &config, "a", &mut on_update);
        assert_eq!(server.lock().unwrap().upgrade_count, 4);
    }

    /// Connections older than `MAX_CONNECTION_AGE` must be
    /// pre-emptively reopened on checkout, so the server's 60-min
    /// hard cap never fires mid-turn.
    #[test]
    fn pool_reopens_aged_out_connections_on_checkout() {
        let (addr, server) = spawn_fake_codex_server();
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let mut on_update = |_: &str, _: Option<&str>| {};

        // First turn opens connection #1.
        run_turn(&mut pool, &config, "session-aged", &mut on_update);
        assert_eq!(server.lock().unwrap().upgrade_count, 1);

        // Forcibly age the cached connection past the threshold.
        let key = PoolKey::for_request(&config, "session-aged");
        if let Some(conn) = pool.conns.get_mut(&key) {
            conn.opened_at =
                std::time::Instant::now() - MAX_CONNECTION_AGE - Duration::from_secs(1);
        } else {
            panic!("expected connection in pool");
        }

        // Next turn must reopen rather than send on the stale socket.
        run_turn(&mut pool, &config, "session-aged", &mut on_update);
        assert_eq!(
            server.lock().unwrap().upgrade_count,
            2,
            "aged-out connection should have been replaced"
        );
    }

    /// HTTP+SSE base + plain TCP fake server doubles as the WS
    /// transport's smoke test: connect, send a turn, read all the
    /// expected events back, see `response_id` captured.
    #[test]
    fn ws_turn_captures_response_id_for_chain_continuation() {
        let (addr, _server) = spawn_fake_codex_server();
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let mut last_text = String::new();
        let mut on_update = |text: &str, _thinking: Option<&str>| {
            last_text = text.to_owned();
        };

        let session_id = tau_proto::SessionId::new("session-x");
        let request = PromptPayload {
            system_prompt: "sys",
            messages: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            previous_response: None,
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
        };

        let state =
            run_turn_through_pool(&mut pool, &config, "session-x", &request, &mut on_update)
                .expect("turn ok");
        assert_eq!(last_text, "hello");
        assert!(
            state.response_id.is_some(),
            "response_id must be captured so the next turn can chain via previous_response_id"
        );
    }

    /// Codex's WS `previous_response_id` cache is connection-local.
    /// When the pool opens a fresh socket — whether the first turn on
    /// a session, or a reopen after the previous socket died — the
    /// new socket has no knowledge of any prior response id. Sending
    /// one would 404 ("Previous response with id ... not found"),
    /// which is exactly what the production session-debug log showed
    /// before this fix. The pool must therefore strip the chain id
    /// before sending on any just-opened connection.
    #[test]
    fn fresh_open_strips_previous_response_id_from_request() {
        let (addr, server) = spawn_fake_codex_server();
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let mut on_update = |_: &str, _: Option<&str>| {};

        let session_id = tau_proto::SessionId::new("session-fresh");
        let request = PromptPayload {
            system_prompt: "sys",
            messages: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            previous_response: Some(crate::common::PreviousResponse {
                id: "resp_from_a_dead_socket",
                message_index: 0,
            }),
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
        };
        run_turn_through_pool(
            &mut pool,
            &config,
            "session-fresh",
            &request,
            &mut on_update,
        )
        .expect("turn ok");

        let s = server.lock().unwrap();
        let body = &s.requests[0];
        assert!(
            body.get("previous_response_id").is_none(),
            "fresh-open path must not forward previous_response_id to a brand-new socket; got {body}"
        );
        assert_eq!(
            pool.stats().chain_strips_on_fresh,
            1,
            "stat counter should record the strip"
        );
    }

    /// A cached connection dies mid-turn (keepalive timeout / TCP
    /// reset). The pool must reopen and replay once *silently* — no
    /// error returned to the outer retry loop, no user-visible retry
    /// banner — and the replay must drop the in-flight chain id
    /// because the new socket has no record of it.
    #[test]
    fn mid_stream_close_triggers_silent_reconnect_without_chain_id() {
        let (addr, server) = spawn_fake_codex_server();
        // Make connection #0 die mid-turn-#2 (after_turn=1 -> the
        // second arriving turn on conn 0 is the one that gets closed).
        server.lock().unwrap().fault = Some(MidStreamCloseFault {
            on_conn_index: 0,
            after_turn: 1,
        });
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let mut on_update = |_: &str, _: Option<&str>| {};

        // Turn 1: opens conn-0, returns a `response_id` the harness
        // would chain off for turn 2.
        let session_id = tau_proto::SessionId::new("session-die");
        let req1 = PromptPayload {
            system_prompt: "sys",
            messages: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            previous_response: None,
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
        };
        let state1 =
            run_turn_through_pool(&mut pool, &config, "session-die", &req1, &mut on_update)
                .expect("first turn ok");
        let prev_id = state1.response_id.expect("first turn yielded response_id");

        // Turn 2: harness wants to chain via `prev_id`. The cached
        // socket dies mid-stream; pool must transparently reopen and
        // replay without the chain id and return Ok.
        let req2 = PromptPayload {
            system_prompt: "sys",
            messages: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            previous_response: Some(crate::common::PreviousResponse {
                id: &prev_id,
                message_index: 0,
            }),
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
        };
        run_turn_through_pool(&mut pool, &config, "session-die", &req2, &mut on_update)
            .expect("silent reconnect should make this Ok");

        let s = server.lock().unwrap();
        assert_eq!(
            s.upgrade_count, 2,
            "mid-stream close should have forced a reopen"
        );
        // Three captured requests in arrival order:
        //   #0: turn-1 on conn-0 (no chain id, no prior response)
        //   #1: turn-2 on conn-0 (had chain id; this is the one that died)
        //   #2: replay on conn-1 (must have chain id stripped)
        assert_eq!(s.requests.len(), 3, "expected three request envelopes");
        assert!(
            s.requests[1].get("previous_response_id").is_some(),
            "turn-2 on the warm socket should still carry the chain id (warm cache path)"
        );
        assert!(
            s.requests[2].get("previous_response_id").is_none(),
            "post-reconnect replay must drop chain id; got {req}",
            req = s.requests[2]
        );
        assert_eq!(
            pool.stats().silent_reconnects,
            1,
            "stat counter should record the silent reconnect"
        );
    }

    // -----------------------------------------------------------------
    // Fake Codex server: minimal blocking tungstenite acceptor.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct ServerState {
        /// How many TCP+upgrade pairs we've accepted. Each
        /// `(account, session)` pair the pool keys against should
        /// produce exactly one upgrade across its lifetime (modulo
        /// age-out / OAuth refresh).
        upgrade_count: usize,
        /// `turns_per_connection[i]` is the number of
        /// `response.create` envelopes connection `i` served before
        /// closing. Lets pool-reuse tests assert that A's two turns
        /// landed on one socket.
        turns_per_connection: Vec<usize>,
        /// Captured request bodies, in arrival order across all
        /// connections. Available for tests that want to inspect
        /// what the client actually sent (chain ids, model knobs).
        requests: Vec<serde_json::Value>,
        /// Fault injection. When `Some`, the worker for a matching
        /// connection drops the socket with a 1011 close frame
        /// instead of serving the offending turn — mimicking the
        /// "keepalive ping timeout" the live Codex server produces
        /// when its idle reaper fires. Tests use this to exercise
        /// the silent-reconnect path.
        fault: Option<MidStreamCloseFault>,
    }

    /// "After connection index `on_conn_index` has fully served
    /// `after_turn` turns, drop the next incoming turn mid-stream."
    /// Indices are zero-based; `after_turn: 1` means the second
    /// arriving turn on that connection is the one that gets killed.
    #[derive(Clone, Copy)]
    struct MidStreamCloseFault {
        on_conn_index: usize,
        after_turn: usize,
    }

    fn spawn_fake_codex_server() -> (SocketAddr, Arc<Mutex<ServerState>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let conn_state = state_clone.clone();
                thread::spawn(move || handle_one_connection(stream, conn_state));
            }
        });
        (addr, state)
    }

    fn handle_one_connection(stream: TcpStream, state: Arc<Mutex<ServerState>>) {
        let mut ws = match tungstenite::accept(stream) {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let conn_idx;
        {
            let mut s = state.lock().unwrap();
            s.upgrade_count += 1;
            conn_idx = s.turns_per_connection.len();
            s.turns_per_connection.push(0);
        }

        let mut turn_counter = 0_usize;
        loop {
            let msg = match ws.read() {
                Ok(m) => m,
                Err(_) => return,
            };
            match msg {
                Message::Text(text) => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(text.as_str()).unwrap_or(serde_json::Value::Null);
                    let fault_now = {
                        let mut s = state.lock().unwrap();
                        s.requests.push(parsed.clone());
                        s.turns_per_connection[conn_idx] += 1;
                        s.fault
                            .filter(|f| f.on_conn_index == conn_idx && turn_counter >= f.after_turn)
                    };
                    turn_counter += 1;
                    if fault_now.is_some() {
                        // Mimic the live Codex 1011 keepalive-timeout
                        // drop: send a close frame and bail without
                        // streaming the response body. Client side
                        // sees `Message::Close` → `LlmError(0, "stream
                        // error: ws closed mid-stream ...")`.
                        let _ = ws.send(Message::Close(Some(tungstenite::protocol::CloseFrame {
                            code: tungstenite::protocol::frame::coding::CloseCode::Error,
                            reason: "keepalive ping timeout".into(),
                        })));
                        return;
                    }
                    // Stream a tiny canned event sequence: one
                    // visible-text delta, then completed.
                    let events = [
                        serde_json::json!({
                            "type": "response.output_text.delta",
                            "delta": "hello",
                        }),
                        serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": format!("resp_{conn_idx}_{turn_counter}"),
                                "usage": {
                                    "input_tokens": 1,
                                    "output_tokens": 1,
                                    "input_tokens_details": { "cached_tokens": 0 },
                                },
                            },
                        }),
                    ];
                    for ev in events {
                        let txt = serde_json::to_string(&ev).expect("serialize");
                        if ws.send(Message::Text(txt.into())).is_err() {
                            return;
                        }
                    }
                }
                Message::Close(_) => return,
                _ => continue,
            }
        }
    }

    fn run_turn(
        pool: &mut WsPool,
        config: &ResponsesConfig,
        session: &str,
        on_update: &mut impl FnMut(&str, Option<&str>),
    ) {
        let session_id = tau_proto::SessionId::new(session);
        let request = PromptPayload {
            system_prompt: "sys",
            messages: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            previous_response: None,
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
        };
        run_turn_through_pool(pool, config, session, &request, on_update).expect("turn ok");
    }

    fn make_config(base_url: &str, account_id: Option<&str>) -> ResponsesConfig {
        ResponsesConfig {
            base_url: base_url.into(),
            api_key: "test".into(),
            model_id: "gpt-5-codex".into(),
            account_id: account_id.map(str::to_owned),
            supports_reasoning_effort: false,
            supports_reasoning_summary: false,
            supports_verbosity: false,
            supports_phase: false,
            supports_websocket: true,
            prompt_cache_key: None,
            prompt_cache_retention: None,
        }
    }
}
