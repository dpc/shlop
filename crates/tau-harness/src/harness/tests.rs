use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    RoutedFrame, SessionEntry, ToolActivityOutcome, ToolActivityRecord,
};
use tau_proto::{
    AgentResponseFinished, AgentResponseUpdated, AgentToolCall, CborValue, Disconnect, Event,
    EventSelector, ExtAgentQuery, Frame, FrameReader, FrameWriter, Intercept, InterceptAction,
    InterceptReply, InterceptionPriority, Message, SessionPromptCreated, SessionPromptId,
    SessionPromptQueued, Subscribe, ToolCallId, ToolName, ToolResult, ToolSideEffects, ToolSpec,
    UiPromptDraft, UiPromptSubmitted,
};
use tempfile::TempDir;

use super::Harness;
use crate::conversation::ConversationTurnState;
use crate::daemon::{
    ServeOptions, bind_listener, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message, send_daemon_message_with_trace,
};
use crate::dirs::{
    default_session_id, open_session_store, policy_lines, session_lines, session_list_lines,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::format_session_entry;
use crate::model::selected_effort_for_model;
use crate::prompt::build_system_prompt;
use crate::turn::{PromptSubmission, TurnState};

fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    tau_agent::run_echo(r, w).map_err(|e| e.to_string())
}

/// Test-only helper that pushes a `UiPromptSubmitted` through the
/// harness's normal publish path, which writes the durable per-session
/// event and folds it into the SessionTree. Production code reaches
/// the same place via `dispatch_user_prompt`; tests use this when
/// they want a tree node without driving the full agent turn.
fn append_user_message_via_event(h: &mut Harness, session_id: &str, text: &str) {
    h.publish_event(
        None,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: session_id.into(),
            text: text.to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
}

fn echo_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_for("s1", state_dir)
}

fn echo_harness_for(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    Harness::new_with_agent(
        state_dir,
        tau_config::settings::TauDirs::default(),
        echo_runner,
        session_id,
    )
}

struct TestSink {
    events: Arc<Mutex<Vec<RoutedFrame>>>,
}

impl ConnectionSink for TestSink {
    fn send(&mut self, event: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.events.lock().expect("sink mutex").push(event);
        Ok(())
    }
}

fn connect_test_tool(h: &mut Harness, name: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

/// Pre-seed the per-conversation `AgentThinking` state for tests that
/// bypass `dispatch_prompt_for_conversation` and call response handlers
/// directly.
fn seed_agent_thinking(h: &mut Harness, cid: &crate::conversation::ConversationId, spid: &str) {
    h.conversations
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = ConversationTurnState::AgentThinking {
        session_prompt_id: spid.into(),
    };
}

/// Pre-seed the per-conversation `ToolsRunning` state for tests that
/// bypass the agent-response path and call tool handlers directly.
fn seed_tools_running(
    h: &mut Harness,
    cid: &crate::conversation::ConversationId,
    remaining: Vec<ToolCallId>,
) {
    h.conversations
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = ConversationTurnState::ToolsRunning {
        remaining_calls: remaining,
    };
}

#[test]
fn format_session_entry_tree_preview_hides_call_id_and_shows_skill_name() {
    let skill_request = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_HC8dStLuLeEjHCxFZsBx6jfV".into(),
        tool_name: "skill".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("load".to_owned()),
                ),
                (
                    CborValue::Text("name".to_owned()),
                    CborValue::Text("jujutsu".to_owned()),
                ),
            ]),
        },
    });
    assert_eq!(
        format_session_entry(&skill_request),
        "tool.request skill jujutsu"
    );

    let skill_search = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_search".into(),
        tool_name: "skill".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("search".to_owned()),
                ),
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("commit".to_owned()),
                ),
            ]),
        },
    });
    assert_eq!(
        format_session_entry(&skill_search),
        "tool.request skill search commit"
    );

    let read_request = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_ugly".into(),
        tool_name: "read".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("foo.txt".to_owned()),
            )]),
        },
    });
    assert_eq!(format_session_entry(&read_request), "tool.request read");

    let result = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_ugly".into(),
        tool_name: "read".into(),
        outcome: ToolActivityOutcome::Result {
            result: CborValue::Text("hello".to_owned()),
        },
    });
    assert_eq!(format_session_entry(&result), "tool.result read -> hello");
}

#[test]
fn embedded_mode_returns_agent_response_and_persists_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "hello")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "response should not be empty: {r:?}");
    let store = open_session_store(&sp).expect("reopen");
    let branch = store.session("s1").expect("session").current_branch();
    assert!(
        branch.len() >= 2,
        "should have user msg + agent response, got {}",
        branch.len()
    );
}

#[test]
fn daemon_mode_accepts_later_clients() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(2).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3), "socket timeout");
        thread::sleep(Duration::from_millis(10));
    }

    let r1 = send_daemon_message(&sock, "s1", "hello").expect("first");
    let r2 = send_daemon_message(&sock, "s1", "again").expect("second");
    assert_eq!(r1, "hello", "first cycle should echo our submission");
    assert_eq!(r2, "again", "second cycle should echo our submission");

    server.join().expect("join").expect("daemon clean exit");
    let store = open_session_store(&sp).expect("reopen");
    let branch = store.session("s1").expect("session").current_branch();
    // System AGENTS.md preamble + 2 × (user, tool.req, tool.res, agent).
    assert_eq!(
        branch.len(),
        9,
        "expected full two-cycle persistence, got {branch:?}"
    );
}

#[test]
fn embedded_mode_can_read_files() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let fp = td.path().join("note.txt");
    std::fs::write(&fp, "hello from disk").expect("write fixture");
    let r = run_embedded_message_with_echo(&sp, "s1", &format!("read {}", fp.display()))
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "read response should not be empty");
    assert!(r.contains("hello from disk"));
}

#[test]
fn embedded_mode_can_run_shell_commands() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "shell printf hi")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "shell response should not be empty");
}

#[test]
fn unavailable_tool_is_reported_without_crashing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let removed = h.registry.unregister_connection(&conn_id);
    assert!(removed.iter().any(|t| t == "shell"));

    let outcome = h
        .send_user_message("s1", "shell printf hi", None)
        .expect("should succeed with error");
    assert!(outcome.response.contains("no live provider available"));
    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_completes_pending_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let call_id: ToolCallId = "call-1".into();
    let tool_name: ToolName = "shell".into();
    let cid = h.default_conversation_id.clone();
    h.tool_conversations.insert(call_id.clone(), cid.clone());
    h.pending_tool_names
        .insert(call_id.clone(), tool_name.clone());
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.in_flight_tool_kinds
        .insert(call_id.clone(), tau_proto::ToolSideEffects::Mutating);
    if let Some(conv) = h.conversations.get_mut(&cid) {
        conv.turn_state = ConversationTurnState::ToolsRunning {
            remaining_calls: vec![call_id.clone()],
        };
    }

    h.handle_disconnect(&conn_id);

    // Disconnect publishes a ToolError, drops the call from the
    // conversation's `ToolsRunning` set, and — since that was the
    // last outstanding call — re-prompts the agent so it can react
    // to the failure. The conversation therefore transitions
    // `ToolsRunning -> AgentThinking`, not back to `Idle`.
    assert!(matches!(h.turn_state, TurnState::Idle));
    assert!(matches!(
        h.conversations
            .get(&h.default_conversation_id)
            .expect("default conversation")
            .turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    assert!(!h.tool_conversations.contains_key(&call_id));
    assert!(!h.pending_tool_providers.contains_key(&call_id));

    let branch = h.store.session("s1").expect("session").current_branch();
    assert!(branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                call_id: logged_call_id,
                outcome: ToolActivityOutcome::Error { message, .. },
                ..
            }) if logged_call_id == &call_id && message == "tool provider disconnected"
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_is_removed_cleanly() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Send disconnect to the extension via the bus (through the
    // writer channel → writer thread → stream).
    let _ = h.bus.send_to(
        &conn_id,
        None,
        Frame::Message(Message::Disconnect(Disconnect {
            reason: Some("test".to_owned()),
        })),
    );

    // Drive event loop until the disconnect arrives.
    let started = Instant::now();
    loop {
        let event =
            h.rx.recv_timeout(Duration::from_secs(2))
                .expect("should get disconnect");
        match event {
            HarnessEvent::Disconnected {
                ref connection_id, ..
            } if *connection_id == conn_id => {
                h.handle_disconnect(&conn_id);
                break;
            }
            HarnessEvent::FromConnection {
                connection_id,
                frame,
            } => {
                let _ = h.handle_extension_event(&connection_id, *frame);
            }
            _ => {}
        }
        assert!(started.elapsed() < Duration::from_secs(2), "timeout");
    }

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(h.registry.providers_for("shell").is_empty());
    assert!(
        h.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell exited")
    );

    let outcome = h
        .send_user_message("s1", "shell printf hi", None)
        .expect("should succeed with error");
    assert!(outcome.response.contains("no live provider available"));
    h.shutdown().expect("shutdown");
}

#[test]
fn traced_embedded_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
    assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
    assert!(!o.response.is_empty(), "shell response should not be empty");
}

#[test]
fn traced_daemon_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let o = send_daemon_message_with_trace(&sock, "s1", "shell printf hi").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell ready")
    );
    assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
    assert!(!o.response.is_empty(), "shell response should not be empty");
    server.join().expect("join").expect("clean exit");
}

#[test]
fn traced_embedded_reports_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "hello").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent starting")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent exited")
    );
}

#[test]
fn session_and_policy_lines_are_printable() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let _ = send_daemon_message_with_trace(&sock, "s1", "hello").expect("ok");
    server.join().expect("join").expect("clean exit");

    let sl = session_lines(&sp, "s1").expect("lines");
    assert!(sl.iter().any(|l| l.contains("user: hello")));
    assert!(sl.iter().any(|l| l.contains("tool.request echo")));
    let sll = session_list_lines(&sp).expect("list");
    assert!(sll.iter().any(|l| l.contains("s1 (5 entries)")));
    let pl = policy_lines(sp.join("policy.cbor")).expect("policy");
    assert!(pl.iter().any(|l| l.contains("socket-ui")));
}

#[test]
fn empty_session_and_policy_views() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    std::fs::create_dir_all(&sp).expect("mkdir");
    assert_eq!(session_list_lines(&sp).expect("ok"), vec!["no sessions"]);
    assert_eq!(
        policy_lines(sp.join("policy.cbor")).expect("ok"),
        vec!["no policy approvals"]
    );
    assert_eq!(
        session_lines(&sp, "x").expect("ok"),
        vec!["session x not found"]
    );
}

#[test]
fn daemon_disconnect_reason_is_reported() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let listener = bind_listener(&sock).expect("bind");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let read_stream = stream.try_clone().expect("clone");
        let mut reader = FrameReader::new(BufReader::new(read_stream));
        let mut writer = FrameWriter::new(BufWriter::new(stream));
        let _ = reader.read_frame(); // hello
        let _ = reader.read_frame(); // subscribe
        let _ = reader.read_frame(); // message
        writer
            .write_frame(&Frame::Message(Message::Disconnect(Disconnect {
                reason: Some("test disconnect".to_owned()),
            })))
            .expect("write");
        writer.flush().expect("flush");
    });

    let err =
        send_daemon_message_with_trace(&sock, "s1", "hello").expect_err("should get disconnect");
    assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
    server.join().expect("join");
}

