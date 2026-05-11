use std::cell::RefCell;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    EventBus, RoutedFrame, SessionStore, ToolRegistry, memory_connection,
};
use tau_proto::{
    ClientKind, ConnectionId, Event, EventName, EventSelector, Frame, FrameReader, FrameWriter,
    SessionPromptCreated,
};
use tempfile::TempDir;

use super::*;

#[test]
fn runtime_supports_embedded_and_daemon_scenarios() {
    let runtime = TestRuntime::new().expect("runtime should be created");

    let embedded = runtime
        .run_embedded("session-1", "hello")
        .expect("embedded run should succeed");
    assert!(!embedded.is_empty(), "response should not be empty");

    let daemon = runtime.spawn_daemon("session-2", Some(1));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");
    let attached = runtime
        .send_daemon_message("session-2", "hello")
        .expect("daemon message should succeed");
    assert!(!attached.is_empty(), "response should not be empty");
    daemon.join().expect("daemon should exit cleanly");
}

struct StreamSink {
    writer: Rc<RefCell<FrameWriter<BufWriter<UnixStream>>>>,
}

impl ConnectionSink for StreamSink {
    fn send(&mut self, routed: RoutedFrame) -> Result<(), ConnectionSendError> {
        let mut writer = self.writer.borrow_mut();
        writer
            .write_frame(&routed.frame)
            .map_err(|error| ConnectionSendError::new(error.to_string()))?;
        writer
            .flush()
            .map_err(|error| ConnectionSendError::new(error.to_string()))
    }
}

fn stream_connection(
    name: &str,
    kind: ClientKind,
    stream: UnixStream,
) -> (Connection, FrameReader<BufReader<UnixStream>>) {
    let writer_stream = stream
        .try_clone()
        .expect("stream clone for writer should succeed");
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(StreamSink {
            writer: Rc::new(RefCell::new(FrameWriter::new(BufWriter::new(
                writer_stream,
            )))),
        }),
    );
    let reader = FrameReader::new(BufReader::new(stream));
    (connection, reader)
}