// -- AGENTS.md --

#[test]
fn agents_context_is_injected_at_session_init() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Eager init at construction may have already appended a real
    // AGENTS.md (ext-shell walks the test cwd). Clear so we assert
    // only on the test-injected pair below.
    h.discovered_agents_files.clear();
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/AGENTS.md"),
        content: "# Root\n- root rule\n".to_owned(),
    });
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/pkg/AGENTS.md"),
        content: "# Package\n- package rule\n".to_owned(),
    });
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
            },
        )),
    )
    .expect("ready");

    assert!(matches!(h.turn_state, TurnState::Idle));

    let branch = h.store.session("s1").expect("session").current_branch();
    let injected = branch
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEntry::UserMessage { text }
                if text.starts_with("# AGENTS.md instructions")
                    && text.contains("/repo/AGENTS.md") =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .expect("expected injected AGENTS.md user message");
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
    let root_pos = injected.find("root rule").expect("root rule");
    let pkg_pos = injected.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn cross_session_prompt_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.selected_model = "test/model".into();
    let submission = h
        .submit_user_prompt("chat-1".into(), "hello".to_owned())
        .expect("submit");
    match submission {
        PromptSubmission::Rejected { reason } => {
            assert!(reason.contains("s1"), "reason should name bound session");
            assert!(reason.contains("chat-1"), "reason should name rejected id");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(
        h.conversations
            .get(&h.default_conversation_id)
            .expect("default conversation")
            .pending_prompts
            .is_empty(),
        "rejected prompt must not queue"
    );
    assert!(
        h.store.session("chat-1").is_none(),
        "rejected session must not be created"
    );

    h.shutdown().expect("shutdown");
}

// -- Eager session init --

#[test]
fn harness_startup_eagerly_initializes_eager_session() {
    // Guards against the recurring "this looks like redundant work"
    // urge to lazy-ify session init. `echo_harness` calls
    // `Harness::new_with_agent`, which must eagerly initialize the
    // session before returning — see the design-choice comment in
    // the constructor for why.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");

    assert!(
        h.initialized_sessions.contains("s1"),
        "eager init should mark the bound session as initialized at startup; \
         `initialized_sessions` was {:?}",
        h.initialized_sessions
    );
    assert!(
        matches!(h.turn_state, TurnState::Idle),
        "turn state should be Idle after eager init completes"
    );
}

#[test]
fn late_joining_ui_client_receives_replayed_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.send_user_message("s1", "hello replay", None)
        .expect("send message");

    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events
            .iter()
            .any(|entry| matches!(entry.event, Event::UiPromptSubmitted(_))),
        "user prompt should be in durable session event log"
    );
    assert!(
        events
            .iter()
            .any(|entry| matches!(entry.event, Event::AgentResponseFinished(_))),
        "final agent response should be in durable session event log"
    );
    assert!(
        events.iter().all(|entry| !entry.event.is_transient()),
        "transient events must not be persisted"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        Frame::Message(Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("ui.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = FrameReader::new(BufReader::new(client_end));
    let mut got_prompt = false;
    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !(got_prompt && got_response) {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let (_log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::UiPromptSubmitted(prompt)) if prompt.text == "hello replay" => {
                got_prompt = true;
            }
            Frame::Event(Event::AgentResponseFinished(finished))
                if finished
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("hello replay")) =>
            {
                got_response = true;
            }
            _ => {}
        }
    }

    assert!(got_prompt, "late UI should replay prior user prompt");
    assert!(got_response, "late UI should replay prior agent response");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_only_final_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let spid: SessionPromptId = "sp-replay".into();
    h.prompt_conversations
        .insert(spid.clone(), h.default_conversation_id.clone());
    h.publish_event(
        None,
        Event::SessionPromptQueued(SessionPromptQueued {
            session_id: "s1".into(),
            text: "queued but not durable-final".to_owned(),
        }),
    );
    h.publish_event(
        None,
        Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: spid.clone(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: Default::default(),
            thinking_summary: Default::default(),
            originator: Default::default(),
            ctx_id: None,
        }),
    );
    h.publish_event(
        None,
        Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: spid.clone(),
            text: "partial".to_owned(),
            thinking: None,
            originator: Default::default(),
        }),
    );
    h.publish_event(
        None,
        Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: spid,
            text: Some("final".to_owned()),
            tool_calls: Vec::new(),
            originator: Default::default(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
        }),
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        Frame::Message(Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("session.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = FrameReader::new(BufReader::new(client_end));
    let mut replayed = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let (_log_id, inner) = frame.peel_log();
        if let Frame::Event(event) = inner {
            replayed.push(event.name());
        }
    }

    assert!(replayed.contains(&tau_proto::EventName::AGENT_RESPONSE_FINISHED));
    assert!(!replayed.contains(&tau_proto::EventName::SESSION_PROMPT_QUEUED));
    assert!(!replayed.contains(&tau_proto::EventName::SESSION_PROMPT_CREATED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_RESPONSE_UPDATED));

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_receives_replayed_agents_md_and_context_ready() {
    // The CLI connects after the daemon's eager init has already
    // fired, so live subscription alone would miss
    // `ExtAgentsMdAvailable` and `ExtensionContextReady`. The
    // subscribe handler must replay them — currently via the
    // durable per-session log (`replay_session_events`) — so the UI
    // still renders the "loaded: …" / "session context ready" lines.
    //
    // Each event must arrive exactly once. They used to be replayed
    // by both `replay_session_events` and `replay_harness_info`,
    // which made the CLI render every line twice on startup.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Inject synthetic discovery events as if ext-shell had reported
    // them during eager init. publish_event appends to the durable
    // session log because session_id_for_event maps these events to
    // the current session.
    h.publish_event(
        Some(&tools_conn),
        Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
            file_path: "/test/AGENTS.md".into(),
            content: "# test\n".to_owned(),
        }),
    );
    h.publish_event(
        Some(&tools_conn),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: default_session_id().into(),
        }),
    );

    // Hook up a fake UI client via a UnixStream pair.
    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");

    // Find the UI connection the bus assigned. `accept_client`
    // gives it name "socket-ui".
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    // Trigger subscribe + replay via the normal client-event path.
    h.handle_client_event(
        &ui_conn,
        Frame::Message(Message::Subscribe(Subscribe {
            selectors: vec![EventSelector::Prefix("extension.".to_owned())],
        })),
    )
    .expect("subscribe");

    // Compare what we receive on the wire against what the durable
    // log holds. The fix collapses two replay paths into one, so each
    // persisted event must arrive exactly once on the late-joining
    // client — not zero, not twice.
    let durable_agents_md = h
        .store
        .session_events(h.current_session_id.as_str())
        .expect("events")
        .into_iter()
        .filter(|e| {
            matches!(
                &e.event,
                Event::ExtAgentsMdAvailable(a)
                    if a.file_path == std::path::Path::new("/test/AGENTS.md")
            )
        })
        .count();
    let durable_context_ready = h
        .store
        .session_events(h.current_session_id.as_str())
        .expect("events")
        .into_iter()
        .filter(|e| matches!(&e.event, Event::ExtensionContextReady(_)))
        .count();
    assert_eq!(
        durable_agents_md, 1,
        "test setup: synthetic agents_md should land in the durable log exactly once"
    );
    assert!(
        durable_context_ready >= 1,
        "test setup: at least one context_ready in durable log"
    );

    let mut reader = FrameReader::new(BufReader::new(client_end));
    let mut agents_md_count = 0;
    let mut context_ready_count = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let (_log_id, inner) = frame.peel_log();
        let Frame::Event(inner) = inner else { continue };
        match inner {
            Event::ExtAgentsMdAvailable(a)
                if a.file_path == std::path::Path::new("/test/AGENTS.md") =>
            {
                agents_md_count += 1;
            }
            Event::ExtensionContextReady(_) => {
                context_ready_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        agents_md_count,
        durable_agents_md,
        "agents_md replayed count must equal durable log count; \
         double replay would produce {} but got {agents_md_count}",
        durable_agents_md * 2,
    );
    assert_eq!(
        context_ready_count,
        durable_context_ready,
        "context_ready replayed count must equal durable log count; \
         double replay would produce {} but got {context_ready_count}",
        durable_context_ready * 2,
    );

    h.shutdown().expect("shutdown");
}

// -- Invalid tool call rejection --

#[test]
fn empty_tool_name_does_not_panic_and_surfaces_error() {
    // Agents occasionally emit tool_calls with empty names
    // (hallucinations, streaming-token splits, model bugs).
    // `ToolName::new("")` panics by design, so the harness must
    // reject these cleanly before that construction happens.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed as if the agent had just been prompted and is now
    // responding with tool_calls.
    h.selected_model = "test/model".into();
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "c1".into(),
            // Intentionally an empty raw string to exercise the
            // `Invalid` arm of `ToolNameMaybe`.
            name: "".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("invalid tool call must not panic");

    // The call must be gone from both the pending queue and the
    // in-flight set — rejection fully completes it.
    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());

    // The error should have been persisted on s1's history so the
    // agent sees it on the next turn — as a Requested + Error pair
    // under the same call_id, so the Responses-API serializer can
    // emit a matching `function_call` / `function_call_output`
    // without the latter looking unpaired.
    let branch = h.store.session("s1").expect("session").current_branch();
    let mut saw_request = false;
    let mut saw_error = false;
    for entry in branch.iter() {
        let SessionEntry::ToolActivity(record) = entry else {
            continue;
        };
        if record.call_id.as_str() != "c1" {
            continue;
        }
        match &record.outcome {
            ToolActivityOutcome::Requested { .. } => saw_request = true,
            ToolActivityOutcome::Error { message, .. } if message.contains("invalid tool name") => {
                saw_error = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_request && saw_error,
        "rejected call should leave both a Requested and an Error \
         ToolActivity so the model-facing conversation has a \
         matching tool_use / tool_result pair"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn empty_tool_call_id_is_normalized_to_synthetic_id() {
    // Models that hallucinate an invalid tool_call often drop the
    // `call_id` too. An empty id breaks two things downstream:
    // it collides with itself as a HashMap key, and it renders
    // into the next prompt as `input[N].call_id: ""` which the
    // OpenAI Responses API rejects outright. Normalize at the
    // boundary.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "".into(),
                name: "".into(),
                arguments: CborValue::Map(Vec::new()),
            },
            AgentToolCall {
                id: "".into(),
                name: "".into(),
                arguments: CborValue::Map(Vec::new()),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("must not panic");

    // Both calls were rejected and the turn is fully drained.
    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());

    // Every persisted ToolActivityRecord must have a non-empty
    // call_id — this is what the LLM serializer round-trips.
    // And each rejected call must appear TWICE (a Requested +
    // Error pair) so the model-facing conversation has a
    // matching function_call for the function_call_output.
    let branch = h.store.session("s1").expect("session").current_branch();
    let activity_records: Vec<_> = branch
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::ToolActivity(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(
        activity_records.len(),
        4,
        "expected two records per rejected call (Requested + Error)"
    );
    let mut synth_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in &activity_records {
        assert!(
            !record.call_id.as_str().is_empty(),
            "synthesized call_id must not be empty; got {:?}",
            record.call_id
        );
        assert!(
            record.call_id.as_str().starts_with("harness-synth-"),
            "synthesized call_id should be clearly synthetic; got {:?}",
            record.call_id
        );
        synth_ids.insert(record.call_id.as_str().to_owned());
    }
    // Exactly two distinct synthetic ids across the four records.
    assert_eq!(
        synth_ids.len(),
        2,
        "the two rejected calls must have distinct synthetic ids; got {synth_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

// -- Tool dispatch state machine --

#[test]
fn pure_mutating_pure_serializes_through_dispatch_state_machine() {
    use tau_proto::ToolSideEffects::{Mutating, Pure};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.selected_model = "test/model".into();
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid);

    // A `read` of a nonexistent path returns a ToolError (Pure);
    // `write` of a valid path creates the file and returns
    // ToolResult (Mutating). Either kind of response path is
    // handled identically by the state machine.
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text("/nonexistent/tau-test-path".to_owned()),
    )]);
    let write_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("w.txt").display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text("hi".to_owned()),
        ),
    ]);
    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: read_args.clone(),
            },
            AgentToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: write_args,
            },
            AgentToolCall {
                id: "c3".into(),
                name: "read".into(),
                arguments: read_args,
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("finished");

    // Right after dispatch, only c1 (Pure) should be in-flight;
    // c2 (Mutating) and c3 (Pure behind the Mutating) must wait.
    let c1_id: ToolCallId = "c1".to_owned().into();
    let c2_id: ToolCallId = "c2".to_owned().into();
    let c3_id: ToolCallId = "c3".to_owned().into();
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c1_id), Some(&Pure));
    assert_eq!(h.pending_tool_invocations.len(), 2);
    assert_eq!(h.pending_tool_invocations[0].1.id, "c2");
    assert_eq!(h.pending_tool_invocations[1].1.id, "c3");

    drive_harness_until_call_completes(&mut h, "c1");

    // After c1 completes the Mutating gate opens and c2 dispatches.
    // c3 must stay queued behind it.
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c2_id), Some(&Mutating));
    assert_eq!(h.pending_tool_invocations.len(), 1);
    assert_eq!(h.pending_tool_invocations[0].1.id, "c3");

    drive_harness_until_call_completes(&mut h, "c2");

    // With the Mutating cleared, c3 finally dispatches.
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c3_id), Some(&Pure));
    assert!(h.pending_tool_invocations.is_empty());

    drive_harness_until_call_completes(&mut h, "c3");
    assert!(h.in_flight_tool_kinds.is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn multi_tool_turn_keeps_all_results_in_followup_prompt() {
    // Regression: when several tool calls complete in sequence, every
    // ToolResult must end up on the current branch so the follow-up
    // prompt sees a balanced tool_use ↔ tool_result set. A previous
    // bug let `publish_event` (used by the ToolResult/ToolError path)
    // leave the conversation's local head stale, so the next
    // ToolRequest's `publish_for_conversation` emitted a
    // `UiNavigateTree` that bounced the tree head backward — orphaning
    // the just-published ToolResult onto a dead branch and triggering
    // OpenAI's "No tool output found for function call ..." 400.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "go");
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid);

    let write_args = |name: &str| {
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join(name).display().to_string()),
            ),
            (
                CborValue::Text("content".to_owned()),
                CborValue::Text(name.to_owned()),
            ),
        ])
    };
    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "c1".into(),
                name: "write".into(),
                arguments: write_args("a.txt"),
            },
            AgentToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: write_args("b.txt"),
            },
            AgentToolCall {
                id: "c3".into(),
                name: "write".into(),
                arguments: write_args("c.txt"),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };
    h.handle_agent_response_finished(response)
        .expect("finished");

    drive_harness_until_call_completes(&mut h, "c1");
    drive_harness_until_call_completes(&mut h, "c2");
    drive_harness_until_call_completes(&mut h, "c3");

    // After all three tools complete, the harness has auto-dispatched
    // a follow-up prompt. Read its messages and check that every
    // tool_use has a matching tool_result on the same branch.
    let spid: SessionPromptId = "sp-0".into();
    let prompt = read_prompt_created(&h, &spid);
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut tool_result_ids: Vec<String> = Vec::new();
    for msg in &prompt.messages {
        for block in &msg.content {
            match block {
                tau_proto::ContentBlock::ToolUse { id, .. } => {
                    tool_use_ids.push(id.to_string());
                }
                tau_proto::ContentBlock::ToolResult { tool_use_id, .. } => {
                    tool_result_ids.push(tool_use_id.to_string());
                }
                tau_proto::ContentBlock::Text { .. } => {}
            }
        }
    }
    assert_eq!(
        tool_use_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "follow-up prompt must keep every tool_use; got {tool_use_ids:?}"
    );
    assert_eq!(
        tool_result_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "every tool_use must be paired with a tool_result on the current branch; \
         got {tool_result_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_prompt_is_steered_into_next_round_after_tool_result() {
    // While the agent is mid-turn (a tool is in flight), a fresh user
    // prompt must queue rather than dispatch. When the tool result
    // arrives and the harness is about to issue the next-round prompt,
    // it should drain the queued prompt onto this conversation's
    // branch as a `SessionPromptSteered` event so it rides the same
    // `SessionPromptCreated` as the tool results — instead of waiting
    // for full `Idle` and starting a separate turn.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());

    let write_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("a.txt").display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text("a".to_owned()),
        ),
    ]);
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: write_args,
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("agent response with tool call");

    // The conversation must be in `ToolsRunning` so `submit_user_prompt`
    // takes the queued path rather than dispatching.
    assert!(matches!(
        h.conversations.get(&cid).expect("default").turn_state,
        ConversationTurnState::ToolsRunning { .. }
    ));

    let submission = h
        .submit_user_prompt("s1".into(), "redirect".to_owned())
        .expect("submit");
    assert!(
        matches!(submission, PromptSubmission::Queued),
        "in-flight turn should force queueing, got {submission:?}"
    );
    assert_eq!(
        h.conversations
            .get(&cid)
            .expect("default")
            .pending_prompts
            .len(),
        1,
        "the steering message should sit in pending_prompts until the next-round seam",
    );

    drive_harness_until_call_completes(&mut h, "c1");

    assert!(
        h.conversations
            .get(&cid)
            .expect("default")
            .pending_prompts
            .is_empty(),
        "queued prompt must be drained when folded as a steer",
    );

    // Walk the event log and verify ordering: the SessionPromptSteered
    // is published before the next-round SessionPromptCreated, and the
    // latter's `messages` includes the steered text alongside the
    // original user prompt.
    let next_round_spid: SessionPromptId = "sp-0".into();
    let mut cursor = 0;
    let mut saw_steered = false;
    let mut saw_next_round = false;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        match &entry.event {
            Event::SessionPromptSteered(steered) => {
                assert_eq!(steered.text, "redirect");
                assert!(
                    !saw_next_round,
                    "steered event must precede the prompt it folds into",
                );
                saw_steered = true;
            }
            Event::SessionPromptCreated(p) if p.session_prompt_id == next_round_spid => {
                assert!(
                    saw_steered,
                    "next-round prompt must follow the SessionPromptSteered",
                );
                saw_next_round = true;

                let user_texts: Vec<String> = p
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, tau_proto::ConversationRole::User))
                    .flat_map(|m| {
                        m.content.iter().filter_map(|b| match b {
                            tau_proto::ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    })
                    .collect();
                assert!(
                    user_texts.iter().any(|t| t == "redirect"),
                    "next-round prompt should fold the steered message into messages; \
                     user texts were {user_texts:?}",
                );

                // The steered message must land *after* the tool result
                // on the same branch — otherwise the model sees its
                // tool_use replied to with a steer instead of the
                // ToolResult, which providers reject.
                let last_tool_result_idx = p.messages.iter().rposition(|m| {
                    m.content
                        .iter()
                        .any(|b| matches!(b, tau_proto::ContentBlock::ToolResult { .. }))
                });
                let last_user_idx = p.messages.iter().rposition(|m| {
                    matches!(m.role, tau_proto::ConversationRole::User)
                        && m.content.iter().any(|b| {
                            matches!(
                                b,
                                tau_proto::ContentBlock::Text { text } if text == "redirect"
                            )
                        })
                });
                assert!(
                    last_tool_result_idx.is_some(),
                    "next-round prompt must include the tool result"
                );
                assert!(
                    matches!((last_tool_result_idx, last_user_idx),
                        (Some(t), Some(u)) if u > t),
                    "steered user message must follow the tool result, not precede it",
                );
            }
            _ => {}
        }
    }
    assert!(saw_steered, "expected a SessionPromptSteered event");
    assert!(
        saw_next_round,
        "expected the next-round SessionPromptCreated"
    );

    h.shutdown().expect("shutdown");
}

/// Pumps the harness event loop until the named tool call's result
/// or error is received and handled. Panics on timeout.
fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for {target_call_id} to complete");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                frame,
            } => {
                let is_target = match frame.as_ref() {
                    Frame::Event(Event::ToolResult(r)) => r.call_id.as_str() == target_call_id,
                    Frame::Event(Event::ToolError(e)) => e.call_id.as_str() == target_call_id,
                    _ => false,
                };
                h.handle_extension_event(&connection_id, *frame)
                    .expect("handle");
                if is_target {
                    return;
                }
            }
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
        }
    }
}

// -- At-least-once delivery --

#[test]
fn extension_ack_advances_cursor() {
    // Verifies the at-least-once cursor: after the harness receives
    // an Ack from an extension, that extension's `last_acked` field
    // reflects the highest acked id.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    h.handle_extension_event(
        &tools_id,
        Frame::Message(Message::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(7),
        })),
    )
    .expect("ack");

    let tools = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry");
    assert_eq!(tools.last_acked, tau_proto::LogEventId::new(7));
    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_ack_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let before = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry")
        .last_acked;

    // Resending an old ack must not move the cursor backward and
    // must not bump it forward either.
    h.handle_extension_event(
        &tools_id,
        Frame::Message(Message::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(0),
        })),
    )
    .expect("ack");

    let after = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry")
        .last_acked;
    assert_eq!(before, after, "stale ack should not change cursor");
    h.shutdown().expect("shutdown");
}

// -- Skills --

#[test]
fn selected_effort_is_model_specific_and_clamped() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            default_efforts: {
                "openai/gpt-4.1": "high",
                "local/llama": "high",
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            providers: {
                local: {
                    compat: { supportsReasoningEffort: false },
                    models: [{ id: "llama" }],
                },
                openai: {
                    compat: { supportsReasoningEffort: true },
                    models: [{ id: "gpt-4.1" }],
                },
            },
        }"#,
    )
    .expect("write models");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "last_selected_model": "openai/gpt-4.1",
            "last_efforts": {
                "openai/gpt-4.1": "minimal",
                "local/llama": "high"
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let model_registry = tau_config::settings::load_models_in(&dirs).expect("load models");

    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "openai/gpt-4.1",),
        tau_proto::Effort::High
    );
    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "local/llama"),
        tau_proto::Effort::Off
    );
}