/// End-to-end vertical slice: real `tau-agent` and `tau-ext-shell`
/// processes wired through a `tau-core` bus, asserting the protocol
/// handshake and a no-model agent response. Lives here (rather than
/// inside `tau-core`'s tests) because the agent + extension layers
/// sit downstream of `tau-core`; keeping the test here avoids
/// declaring `tau-agent` and `tau-ext-shell` as dev-dependencies of
/// the very crate they depend on.
#[test]
fn deterministic_agent_and_tool_complete_one_vertical_slice() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let _store = SessionStore::open(&store_path).expect("store should open");
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (agent_runtime_stream, agent_harness_stream) =
        UnixStream::pair().expect("agent stream pair should open");
    let (tool_runtime_stream, tool_harness_stream) =
        UnixStream::pair().expect("tool stream pair should open");

    let agent_thread = thread::spawn(move || {
        let agent_reader = agent_runtime_stream
            .try_clone()
            .expect("agent reader clone should succeed");
        tau_agent::run(agent_reader, agent_runtime_stream).expect("agent should run successfully");
    });
    let tool_thread = thread::spawn(move || {
        let tool_reader = tool_runtime_stream
            .try_clone()
            .expect("tool reader clone should succeed");
        tau_ext_shell::run(tool_reader, tool_runtime_stream)
            .expect("tool extension should run successfully");
    });

    let (agent_connection, mut agent_reader) =
        stream_connection("agent", ClientKind::Agent, agent_harness_stream);
    let (tool_connection, mut tool_reader) =
        stream_connection("tool", ClientKind::Tool, tool_harness_stream);
    let agent_id = bus.connect(agent_connection);
    let tool_id = bus.connect(tool_connection);

    let (ui_connection, _ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let ui_id = bus.connect(ui_connection);
    bus.set_subscriptions(
        &ui_id,
        vec![EventSelector::Exact(EventName::AGENT_RESPONSE_FINISHED)],
    )
    .expect("ui subscription should be stored");

    // Read and process the agent's startup frames (hello, subscribe, ready).
    // Subscribe is now a `Message`, not an `Event`, so we install
    // subscriptions directly via `set_subscriptions` rather than
    // republishing the message.
    let agent_hello = agent_reader
        .read_frame()
        .expect("read")
        .expect("agent hello should arrive");
    assert!(matches!(
        agent_hello,
        Frame::Message(tau_proto::Message::Hello(_))
    ));
    let agent_subscribe = agent_reader
        .read_frame()
        .expect("read")
        .expect("agent subscribe should arrive");
    if let Frame::Message(tau_proto::Message::Subscribe(sub)) = agent_subscribe {
        bus.set_subscriptions(&agent_id, sub.selectors)
            .expect("agent subscriptions should be stored");
    } else {
        panic!("expected agent subscribe message");
    }
    let agent_ready = agent_reader
        .read_frame()
        .expect("read")
        .expect("agent ready should arrive");
    assert!(matches!(
        agent_ready,
        Frame::Message(tau_proto::Message::Ready(_))
    ));

    let tool_hello = tool_reader
        .read_frame()
        .expect("read")
        .expect("tool hello should arrive");
    assert!(matches!(
        tool_hello,
        Frame::Message(tau_proto::Message::Hello(_))
    ));
    let tool_subscribe = tool_reader
        .read_frame()
        .expect("read")
        .expect("tool subscribe should arrive");
    if let Frame::Message(tau_proto::Message::Subscribe(sub)) = tool_subscribe {
        bus.set_subscriptions(&tool_id, sub.selectors)
            .expect("tool subscriptions should be stored");
    } else {
        panic!("expected tool subscribe message");
    }
    let mut registered_tool_names = Vec::new();
    loop {
        let startup_frame = tool_reader
            .read_frame()
            .expect("read")
            .expect("tool startup event should arrive");
        match startup_frame {
            Frame::Event(Event::ToolRegister(tool_register)) => {
                let register_report = registry.register(&tool_id, tool_register.tool.clone());
                assert!(register_report.warnings.is_empty());
                registered_tool_names.push(tool_register.tool.name);
            }
            Frame::Message(tau_proto::Message::Ready(_)) => break,
            _ => panic!("unexpected tool startup event"),
        }
    }
    assert!(registered_tool_names.iter().any(|name| name == "echo"));
    assert!(registered_tool_names.iter().any(|name| name == "read"));

    // Send a SessionPromptCreated to the agent (new protocol).
    use tau_proto::{ContentBlock, ConversationMessage, ConversationRole, ToolDefinition};

    let prompt = SessionPromptCreated {
        session_prompt_id: "sp-1".into(),
        session_id: "session-1".into(),
        system_prompt: "You are helpful.".to_owned(),
        messages: vec![ConversationMessage {
            role: ConversationRole::User,
            content: vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "echo".into(),
            description: None,
            parameters: None,
        }],
        model: None,
        effort: tau_proto::Effort::Off,
        thinking_summary: tau_proto::ThinkingSummary::Off,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    };
    let _ = bus.send_to(
        &agent_id,
        None,
        Frame::Event(Event::SessionPromptCreated(prompt)),
    );

    // Without a model, the agent should report an error.
    let response = loop {
        let frame = agent_reader
            .read_frame()
            .expect("read")
            .expect("agent event should arrive");
        if let Frame::Event(Event::AgentResponseFinished(r)) = frame {
            break r;
        }
    };
    assert!(response.text.as_deref().unwrap_or("").contains("no model"));
    assert!(response.tool_calls.is_empty());

    bus.send_to(
        &agent_id,
        Some(&ui_id),
        Frame::Message(tau_proto::Message::Disconnect(tau_proto::Disconnect {
            reason: Some("test complete".to_owned()),
        })),
    )
    .expect("agent disconnect should route");
    bus.send_to(
        &tool_id,
        Some(&ui_id),
        Frame::Message(tau_proto::Message::Disconnect(tau_proto::Disconnect {
            reason: Some("test complete".to_owned()),
        })),
    )
    .expect("tool disconnect should route");

    agent_thread.join().expect("agent thread should finish");
    tool_thread.join().expect("tool thread should finish");
}