/// First-time users (no per-model entry in `default_efforts`, no
/// persisted `last_efforts`) get the middle of the available
/// reasoning levels, not the lowest. For the standard
/// reasoning-supporting list (`[Off, Minimal, Low, Medium, High]`)
/// that's `Low`. Non-reasoning providers stay at `Off`.
#[test]
fn fresh_install_picks_middle_effort_when_no_history() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    // No harness.json5: default settings, empty default_efforts.
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            providers: {
                local: {
                    compat: { supportsReasoningEffort: false },
                    models: [{ id: "llama" }],
                },
                openai: {
                    compat: { supportsReasoningEffort: true },
                    models: [{ id: "gpt-4.1" }],
                },
            },
        }"#,
    )
    .expect("write models");
    // No harness.json5: fresh install.

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let model_registry = tau_config::settings::load_models_in(&dirs).expect("load models");

    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "openai/gpt-4.1"),
        tau_proto::Effort::Low,
    );
    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "local/llama"),
        tau_proto::Effort::Off,
    );
}

#[test]
fn build_system_prompt_includes_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("brave-search"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: PathBuf::from("/skills/brave-search/SKILL.md"),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>brave-search</name>"));
    assert!(prompt.contains("Web search via Brave API"));
    assert!(!prompt.contains("Current date:"));
    assert!(prompt.contains("Current working directory: /tmp/work"));
}

#[test]
fn build_system_prompt_excludes_hidden_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("hidden"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Should not appear".to_owned(),
            file_path: PathBuf::from("/skills/hidden/SKILL.md"),
            add_to_prompt: false,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(!prompt.contains("<available_skills>"));
    assert!(!prompt.contains("hidden"));
}

#[test]
fn linear_session_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("hi".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.effort, prompt1.effort);
    assert!(
        prompt1.messages.len() < prompt2.messages.len(),
        "second prompt should strictly extend first: {} !< {}",
        prompt1.messages.len(),
        prompt2.messages.len()
    );
    assert_eq!(
        &prompt2.messages[..prompt1.messages.len()],
        prompt1.messages.as_slice(),
        "second prompt must keep first prompt messages as an exact prefix"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn resumed_harness_replays_persisted_session_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    {
        let mut h = echo_harness_for("s1", &sp).expect("start");
        h.selected_model = "test/model".into();

        h.submit_user_prompt("s1".into(), "remember potato".to_owned())
            .expect("submit first prompt");
        let spid = h
            .prompt_conversations
            .keys()
            .next()
            .expect("first session prompt id")
            .clone();
        h.handle_agent_response_finished(AgentResponseFinished {
            session_prompt_id: spid,
            text: Some("remembered potato".to_owned()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("persist agent response");

        h.shutdown().expect("shutdown");
    }

    let mut resumed = echo_harness_for("s1", &sp).expect("resume");
    resumed.selected_model = "test/model".into();

    resumed
        .submit_user_prompt("s1".into(), "what was it?".to_owned())
        .expect("submit resumed prompt");
    let spid = resumed
        .prompt_conversations
        .keys()
        .next()
        .expect("resumed session prompt id")
        .clone();
    let prompt = read_prompt_created(&resumed, &spid);
    let serialized = serde_json::to_string(&prompt.messages).expect("json");

    assert!(
        serialized.contains("remember potato"),
        "resumed prompt must replay persisted user message: {serialized}",
    );
    assert!(
        serialized.contains("remembered potato"),
        "resumed prompt must replay persisted agent response: {serialized}",
    );
    assert!(
        serialized.contains("what was it?"),
        "resumed prompt must include the new prompt: {serialized}",
    );

    resumed.shutdown().expect("shutdown");
}

#[test]
fn thinking_is_persisted_but_excluded_from_prompt_replay() {
    // Linear-prefix and prompt-cache hygiene depends on
    // `assemble_conversation` ignoring the persisted thinking
    // field. Otherwise the model would see its own reasoning
    // summary echoed back as plain assistant text.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "first");

    let spid1 = h.send_prompt_to_agent("s1");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: Some("The user is asking ...".to_owned()),
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("persist agent response");

    // Confirm it was stored on the session entry.
    let stored = h
        .store
        .session("s1")
        .expect("session")
        .current_branch()
        .into_iter()
        .find_map(|e| match e {
            SessionEntry::AgentMessage { thinking, .. } => Some(thinking.clone()),
            _ => None,
        })
        .expect("agent message");
    assert_eq!(stored.as_deref(), Some("The user is asking ..."));

    // The next prompt's replayed messages must NOT contain the
    // thinking text.
    append_user_message_via_event(&mut h, "s1", "second");
    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);
    let serialized = serde_json::to_string(&prompt2.messages).expect("json");
    assert!(
        !serialized.contains("The user is asking"),
        "prompt replay must not echo reasoning summary back to the model",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn skill_tool_reads_file_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let skill_dir = td.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing.",
    )
    .expect("write");

    let mut h = echo_harness(&sp).expect("start");

    // Manually insert a discovered skill.
    h.discovered_skills.insert(
        tau_proto::SkillName::from("my-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "A test skill".to_owned(),
            file_path: skill_file,
            add_to_prompt: true,
        },
    );

    // Directly invoke the skill tool handler.
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-skill".into()]);
    let call = AgentToolCall {
        id: "call-skill".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text("my-skill".to_owned()),
            ),
        ]),
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify the tool result was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_result = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Result { .. },
                ..
            })
        )
    });
    assert!(has_skill_result, "expected skill tool result in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolResult(result) if result.call_id.as_str() == "call-skill"
        )),
        "expected skill tool result in durable session event log"
    );
}

#[test]
fn skill_tool_returns_error_for_unknown_skill() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-missing".into()]);
    let call = AgentToolCall {
        id: "call-missing".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text("nonexistent".to_owned()),
            ),
        ]),
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify a tool error was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_error = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Error { .. },
                ..
            })
        )
    });
    assert!(has_skill_error, "expected skill tool error in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolError(error) if error.call_id.as_str() == "call-missing"
        )),
        "expected skill tool error in durable session event log"
    );
}

#[test]
fn skill_load_unknown_attaches_split_name_search_suggestions() {
    // Agents routinely guess at skill names; when the load misses we
    // free-search using the requested name split on `-`/`_` so the
    // error response carries plausible alternatives.
    //
    // Use synthetic tokens that won't collide with the user's real
    // `~/.agents/skills` library that `echo_harness` discovers, then
    // wipe `discovered_skills` so the assertions are deterministic.
    const PREFIX: &str = "qzxtest";
    const TKLANG: &str = "qzxlang";
    const TKSTYLE: &str = "qzxstyle";

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let make_skill = |name: &str, description: &str| {
        let dir = td.path().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .expect("write");
        path
    };
    let lang_name = format!("{PREFIX}-{TKLANG}-helper");
    let style_name = format!("{PREFIX}-{TKSTYLE}-guide");
    let decoy_name = "totally-unrelated-skill".to_owned();
    let lang_path = make_skill(&lang_name, &format!("{TKLANG} helpers"));
    let style_path = make_skill(&style_name, &format!("{TKSTYLE} guide"));
    let decoy_path = make_skill(&decoy_name, "unrelated thing");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();
    for (name, desc, path) in [
        (lang_name.clone(), format!("{TKLANG} helpers"), lang_path),
        (style_name.clone(), format!("{TKSTYLE} guide"), style_path),
        (decoy_name.clone(), "unrelated thing".to_owned(), decoy_path),
    ] {
        h.discovered_skills.insert(
            tau_proto::SkillName::from(name.as_str()),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: desc,
                file_path: path,
                add_to_prompt: false,
            },
        );
    }

    let requested = format!("{PREFIX}-{TKLANG}-{TKSTYLE}");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-miss".into()]);
    let call = AgentToolCall {
        id: "call-miss".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text(requested.clone()),
            ),
        ]),
    };
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    let events = h.store.session_events("s1").expect("session events");
    let err = events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ToolError(e) if e.call_id.as_str() == "call-miss" => Some(e),
            _ => None,
        })
        .expect("tool error");
    assert!(
        err.message.contains("unknown skill"),
        "unexpected message: {}",
        err.message
    );
    let details = err.details.as_ref().expect("details");
    let CborValue::Map(entries) = details else {
        panic!("details should be a map: {details:?}");
    };
    let get = |key: &str| {
        entries.iter().find_map(|(k, v)| match k {
            CborValue::Text(s) if s == key => Some(v),
            _ => None,
        })
    };
    assert_eq!(
        get("name"),
        Some(&CborValue::Text(requested.clone())),
        "details.name should echo the requested name"
    );
    let queries = match get("queries") {
        Some(CborValue::Array(a)) => a.clone(),
        other => panic!("queries should be array: {other:?}"),
    };
    let needles: Vec<String> = queries
        .iter()
        .filter_map(|v| match v {
            CborValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(needles, vec![PREFIX, TKLANG, TKSTYLE]);
    let matches = match get("matches") {
        Some(CborValue::Array(a)) => a.clone(),
        other => panic!("matches should be array: {other:?}"),
    };
    let match_names: Vec<String> = matches
        .iter()
        .filter_map(|m| match m {
            CborValue::Map(fields) => fields.iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    // Both helpers should be suggested (each shares two needles with
    // the requested name); the unrelated decoy must not appear.
    assert!(
        match_names.iter().any(|n| n == &lang_name),
        "expected {lang_name} in suggestions, got: {match_names:?}"
    );
    assert!(
        match_names.iter().any(|n| n == &style_name),
        "expected {style_name} in suggestions, got: {match_names:?}"
    );
    assert!(
        !match_names.iter().any(|n| n == &decoy_name),
        "unrelated decoy leaked into suggestions: {match_names:?}"
    );
}

#[test]
fn skill_tool_search_matches_name_description_and_optional_content() {
    // The search action backs progressive skill discovery: when most
    // skills are not advertised at session start, the agent must be
    // able to find them by keyword. Default scope is name +
    // description; `search_content: true` opts into grepping bodies.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    // Use unique tokens that won't collide with the user's real
    // `~/.agents/skills` library that `echo_harness` discovers during
    // eager init.
    const KW: &str = "zqxtoken";
    const BODY_KW: &str = "zqxbody";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!(
            "---\nname: zqx-alpha\ndescription: {KW} helpers\n---\nalpha body, mentions {BODY_KW}"
        ),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!(
            "---\nname: zqx-beta\ndescription: unrelated thing\n---\nbeta body, mentions {BODY_KW} too"
        ),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        "---\nname: zqx-gamma\ndescription: a different topic\n---\nno keyword references here",
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("{KW} helpers"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated thing".to_owned(),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "a different topic".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, search_content: bool, id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("search".to_owned()),
            ),
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(query.to_owned()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
        ]),
    };

    let read_matches = |h: &Harness, call_id: &str| -> Vec<String> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                entries
                    .into_iter()
                    .find_map(|(k, v)| match (k, v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                        _ => None,
                    })
                    .expect("name in match")
            })
            .collect()
    };

    // Description match: KW only appears in zqx-alpha's description.
    seed_tools_running(&mut h, &cid, vec!["call-1".into()]);
    h.handle_skill_tool_call(&cid, &call_search(KW, false, "call-1"))
        .expect("search 1");
    assert_eq!(read_matches(&h, "call-1"), vec!["zqx-alpha"]);

    // Default scope must NOT search content: BODY_KW appears only in
    // alpha and beta bodies. With search_content=false → no hits.
    seed_tools_running(&mut h, &cid, vec!["call-2".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, false, "call-2"))
        .expect("search 2");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-2"), empty);

    // Opt into content search: now alpha and beta both match,
    // sorted alphabetically.
    seed_tools_running(&mut h, &cid, vec!["call-3".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, true, "call-3"))
        .expect("search 3");
    assert_eq!(read_matches(&h, "call-3"), vec!["zqx-alpha", "zqx-beta"]);

    // Name match works case-insensitively.
    seed_tools_running(&mut h, &cid, vec!["call-4".into()]);
    h.handle_skill_tool_call(&cid, &call_search("ZQX-ALPHA", false, "call-4"))
        .expect("search 4");
    assert_eq!(read_matches(&h, "call-4"), vec!["zqx-alpha"]);
}

#[test]
fn skill_tool_search_accepts_multiple_terms_and_ranks_by_hit_count() {
    // Multi-term search: the agent fires several plausible terms at
    // once, the harness scores each skill by how many terms matched
    // it, and returns hits sorted by score descending. Ties break on
    // name to keep the output deterministic.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    const T1: &str = "zqxalpha";
    const T2: &str = "zqxbeta";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!("---\nname: zqx-alpha\ndescription: matches {T1} and {T2}\n---\nbody"),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!("---\nname: zqx-beta\ndescription: matches only {T1}\n---\nbody"),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        "---\nname: zqx-gamma\ndescription: unrelated\n---\nbody",
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches {T1} and {T2}"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches only {T1}"),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search_array = |terms: &[&str], id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("search".to_owned()),
            ),
            (
                CborValue::Text("query".to_owned()),
                CborValue::Array(
                    terms
                        .iter()
                        .map(|t| CborValue::Text((*t).to_owned()))
                        .collect(),
                ),
            ),
        ]),
    };

    let read_match_records = |h: &Harness, call_id: &str| -> Vec<(String, u64)> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                let mut name = None;
                let mut hits: Option<u64> = None;
                for (k, v) in entries {
                    match (&k, &v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => {
                            name = Some(v.clone());
                        }
                        (CborValue::Text(k), CborValue::Integer(i)) if k == "hit_count" => {
                            let n: i128 = (*i).into();
                            hits = Some(n as u64);
                        }
                        _ => {}
                    }
                }
                (name.expect("name"), hits.expect("hit_count"))
            })
            .collect()
    };

    seed_tools_running(&mut h, &cid, vec!["call-multi".into()]);
    h.handle_skill_tool_call(&cid, &call_search_array(&[T1, T2], "call-multi"))
        .expect("multi search");
    let records = read_match_records(&h, "call-multi");
    assert_eq!(
        records,
        vec![("zqx-alpha".to_owned(), 2), ("zqx-beta".to_owned(), 1),],
        "alpha matches both terms (rank 2), beta matches one (rank 1), \
         gamma matches none and must be filtered out",
    );

    // A single-element array must behave the same as a single string —
    // the per-term matcher is unchanged.
    seed_tools_running(&mut h, &cid, vec!["call-single".into()]);
    h.handle_skill_tool_call(&cid, &call_search_array(&[T2], "call-single"))
        .expect("single in array");
    assert_eq!(
        read_match_records(&h, "call-single"),
        vec![("zqx-alpha".to_owned(), 1)],
    );

    // Empty array should error rather than silently returning every
    // skill — the agent passing `[]` is almost always a bug.
    seed_tools_running(&mut h, &cid, vec!["call-empty".into()]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-empty".into(),
            name: "skill".into(),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("search".to_owned()),
                ),
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Array(Vec::new()),
                ),
            ]),
        },
    )
    .expect("call");
    let events = h.store.session_events("s1").expect("events");
    let saw_error = events.iter().rev().any(|entry| {
        matches!(
            &entry.event,
            Event::ToolError(e) if e.call_id.as_str() == "call-empty"
        )
    });
    assert!(saw_error, "empty query array must produce a ToolError");
}

#[test]
fn skill_tool_unknown_action_returns_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-bogus".into()]);
    let call = AgentToolCall {
        id: "call-bogus".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![(
            CborValue::Text("action".to_owned()),
            CborValue::Text("invoke".to_owned()),
        )]),
    };
    h.handle_skill_tool_call(&cid, &call).expect("dispatch");
    let events = h.store.session_events("s1").expect("events");
    let err = events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ToolError(e) if e.call_id.as_str() == "call-bogus" => Some(e.message.clone()),
            _ => None,
        })
        .expect("tool error");
    assert!(
        err.contains("unknown skill action"),
        "unexpected error message: {err}"
    );
}

#[test]
fn skill_tool_registered_in_tool_list() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let h = echo_harness(&sp).expect("start");
    let defs = h.gather_tool_definitions();
    assert!(
        defs.iter().any(|d| d.name == "skill"),
        "skill tool should be registered; got: {:?}",
        defs.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_tool_result_is_discarded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");

    // Fabricate a tool result for a call_id that is not in pending_tool_sessions.
    let result = h.handle_extension_event(
        "fake-ext",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "orphan-call".into(),
            tool_name: "read".into(),
            result: tau_proto::CborValue::Text("stale data".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}

#[test]
fn queued_prompt_extends_completed_first_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    let first = h
        .submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    assert_eq!(first, PromptSubmission::Dispatched);
    let spid1: SessionPromptId = "sp-0".into();
    let prompt1 = read_prompt_created(&h, &spid1);

    let second = h
        .submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    assert_eq!(second, PromptSubmission::Queued);

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("first answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("finish first");

    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);
    assert!(
        prompt1.messages.len() < prompt2.messages.len(),
        "queued follow-up should extend the first prompt"
    );
    assert_eq!(
        &prompt2.messages[..prompt1.messages.len()],
        prompt1.messages.as_slice()
    );
    let last = prompt2.messages.last().expect("last message");
    assert_eq!(last.role, tau_proto::ConversationRole::User);
    assert_eq!(
        last.content,
        vec![tau_proto::ContentBlock::Text {
            text: "second".to_owned()
        }]
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn switch_session_rebinds_default_conversation() {
    // Regression: `/new` flips `current_session_id` but used to leave
    // the default conversation pointing at the old session, which made
    // the next user prompt panic in `dispatch_user_prompt`'s
    // assert_eq!.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"
    h.selected_model = "test/model".into();

    let cid = h.default_conversation_id.clone();
    assert_eq!(h.conversations[&cid].session_id.as_str(), "s1");

    let shell_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch");

    let mut saw_session_dir = false;
    let mut cursor = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq + 1;
        if let Event::HarnessSessionDir(session_dir) = &entry.event
            && session_dir.session_id == "s2"
            && session_dir.path.ends_with("s2")
            && session_dir.status == tau_proto::SessionDirStatus::New
        {
            saw_session_dir = true;
        }
    }
    assert!(saw_session_dir, "switch must announce the new session dir");

    assert_eq!(h.current_session_id.as_str(), "s2");
    assert_eq!(
        h.conversations[&cid].session_id.as_str(),
        "s2",
        "default conversation must follow the bound session id",
    );

    // Drive the new session through init so submit_user_prompt
    // actually dispatches (rather than queuing).
    h.handle_extension_event(
        &shell_conn,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s2".into(),
            },
        )),
    )
    .expect("ready");

    let submission = h
        .submit_user_prompt("s2".into(), "hello".to_owned())
        .expect("submit");
    assert_eq!(submission, PromptSubmission::Dispatched);

    h.shutdown().expect("shutdown");
}

#[test]
fn ext_agent_query_dispatches_while_tool_is_running_and_restores_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-main".into();
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "delegate-call".into(),
            name: "delegate".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("tool response");

    assert!(matches!(h.turn_state, TurnState::Idle));
    let default_turn = &h
        .conversations
        .get(&h.default_conversation_id)
        .expect("default conversation")
        .turn_state;
    assert!(matches!(
        default_turn,
        ConversationTurnState::ToolsRunning { .. }
    ));
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("query");

    assert!(
        h.conversations
            .values()
            .all(|conv| conv.pending_prompts.is_empty()),
        "side prompt must dispatch immediately"
    );
    assert!(matches!(h.turn_state, TurnState::Idle));

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: side_spid,
        text: Some("delegated answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q1".to_owned(),
        },
    })
    .expect("side finished");

    assert!(matches!(h.turn_state, TurnState::Idle));
    let default_turn = &h
        .conversations
        .get(&h.default_conversation_id)
        .expect("default conversation")
        .turn_state;
    assert!(matches!(
        default_turn,
        ConversationTurnState::ToolsRunning { .. }
    ));
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Event(Event::ExtAgentQueryResult(result)) if result.query_id == "q1" => {
                Some(result)
            }
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "delegated answer");
    h.shutdown().expect("shutdown");
}

#[test]
fn ext_agent_query_during_tool_call_branches_off_unresolved_tool_use() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "delegate-call".into(),
            name: "delegate".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("tool response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);

    // The sub-agent gets a fresh context regardless of whether its
    // parent is mid-tool-call: it sees only its own `query.instruction`,
    // never the parent's unresolved `delegate` tool_use (which would
    // be an orphan ToolUse the provider rejects), and never the
    // user's task framing (which would invite recursive re-delegation).
    let saw_orphan_tool_use = prompt.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                tau_proto::ContentBlock::ToolUse { id, .. }
                    if id.as_str() == "delegate-call"
            )
        })
    });
    assert!(
        !saw_orphan_tool_use,
        "side prompt must not replay the parent's unresolved delegate tool_use"
    );

    let saw_user_framing = prompt.messages.iter().any(|message| {
        matches!(message.role, tau_proto::ConversationRole::User)
            && message.content.iter().any(|block| {
                matches!(block, tau_proto::ContentBlock::Text { text } if text.contains("delegate something"))
            })
    });
    assert!(
        !saw_user_framing,
        "side prompt must NOT inherit the user's task framing — sub-agents start with a fresh context"
    );

    let saw_own_instruction = prompt.messages.iter().any(|message| {
        matches!(message.role, tau_proto::ConversationRole::User)
            && message.content.iter().any(|block| {
                matches!(block, tau_proto::ContentBlock::Text { text } if text == "side task")
            })
    });
    assert!(
        saw_own_instruction,
        "side prompt should contain the delegated instruction"
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a sub-agent's `Pure` tool call must not be gated by the
/// parent's still-in-flight `Mutating` `delegate` call. The parent's
/// delegate only resolves once the sub-agent's tools have run, so a
/// global pure-vs-mutating gate produces a self-deadlock — the main
/// symptom we hit in `tau-agent-m2dpw4`'s event log.
#[test]
fn side_conversation_pure_tool_dispatches_through_parent_mutating_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: "websearch".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );

    // Main agent issues `delegate`, putting a Mutating call in flight
    // on the default conversation.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "delegate-call".into(),
            name: "delegate".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    // Delegate extension turns it into an ExtAgentQuery; the harness
    // spawns a side conversation and dispatches its prompt.
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("query");

    // Sub-agent now responds with a Pure `websearch` call. Without
    // per-conversation gating this would queue forever behind the
    // parent's still-in-flight Mutating `delegate`.
    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: side_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "websearch-call".into(),
            name: "websearch".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q1".to_owned(),
        },
    })
    .expect("side response");

    // The Pure call must have been routed to the websearch
    // extension — the bus sends `ToolInvoke` directly to the
    // resolved provider, so the test sink sees it there rather
    // than the broadcast `ToolRequest`.
    let saw_routed = websearch_events.lock().expect("ws").iter().any(|routed| {
        matches!(
            &routed.frame,
            Frame::Event(Event::ToolInvoke(invoke)) if invoke.call_id.as_str() == "websearch-call"
        )
    });
    assert!(
        saw_routed,
        "side conversation's Pure tool must dispatch despite parent's in-flight Mutating delegate"
    );
    assert!(
        h.pending_tool_invocations.is_empty(),
        "no entries should be left queued"
    );

    h.shutdown().expect("shutdown");
}

/// Two `delegate` calls with `read_only: true` issued in the same
/// agent turn must be classified as `Pure` and therefore dispatch
/// concurrently — `delegate` is registered as `Mutating` (the safe
/// default), but the per-call override on `read_only: true` lets the
/// agent opt two known-safe delegations into parallel scheduling.
#[test]
fn read_only_delegate_calls_dispatch_concurrently() {
    use tau_proto::{CborValue, ToolNameMaybe};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "two read-only lookups".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let read_only_args = CborValue::Map(vec![(
        CborValue::Text("read_only".to_owned()),
        CborValue::Bool(true),
    )]);
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "ro-1".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: read_only_args.clone(),
            },
            AgentToolCall {
                id: "ro-2".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: read_only_args.clone(),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    // Both calls should be in flight simultaneously: per-call kind
    // resolves to `Pure` via `read_only: true`, and `Pure` does not
    // serialize against other Pure on the same conversation.
    assert_eq!(h.in_flight_tool_kinds.len(), 2);
    assert!(
        h.in_flight_tool_kinds
            .values()
            .all(|kind| matches!(kind, tau_proto::ToolSideEffects::Pure)),
        "both read-only delegates should be classified Pure",
    );
    assert!(
        h.pending_tool_invocations.is_empty(),
        "no entries should remain queued — Pure+Pure dispatches in parallel",
    );

    // Sanity: without `read_only` the same two calls must not
    // parallelize. Reset the harness and replay with bare delegates.
    let td2 = TempDir::new().expect("tempdir");
    let sp2 = td2.path().join("state");
    let mut h2 = echo_harness(&sp2).expect("start");
    h2.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h2, "conn-delegate");
    h2.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let cid2 = h2.default_conversation_id.clone();
    let spid2: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h2, &cid2, "sp-main");
    h2.prompt_conversations.insert(spid2.clone(), cid2.clone());
    h2.publish_for_conversation(
        &cid2,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "two mutating delegations".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h2.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid2,
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "mut-1".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
            },
            AgentToolCall {
                id: "mut-2".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");
    assert_eq!(
        h2.in_flight_tool_kinds.len(),
        1,
        "only first Mutating dispatches"
    );
    assert_eq!(
        h2.pending_tool_invocations.len(),
        1,
        "second Mutating queues"
    );
}

/// Sub-agent state changes (tool start, response usage, tool finish)
/// must surface to the user as `DelegateProgress` events keyed on the
/// parent's `delegate` tool call_id. The CLI uses these to repaint
/// the running tool block as `delegate [task] ctx: …% tools: a/b …`.
#[test]
fn delegate_emits_progress_as_sub_agent_makes_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let _websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: "websearch".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );

    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "delegate-call".into(),
            name: "delegate".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    let sink = collect_event_sink(&mut h);
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("look it up".to_owned()),
        },
    )
    .expect("query");

    // First snapshot: side conversation just spawned, sub-agent has
    // not yet acted. Counters at zero, no context info.
    let initial = pop_delegate_progress(&sink, "delegate-call")
        .expect("initial DelegateProgress on side conv spawn");
    assert_eq!(initial.task_name, "look it up");
    assert_eq!(initial.tools_in_flight, 0);
    assert_eq!(initial.tools_total, 0);

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: side_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "websearch-call".into(),
            name: "websearch".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: Some(1234),
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q1".to_owned(),
        },
    })
    .expect("side response");

    // After the side response finishes (which carries usage) and the
    // sub-agent's tool starts dispatching, we should observe at
    // least one `DelegateProgress` reflecting the new state. The
    // exact emit count is not load-bearing — the *latest* snapshot
    // is what the UI renders.
    let latest = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("at least one DelegateProgress after side response");
    assert_eq!(latest.task_name, "look it up");
    assert_eq!(latest.tools_in_flight, 1, "websearch is in flight");
    assert_eq!(latest.tools_total, 1, "websearch counts toward total");
    assert_eq!(latest.ctx_input_tokens, Some(1234));

    // Complete the sub-agent's tool — counters should drop and a
    // fresh progress event should show 0 in flight, 1 total.
    h.handle_extension_event(
        "conn-websearch",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "websearch-call".into(),
            tool_name: "websearch".into(),
            result: CborValue::Text("fake result".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("ws result");
    let after_complete = drain_delegate_progress(&sink, "delegate-call")
        .pop()
        .expect("DelegateProgress after sub-tool completion");
    assert_eq!(after_complete.tools_in_flight, 0);
    assert_eq!(after_complete.tools_total, 1);

    h.shutdown().expect("shutdown");
}

/// Regression: when one side conversation tears down (running
/// `snap_to_default_conversation`) before another's tool result
/// arrives, the result must still fold onto the *originating*
/// conversation's branch. Before this fix, the result landed at
/// `tree.head` (which `snap_to_default` had moved to the parent
/// branch), producing orphan ToolUse blocks in subsequent prompts —
/// the exact `No tool output found for function call …` 400 we hit
/// in `tau-agent-yvxco1`'s log.
#[test]
fn sibling_side_conv_teardown_does_not_misplace_other_side_conv_tool_result() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    // Set up the main agent's turn that emits a single delegate call.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "outer-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    // Spawn the outer side conversation.
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    // Have the outer sub-agent emit a *nested* delegate. The harness
    // should issue another ExtAgentQuery for it, which we then ack
    // with a fresh side conversation. This is the exact pattern that
    // produced the misplacement: outer side conv runs teardown
    // (snap_to_default) before nested side conv's tool result lands.
    let outer_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: outer_side_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "nested-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },
    })
    .expect("outer response");
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    // Nested sub-agent finishes with a final answer. This triggers
    // side teardown: `snap_to_default_conversation` runs, moving
    // tree.head back to the main branch. The delegate ext then
    // publishes a ToolResult for `nested-call` — which must fold on
    // the *outer* conv's branch (since outer issued nested-call), not
    // wherever tree.head happens to be.
    let nested_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: nested_side_spid,
        text: Some("nested answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-nested".to_owned(),
        },
    })
    .expect("nested final");

    // The delegate extension would route the nested ExtAgentQueryResult
    // back as a ToolResult — simulate that here.
    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "nested-call".into(),
            tool_name: "delegate".into(),
            result: CborValue::Text("nested answer".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("nested tool result");

    // Now re-prompt the outer sub-agent and inspect the assembled
    // messages. The `outer-call` tool_use must NOT appear in the
    // outer sub-agent's branch — the only ToolUse the outer
    // sub-agent should see is its own `nested-call` (with a
    // matching ToolResult).
    let outer_resume_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() == outer_side_cid_str(&h)).then_some(spid.clone())
        })
        .expect("outer resume prompt id");
    let prompt = read_prompt_created(&h, &outer_resume_spid);

    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();
    for message in &prompt.messages {
        for block in &message.content {
            match block {
                tau_proto::ContentBlock::ToolUse { id, .. } => {
                    tool_uses.push(id.as_str().to_owned())
                }
                tau_proto::ContentBlock::ToolResult { tool_use_id, .. } => {
                    tool_results.push(tool_use_id.as_str().to_owned())
                }
                _ => {}
            }
        }
    }
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "outer sub-agent's prompt must not include the parent's `outer-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_uses.iter().any(|id| id == "nested-call"),
        "outer sub-agent's prompt must include its own `nested-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_results.iter().any(|id| id == "nested-call"),
        "outer sub-agent must see the matching ToolResult for `nested-call`; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: nested extension-agent queries must branch from the
/// conversation that issued the nested tool call. Branching from the
/// default conversation can replay unrelated in-flight ToolUse blocks
/// from the main branch into the nested sub-agent prompt, which OpenAI
/// rejects with `No tool output found for function call …`.
#[test]
fn nested_ext_agent_query_branches_from_tool_owner_conversation() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    let default_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_conversation(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "outer-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("outer query");

    let outer_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: outer_side_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "nested-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },
    })
    .expect("outer response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    let nested_side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    let prompt = read_prompt_created(&h, &nested_side_spid);

    let mut tool_uses = Vec::new();
    for message in &prompt.messages {
        for block in &message.content {
            if let tau_proto::ContentBlock::ToolUse { id, .. } = block {
                tool_uses.push(id.as_str().to_owned());
            }
        }
    }
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "nested sub-agent's prompt must not include the default branch's unresolved `outer-call`; got: {tool_uses:?}",
    );
    assert!(
        !tool_uses.iter().any(|id| id == "nested-call"),
        "nested sub-agent starts before its parent call has a result, so it must not include `nested-call`; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn completed_side_conversation_tool_result_reprompts_parent() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    let cid = h.default_conversation_id.clone();
    let spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations.insert(spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "outer-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: side_spid,
        text: Some("outer answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },
    })
    .expect("side final");

    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "outer-call".into(),
            tool_name: "delegate".into(),
            result: CborValue::Text("outer answer".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("delegate result");

    let main_resume_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() == "default").then_some(spid.clone()))
        .expect("main resume prompt id");
    let prompt = read_prompt_created(&h, &main_resume_spid);
    let mut tool_results = Vec::new();
    for message in &prompt.messages {
        for block in &message.content {
            if let tau_proto::ContentBlock::ToolResult { tool_use_id, .. } = block {
                tool_results.push(tool_use_id.as_str().to_owned());
            }
        }
    }
    assert!(
        tool_results.iter().any(|id| id == "outer-call"),
        "parent conversation must be re-prompted with delegate ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn recursive_delegate_prompt_contains_only_leaf_instruction() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    let default_cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_conversation(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "ROOT: ask top delegate to delegate again".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "top-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-top".to_owned(),
            instruction: "TOP: delegate exactly two more subtasks".to_owned(),
            tool_call_id: Some("top-call".into()),
            task_name: Some("top".to_owned()),
        },
    )
    .expect("top query");

    let top_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("top prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: top_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "leaf-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-top".to_owned(),
        },
    })
    .expect("top response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-leaf".to_owned(),
            instruction: "LEAF: do one terminal search only".to_owned(),
            tool_call_id: Some("leaf-call".into()),
            task_name: Some("leaf".to_owned()),
        },
    )
    .expect("leaf query");

    let leaf_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| {
            matches!(
                h.conversations
                    .get(prompt_cid)
                    .map(|conv| &conv.originator),
                Some(tau_proto::PromptOriginator::Extension { query_id, .. }) if query_id == "q-leaf"
            )
            .then_some(spid.clone())
        })
        .expect("leaf prompt id");
    let prompt = read_prompt_created(&h, &leaf_spid);
    let rendered = prompt
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            tau_proto::ContentBlock::Text { text } => Some(text.as_str()),
            tau_proto::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("LEAF: do one terminal search only"),
        "leaf prompt must include its own instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("TOP: delegate exactly two more subtasks"),
        "leaf prompt must not inherit parent recursive instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("ROOT: ask top delegate to delegate again"),
        "leaf prompt must not inherit ancestor task framing; got: {rendered}",
    );

    let mut tool_uses = Vec::new();
    for message in &prompt.messages {
        for block in &message.content {
            if let tau_proto::ContentBlock::ToolUse { id, .. } = block {
                tool_uses.push(id.as_str().to_owned());
            }
        }
    }
    assert!(
        tool_uses.is_empty(),
        "leaf prompt must not inherit unresolved ancestor tool calls; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: parallel side conversations must not steal each
/// other's branch cursor. Before the per-event `folded_node_id`
/// sync, `commit_event` synced `c.head` from the global
/// `tree.head()`. A non-folding event on conv-A (e.g. an
/// `AgentResponseFinished` carrying only tool calls) would overwrite
/// `c.head[conv-A]` with whatever sibling conv-B last folded — so
/// conv-A's next `ToolRequest` would graft onto conv-B's branch and
/// the resulting prompt would walk through unrelated history,
/// producing orphan ToolUse blocks the provider rejects with
/// `No tool output found for function call …`.
#[test]
fn parallel_side_convs_do_not_share_branch_cursor() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "go".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "main-A".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
            },
            AgentToolCall {
                id: "main-B".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-A".to_owned(),
            instruction: "instr A".to_owned(),
            tool_call_id: Some("main-A".into()),
            task_name: Some("A".to_owned()),
        },
    )
    .expect("query A");
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-B".to_owned(),
            instruction: "instr B".to_owned(),
            tool_call_id: Some("main-B".into()),
            task_name: Some("B".to_owned()),
        },
    )
    .expect("query B");

    let cid_a = h
        .conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-A"
            )
            .then_some(cid.clone())
        })
        .expect("conv A");
    let cid_b = h
        .conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-B"
            )
            .then_some(cid.clone())
        })
        .expect("conv B");

    let head_a_after_init = h.conversations.get(&cid_a).expect("conv A after init").head;
    let head_b_after_init = h.conversations.get(&cid_b).expect("conv B after init").head;
    assert!(head_a_after_init.is_some());
    assert!(head_b_after_init.is_some());
    assert_ne!(
        head_a_after_init, head_b_after_init,
        "the two side convs must point at distinct UserMessage nodes",
    );

    // Conv A's agent finishes with a tool call (no text → the
    // AgentResponseFinished itself does NOT fold a tree node).
    // After the response is processed, the harness emits a
    // ToolRequest for `A-tool` on conv-A's branch. That request must
    // be parented under conv-A's own `UserMessage` (head_a_after_init),
    // not conv-B's last fold.
    let spid_a = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_a).then_some(spid.clone()))
        .expect("spid A");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid_a,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "A-tool".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-A".to_owned(),
        },
    })
    .expect("A response");

    let tree = h.store.session("s1").expect("session tree");
    let a_tool_node = tree
        .nodes()
        .iter()
        .find(|n| {
            matches!(
                &n.entry,
                tau_core::SessionEntry::ToolActivity(rec)
                    if rec.call_id.as_str() == "A-tool"
            )
        })
        .expect("A-tool ToolActivity node");
    assert_eq!(
        a_tool_node.parent_id, head_a_after_init,
        "conv A's ToolRequest must be parented under conv A's UserMessage; \
         drift onto conv B would manifest here",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: when an interceptor is registered on
/// `ui.prompt_submitted` (e.g. `tau-ext-test-dummy`'s tao→tau
/// corrector), the side conversation's `UiPromptSubmitted` parks in
/// `pending_intercept` and `conv.head` stays `None`. If the harness
/// dispatched the agent prompt synchronously after the publish, the
/// assembled message list would be empty and the LLM provider 400s.
/// The dispatch must defer onto `pending_user_prompt_dispatches` and
/// run only after the user message commits.
#[test]
fn ext_agent_query_defers_dispatch_when_publish_is_intercepted() {
    use tau_proto::{ExtensionName, ToolNameMaybe};
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    // Register a no-op interceptor on `ui.prompt_submitted` so any
    // such publish parks in `pending_intercept` instead of committing
    // inline — same shape as `tau-ext-test-dummy`.
    let _interceptor_events = connect_test_tool(&mut h, "conn-interceptor");
    h.interceptors.replace_for_connection(
        "conn-interceptor",
        ExtensionName::from("test-interceptor"),
        vec![EventSelector::Exact(
            tau_proto::EventName::UI_PROMPT_SUBMITTED,
        )],
        InterceptionPriority(0),
    );

    // Drive the main agent into a delegate tool call. Reply Pass to
    // the main UserMessage's intercept first so the rest of the
    // setup proceeds normally.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "go".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_intercept_reply(
        "conn-interceptor",
        InterceptReply {
            action: InterceptAction::Pass(None),
        },
    );

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "main-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-side".to_owned(),
            instruction: "side instruction".to_owned(),
            tool_call_id: Some("main-call".into()),
            task_name: Some("side".to_owned()),
        },
    )
    .expect("ext query");

    // The side conv's UserMessage is parked for interception.
    // No SPC should have been emitted for it yet.
    let pre_reply_side_spid = h
        .prompt_conversations
        .iter()
        .find(|(_, prompt_cid)| prompt_cid.as_str() != "default");
    assert!(
        pre_reply_side_spid.is_none(),
        "side prompt must not dispatch before the intercepted UserMessage commits, got {pre_reply_side_spid:?}",
    );

    h.handle_intercept_reply(
        "conn-interceptor",
        InterceptReply {
            action: InterceptAction::Pass(None),
        },
    );

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt must dispatch after intercept resolves");
    let prompt = read_prompt_created(&h, &side_spid);
    assert!(
        !prompt.messages.is_empty(),
        "side prompt must contain the delegated user instruction; got empty messages",
    );
    let saw_instruction = prompt.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, tau_proto::ContentBlock::Text { text } if text == "side instruction")
        })
    });
    assert!(
        saw_instruction,
        "side prompt must contain `side instruction`; got {:?}",
        prompt.messages,
    );

    h.shutdown().expect("shutdown");
}

/// Tool-event originator should reflect the conversation that owns
/// the call, not a fixed `User`. Main-agent tool calls show
/// `PromptOriginator::User`; sub-agent tool calls show
/// `PromptOriginator::Extension { name, query_id }` matching the
/// side conversation. The harness re-stamps on publish, so
/// extensions don't have to track this themselves.
#[test]
fn tool_events_carry_owning_conversation_originator() {
    use tau_proto::ToolNameMaybe;
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: "delegate".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );

    // Subscribe a sink to tool.request so we can inspect originator.
    let sink = connect_test_tool(&mut h, "test-tool-req-sink");
    h.bus
        .set_subscriptions(
            "test-tool-req-sink",
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::TOOL_REQUEST,
            )],
        )
        .expect("subscribe");

    // Main agent submits a delegate call.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "kick off a delegate".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "main-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("main response");

    // Spawn the sub-agent and have IT call a tool too.
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q-sub".to_owned(),
            instruction: "sub task".to_owned(),
            tool_call_id: Some("main-call".into()),
            task_name: Some("sub".to_owned()),
        },
    )
    .expect("sub query");
    let sub_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("sub prompt id");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: sub_spid,
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "sub-call".into(),
            name: ToolNameMaybe::from_raw("delegate"),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-sub".to_owned(),
        },
    })
    .expect("sub response");

    let frames = sink.lock().expect("sink");
    let mut originators_by_call = std::collections::HashMap::new();
    for routed in frames.iter() {
        if let Frame::Message(tau_proto::Message::LogEvent(env)) = &routed.frame
            && let Event::ToolRequest(req) = env.event.as_ref()
        {
            originators_by_call.insert(req.call_id.as_str().to_owned(), req.originator.clone());
        }
    }
    drop(frames);

    assert!(
        matches!(
            originators_by_call.get("main-call"),
            Some(tau_proto::PromptOriginator::User)
        ),
        "main-agent tool call should be tagged User; got {:?}",
        originators_by_call.get("main-call"),
    );
    assert!(
        matches!(
            originators_by_call.get("sub-call"),
            Some(tau_proto::PromptOriginator::Extension { query_id, .. }) if query_id == "q-sub"
        ),
        "sub-agent tool call should be tagged Extension{{query_id=q-sub}}; got {:?}",
        originators_by_call.get("sub-call"),
    );

    h.shutdown().expect("shutdown");
}

/// Find the conversation id of the outer side conversation (the one
/// whose originator is the delegate extension's first query). Used by
/// the cross-conversation regression test above to disambiguate
/// nested-vs-outer side prompt ids.
fn outer_side_cid_str(h: &Harness) -> &str {
    h.conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-outer"
            )
            .then_some(cid.as_str())
        })
        .unwrap_or("")
}

/// Subscribe a fresh test sink to `tool.delegate_progress` events and
/// hand back its accumulator.
fn collect_event_sink(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = connect_test_tool(h, "test-delegate-progress-sink");
    h.bus
        .set_subscriptions(
            "test-delegate-progress-sink",
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::TOOL_DELEGATE_PROGRESS,
            )],
        )
        .expect("subscribe");
    events
}

/// Peel a routed frame to its bus-event payload, unwrapping the
/// `Message::LogEvent` envelope when present. Returns `None` for
/// non-event messages (Hello, Ack, …).
fn peel_inner_event(frame: &Frame) -> Option<&Event> {
    match frame {
        Frame::Event(event) => Some(event),
        Frame::Message(Message::LogEvent(env)) => Some(&env.event),
        Frame::Message(_) => None,
    }
}

fn pop_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Option<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let pos = events.iter().position(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id
        )
    })?;
    let removed = events.remove(pos);
    match removed.frame {
        Frame::Event(Event::ToolDelegateProgress(p)) => Some(p),
        Frame::Message(Message::LogEvent(env)) => match *env.event {
            Event::ToolDelegateProgress(p) => Some(p),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn drain_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Vec<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let mut out = Vec::new();
    events.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id => {
            out.push(p.clone());
            false
        }
        _ => true,
    });
    out
}

fn read_prompt_created(h: &Harness, spid: &SessionPromptId) -> SessionPromptCreated {
    let mut cursor = 0;
    loop {
        let entry = h
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionPromptCreated(prompt) if &prompt.session_prompt_id == spid => {
                return prompt;
            }
            _ => {}
        }
    }
}

fn intercepted_payload(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> (Event, bool) {
    let events = events.lock().expect("events mutex");
    let intercepted = events
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Message(Message::InterceptRequest(req)) => Some(req),
            _ => None,
        })
        .expect("intercept request delivered");
    ((*intercepted.event).clone(), intercepted.transient)
}

fn draft_event(text: &str) -> Event {
    Event::UiPromptDraft(UiPromptDraft {
        session_id: "s1".into(),
        text: text.to_owned(),
    })
}

#[test]
fn interception_exact_selector_intercepts_before_log() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    let start_seq = h.event_log.next_seq();

    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("held"));

    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, draft_event("held"));
    assert!(
        transient,
        "UiPromptDraft default transient flag is preserved"
    );
    assert_eq!(h.event_log.next_seq(), after_registration_seq);
    assert!(after_registration_seq < start_seq + 2);
}

#[test]
fn interception_drop_prevents_final_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    // UiPromptDraft is not on the must-pass list, so an explicit Drop
    // really does drop it.
    h.publish_event(None, draft_event("dropped"));
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    assert_eq!(h.event_log.next_seq(), after_registration_seq);
}

#[test]
fn interception_pass_through_reaches_log_after_last_interceptor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("released"));
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("released event in log");
    assert_eq!(entry.event, draft_event("released"));
}

#[test]
fn interception_reply_can_modify_event() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("original"));
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(draft_event("modified")))),
        })),
    )
    .expect("modifying reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("modified event in log");
    assert_eq!(entry.event, draft_event("modified"));
}

#[test]
fn interception_priority_orders_lower_values_first() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let low = connect_test_tool(&mut h, "low");
    let high = connect_test_tool(&mut h, "high");
    for (name, priority) in [("low", 10), ("high", 0)] {
        h.handle_extension_event(
            name,
            Frame::Message(Message::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority(priority),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));

    assert!(
        high.lock()
            .expect("high events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
    assert!(
        !low.lock()
            .expect("low events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
}

#[test]
fn interception_same_priority_orders_by_component_name_and_redelivery_continues() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let alpha = connect_test_tool(&mut h, "alpha");
    let beta = connect_test_tool(&mut h, "beta");
    for name in ["beta", "alpha"] {
        h.handle_extension_event(
            name,
            Frame::Message(Message::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("chain"));
    assert!(
        alpha
            .lock()
            .expect("alpha events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
    assert!(
        !beta
            .lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );

    h.handle_extension_event(
        "alpha",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("alpha pass");
    assert!(
        beta.lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
}

#[test]
fn interception_exact_beats_prefix_even_with_lower_prefix_priority() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let exact = connect_test_tool(&mut h, "exact");
    let prefix = connect_test_tool(&mut h, "prefix");
    h.handle_extension_event(
        "prefix",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("ui".to_owned())],
            priority: InterceptionPriority(-100),
        })),
    )
    .expect("prefix registration");
    h.handle_extension_event(
        "exact",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(100),
        })),
    )
    .expect("exact registration");

    h.publish_event(None, draft_event("exact"));

    assert!(
        exact
            .lock()
            .expect("exact events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
    assert!(
        !prefix
            .lock()
            .expect("prefix events")
            .iter()
            .any(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
    );
}

#[test]
fn interception_pass_advances_past_responding_interceptor() {
    // With the new InterceptReply protocol the cursor lives on the
    // harness side and always advances strictly past the interceptor
    // that just replied. The old "Emit with interception: None
    // restarts" pattern is gone — a Pass(None) reply does *not* loop
    // the event back through the same interceptor.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");

    h.publish_event(None, draft_event("once"));
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let count = interceptor
        .lock()
        .expect("events")
        .iter()
        .filter(|event| matches!(event.frame, Frame::Message(Message::InterceptRequest(_))))
        .count();
    assert_eq!(
        count, 1,
        "pass-through must not re-trigger the same interceptor"
    );
}

#[test]
fn interception_defers_subsequent_publishes_until_reply() {
    // Regression for the "Ready" loop: while one publish is parked
    // waiting on an InterceptReply, the harness must defer any
    // subsequent publishes rather than commit them out of order.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    // Publish two: the first parks in interception (matches the
    // selector); the second does NOT match and so would, in the
    // buggy world, race ahead of it.
    h.publish_event(None, draft_event("held"));
    h.publish_event(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "second".to_owned(),
            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );
    // Neither has committed yet — interception is in flight on the
    // first, the second is sitting in `deferred_publishes`.
    assert_eq!(h.event_log.next_seq(), baseline_seq);

    // Reply: pass-through. Both events should now commit, in order.
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let first = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first event committed");
    assert_eq!(first.event, draft_event("held"));
    let second = h
        .event_log
        .get_next_from(first.seq + 1)
        .expect("second event committed");
    assert!(matches!(
        &second.event,
        Event::HarnessInfo(info) if info.message == "second"
    ));
}

#[test]
fn interception_drop_of_must_pass_event_is_overridden() {
    // UiPromptSubmitted is on the MUST_PASS list — even if an
    // interceptor returns Drop, the harness must publish the
    // original event (with a warn).
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::UI_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let prompt = Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    h.publish_event(None, prompt.clone());
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("must-pass event still committed despite Drop");
    assert_eq!(entry.event, prompt);
}

#[test]
fn interception_disconnect_mid_reply_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("inflight"));
    // Disconnect before the interceptor replies. The harness should
    // treat this as Pass(None) and still commit the event.
    h.handle_disconnect("interceptor");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("event committed after disconnect");
    assert_eq!(entry.event, draft_event("inflight"));
}

#[test]
fn interception_user_prompt_dispatch_waits_for_commit() {
    // Regression for the "Ready" loop. When `UiPromptSubmitted` is
    // held in interception, the harness must not dispatch the agent
    // prompt against the pre-prompt conversation tail — the
    // assembled message list must include the just-committed user
    // message. We assert this by inspecting the conversation
    // head/tree before vs. after the intercept reply lands.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::UI_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");

    let cid = h.default_conversation_id.clone();
    let head_before_dispatch = h.conversations.get(&cid).and_then(|c| c.head);
    let next_sp_before = h.next_session_prompt_id;

    // Drive the user-prompt path. The publish parks in interception.
    h.dispatch_prompt_for_conversation(&cid, "real question".to_owned())
        .expect("dispatch");

    // While the intercept is in flight: no agent prompt was minted,
    // c.head hasn't moved, and the deferred-dispatch queue contains
    // our cid.
    assert_eq!(
        h.next_session_prompt_id, next_sp_before,
        "agent dispatch must wait until the prompt commits"
    );
    assert_eq!(
        h.conversations.get(&cid).and_then(|c| c.head),
        head_before_dispatch,
        "c.head must not advance while the prompt is parked"
    );
    assert_eq!(h.pending_user_prompt_dispatches.len(), 1);

    // Reply pass-through. Commit + react fires the deferred
    // dispatch, and the SessionPromptCreated is built from the
    // updated tree.
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    assert_eq!(h.pending_user_prompt_dispatches.len(), 0);
    assert_eq!(
        h.next_session_prompt_id,
        next_sp_before + 1,
        "agent dispatch fires once the prompt commits"
    );
    let head_after = h
        .conversations
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let tree = h.store.session(session_id.as_str()).expect("session tree");
    let entry = tree.node(head_after).expect("head node");
    assert!(
        matches!(
            &entry.entry,
            SessionEntry::UserMessage { text } if text == "real question"
        ),
        "c.head points at the just-committed user prompt"
    );
}

#[test]
fn interception_mutating_prompt_reaches_agent() {
    // End-to-end check that mirrors the test-dummy's "Tao → Tau"
    // correction flow: an interceptor replies with
    // `Pass(Some(modified))` and the agent receives the modified
    // text in its message list. Verifies the full chain (intercept
    // request → reply with mutation → fold of mutated event →
    // c.head sync → agent dispatch with up-to-date branch) end-to-
    // end.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::UI_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");

    let cid = h.default_conversation_id.clone();
    h.dispatch_prompt_for_conversation(&cid, "I love Tao".to_owned())
        .expect("dispatch");

    // Interceptor replies with the mutated event.
    let mutated = Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.clone(),
        text: "I love Tau".to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(mutated))),
        })),
    )
    .expect("intercept reply");

    // The committed user message reflects the *mutated* text — and
    // c.head points at it (see `interception_user_prompt_dispatch_
    // waits_for_commit` for the dispatch-side assertion).
    let head = h
        .conversations
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let tree = h.store.session(session_id.as_str()).expect("session tree");
    let entry = tree.node(head).expect("head node");
    assert!(
        matches!(
            &entry.entry,
            SessionEntry::UserMessage { text } if text == "I love Tau"
        ),
        "the agent will see the *interceptor-mutated* text, not the user's typo"
    );
}

#[test]
fn publish_for_conversation_does_not_emit_navigate_tree() {
    // Phase 4: cross-conversation publishes used to bounce
    // `tree.head()` via a `UiNavigateTree` event before folding the
    // real event. With explicit-parent folds in
    // `SessionTree::apply_event_at`, the bounce is gone — the harness
    // stamps the conversation's `head` directly.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let baseline_seq = h.event_log.next_seq();
    let cid = h.default_conversation_id.clone();

    // Two prompts in a row on the same conversation. Either would
    // historically have caused `publish_for_conversation_from` to
    // bounce `tree.head()` via `UiNavigateTree`.
    h.dispatch_prompt_for_conversation(&cid, "first".to_owned())
        .expect("first dispatch");
    h.dispatch_prompt_for_conversation(&cid, "second".to_owned())
        .expect("second dispatch");

    let mut navigates = 0;
    let mut user_msgs = 0;
    let mut id = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(id) {
        match &entry.event {
            Event::UiNavigateTree(_) => navigates += 1,
            Event::UiPromptSubmitted(_) => user_msgs += 1,
            _ => {}
        }
        id = entry.seq + 1;
    }
    assert_eq!(
        navigates, 0,
        "cross-conversation publishes must not emit UiNavigateTree anymore"
    );
    assert_eq!(user_msgs, 2);
}

#[test]
fn interception_disconnect_clears_registration() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority(0),
        })),
    )
    .expect("intercept registration");
    h.handle_disconnect("interceptor");
    let after_disconnect_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("not intercepted"));

    let entry = h
        .event_log
        .get_next_from(after_disconnect_seq)
        .expect("event reaches log");
    assert_eq!(entry.event, draft_event("not intercepted"));
}
