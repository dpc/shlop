use std::cell::RefCell;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use tau_proto::{
    AgentResponseFinished, CborValue, ClientKind, ConnectionId, Event, EventName, EventReader,
    EventSelector, EventWriter, LifecycleSubscribe, SessionPromptCreated, ToolRegister,
    ToolRequest, ToolSideEffects, ToolSpec, UiPromptSubmitted,
};
use tempfile::TempDir;

use crate::bus::EventBus;
use crate::connection::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    RouteError, RoutedEvent,
};
use crate::event_log::EventLog;
use crate::memory::{MemoryInbox, MemorySink, memory_connection};
use crate::policy::{DefaultSubscriptionPolicy, PolicyStore, SubscriptionApproval};
use crate::session::{NodeId, SessionEntry, ToolActivityOutcome, ToolActivityRecord};
use crate::session_store::SessionStore;
use crate::tool_registry::{ToolRegistry, ToolRegistryWarning};

struct StreamSink {
    writer: Rc<RefCell<EventWriter<BufWriter<UnixStream>>>>,
}

impl ConnectionSink for StreamSink {
    fn send(&mut self, event: RoutedEvent) -> Result<(), ConnectionSendError> {
        let mut writer = self.writer.borrow_mut();
        writer
            .write_event(&event.event)
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
) -> (Connection, EventReader<BufReader<UnixStream>>) {
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
            writer: Rc::new(RefCell::new(EventWriter::new(BufWriter::new(
                writer_stream,
            )))),
        }),
    );
    let reader = EventReader::new(BufReader::new(stream));
    (connection, reader)
}

#[test]
fn subscribed_clients_only_receive_matching_events() {
    let mut bus = EventBus::new();

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let agent_id = bus.connect(agent_connection);
    let ui_id = bus.connect(ui_connection);

    bus.set_subscriptions(
        &agent_id,
        vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
    )
    .expect("agent subscriptions should be stored");
    bus.set_subscriptions(&ui_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("ui subscriptions should be stored");

    let report = bus.publish(Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".to_owned(),
    }));

    assert_eq!(report.delivered_to, vec![agent_id.clone()]);
    assert_eq!(report.skipped_by_subscription, vec![ui_id.clone()]);
    assert_eq!(agent_inbox.snapshot().len(), 1);
    assert!(ui_inbox.snapshot().is_empty());
}

#[test]
fn publish_from_updates_subscription_registry_from_lifecycle_subscribe() {
    let mut bus = EventBus::new();

    let (connection, inbox) = memory_connection("agent", ClientKind::Agent);
    let connection_id = bus.connect(connection);
    bus.set_subscriptions(
        &connection_id,
        vec![EventSelector::Exact(EventName::LIFECYCLE_SUBSCRIBE)],
    )
    .expect("initial subscriptions should be stored");

    let report = bus.publish_from(
        Some(&connection_id),
        Event::LifecycleSubscribe(LifecycleSubscribe {
            selectors: vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
        }),
    );

    assert_eq!(report.delivered_to, Vec::<ConnectionId>::new());
    assert_eq!(
        bus.subscriptions(&connection_id),
        Some([EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)].as_slice())
    );
    assert!(inbox.snapshot().is_empty());
}

#[test]
fn directed_events_ignore_subscriptions_but_still_use_visibility_filters() {
    let mut bus = EventBus::new();

    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let filtered_connection =
        ui_connection.with_visibility_filter(Box::new(|event: &RoutedEvent| {
            event.event.name() == EventName::TOOL_INVOKE
        }));
    let ui_id = bus.connect(filtered_connection);

    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let tool_id = bus.connect(tool_connection);
    bus.set_subscriptions(&tool_id, Vec::new())
        .expect("tool subscriptions should be stored");

    let blocked = bus
        .send_to(
            &ui_id,
            Some(&tool_id),
            Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                text: Some("hidden".to_owned()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                thinking: None,
            }),
        )
        .expect("directed route should succeed");
    assert_eq!(blocked.blocked_by_filter, vec![ui_id.clone()]);
    assert!(ui_inbox.snapshot().is_empty());

    let delivered = bus
        .send_to(
            &ui_id,
            Some(&tool_id),
            Event::ToolInvoke(tau_proto::ToolInvoke {
                call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: CborValue::Null,
            }),
        )
        .expect("directed route should succeed");
    assert_eq!(delivered.delivered_to, vec![ui_id.clone()]);
    assert_eq!(ui_inbox.snapshot().len(), 1);
    assert!(tool_inbox.snapshot().is_empty());
}

#[test]
fn connection_abstraction_is_transport_independent_for_in_memory_clients() {
    let mut bus = EventBus::new();

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let agent_id = bus.connect(agent_connection);
    let tool_id = bus.connect(tool_connection);

    bus.set_subscriptions(&agent_id, vec![EventSelector::Prefix("agent.".to_owned())])
        .expect("agent subscriptions should be stored");
    bus.set_subscriptions(&tool_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("tool subscriptions should be stored");

    let first_report = bus.publish(Event::ToolResult(tau_proto::ToolResult {
        call_id: "call-1".into(),
        tool_name: "echo".into(),
        result: CborValue::Text("done".to_owned()),
    }));
    assert_eq!(first_report.delivered_to, vec![tool_id.clone()]);

    let second_report = bus.publish(Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-1".into(),
        text: Some("done".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
    }));
    assert_eq!(second_report.delivered_to, vec![agent_id.clone()]);

    assert_eq!(tool_inbox.snapshot().len(), 1);
    assert_eq!(agent_inbox.snapshot().len(), 1);
}

#[test]
fn provider_can_register_tool_and_receive_invocations() {
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let agent_id = bus.connect(agent_connection);
    let tool_id = bus.connect(tool_connection);

    let register_report = registry.register(
        &tool_id,
        ToolSpec {
            name: "echo".into(),
            description: Some("Echo a payload".to_owned()),
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );
    assert!(register_report.warnings.is_empty());

    let route_report = registry
        .route_tool_request(
            &mut bus,
            &agent_id,
            ToolRequest {
                call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: CborValue::Text("hello".to_owned()),
            },
        )
        .expect("tool request should route");

    assert_eq!(route_report.provider_connection_id, tool_id.clone());
    assert_eq!(
        route_report.route_report.delivered_to,
        vec![tool_id.clone()]
    );
    assert!(agent_inbox.snapshot().is_empty());

    let delivered_events = tool_inbox.snapshot();
    assert_eq!(delivered_events.len(), 1);
    assert_eq!(delivered_events[0].source_id, Some(agent_id));
    assert_eq!(
        delivered_events[0].event,
        Event::ToolInvoke(tau_proto::ToolInvoke {
            call_id: "call-1".into(),
            tool_name: "echo".into(),
            arguments: CborValue::Text("hello".to_owned()),
        })
    );
}

#[test]
fn duplicate_tool_registrations_warn_but_remain_available() {
    let mut registry = ToolRegistry::new();

    let first_report = registry.register(
        "conn-a",
        ToolSpec {
            name: "echo".into(),
            description: Some("Echo".to_owned()),
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );
    assert!(first_report.warnings.is_empty());

    let second_report = registry.register(
        "conn-b",
        ToolSpec {
            name: "echo".into(),
            description: Some("Echo from another provider".to_owned()),
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );
    assert_eq!(second_report.warnings.len(), 1);
    assert_eq!(
        second_report.warnings[0],
        ToolRegistryWarning::DuplicateRegistration {
            tool_name: "echo".into(),
            existing_provider_ids: vec!["conn-a".into()],
        }
    );

    let providers = registry.providers_for("echo");
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0].connection_id, "conn-a");
    assert_eq!(providers[1].connection_id, "conn-b");
}

#[test]
fn disconnect_cleanup_removes_stale_tool_providers() {
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (first_connection, _first_inbox) = memory_connection("tool-a", ClientKind::Tool);
    let (second_connection, _second_inbox) = memory_connection("tool-b", ClientKind::Tool);
    let first_id = bus.connect(first_connection);
    let second_id = bus.connect(second_connection);

    registry.register(
        &first_id,
        ToolSpec {
            name: "echo".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );
    registry.register(
        &second_id,
        ToolSpec {
            name: "echo".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );
    registry.register(
        &first_id,
        ToolSpec {
            name: "demo_upper".into(),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Pure,
        },
    );

    let disconnected = bus.disconnect(&first_id);
    assert!(disconnected.is_some());
    let removed_tools = registry.unregister_connection(&first_id);
    assert_eq!(removed_tools.len(), 2);
    assert!(removed_tools.iter().any(|tool_name| tool_name == "echo"));
    assert!(
        removed_tools
            .iter()
            .any(|tool_name| tool_name == "demo_upper")
    );

    let echo_providers = registry.providers_for("echo");
    assert_eq!(echo_providers.len(), 1);
    assert_eq!(echo_providers[0].connection_id, second_id);
    assert!(registry.providers_for("demo_upper").is_empty());
}

#[test]
fn register_events_map_cleanly_to_registry_state() {
    let mut registry = ToolRegistry::new();

    let report = registry.register(
        "conn-tool",
        ToolRegister {
            tool: ToolSpec {
                name: "echo".into(),
                description: Some("Echo".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        }
        .tool,
    );

    assert!(report.warnings.is_empty());
    assert_eq!(registry.providers_for("echo").len(), 1);
    assert!(registry.unregister("conn-tool", "echo"));
    assert!(registry.providers_for("echo").is_empty());
}

#[test]
fn session_tree_persists_across_reopen() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let id0 = store
        .append_user_message("session-1", "hello")
        .expect("user message should persist");
    let id1 = store
        .append_agent_message("session-1", "hi there")
        .expect("agent message should persist");

    assert_eq!(id0, NodeId(0));
    assert_eq!(id1, NodeId(1));

    let reopened = SessionStore::open(&store_path).expect("store should reopen");
    let tree = reopened
        .session("session-1")
        .expect("session should reload");
    assert_eq!(tree.head(), Some(NodeId(1)));
    assert_eq!(
        tree.current_branch(),
        vec![
            &SessionEntry::UserMessage {
                text: "hello".to_owned(),
            },
            &SessionEntry::AgentMessage {
                text: "hi there".to_owned(),
                thinking: None,
            },
        ]
    );
    // Verify tree structure.
    assert!(tree.node(NodeId(0)).expect("node 0").parent_id.is_none());
    assert_eq!(
        tree.node(NodeId(1)).expect("node 1").parent_id,
        Some(NodeId(0))
    );
}

#[test]
fn session_tree_supports_branching() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store.append_user_message("s1", "hello").expect("ok");
    let _ = store.append_agent_message("s1", "hi").expect("ok");
    // Branch: go back to node 0 and append a different message.
    store
        .set_head("s1", NodeId(0))
        .expect("set_head should work");
    let _ = store.append_user_message("s1", "goodbye").expect("ok");

    let tree = store.session("s1").expect("session should exist");
    assert_eq!(tree.head(), Some(NodeId(2)));
    // Current branch: hello → goodbye (skipping "hi").
    assert_eq!(
        tree.current_branch(),
        vec![
            &SessionEntry::UserMessage {
                text: "hello".to_owned(),
            },
            &SessionEntry::UserMessage {
                text: "goodbye".to_owned(),
            },
        ]
    );
    // Node 0 has two children (branching point).
    let mut children = tree.children(NodeId(0));
    children.sort_by_key(|id| id.0);
    assert_eq!(children, vec![NodeId(1), NodeId(2)]);

    // Verify persistence across reopen.
    let reopened = SessionStore::open(&store_path).expect("reopen");
    let tree2 = reopened.session("s1").expect("session");
    assert_eq!(tree2.head(), Some(NodeId(2)));
    assert_eq!(tree2.current_branch().len(), 2);
}

#[test]
fn session_tree_associates_tool_activity() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    store
        .append_user_message("session-1", "read a file")
        .expect("user message should persist");
    store
        .append_tool_activity(
            "session-1",
            ToolActivityRecord {
                call_id: "call-1".into(),
                tool_name: "read".into(),
                outcome: ToolActivityOutcome::Result {
                    result: CborValue::Text("README".to_owned()),
                },
            },
        )
        .expect("tool activity should persist");

    let reopened = SessionStore::open(&store_path).expect("store should reopen");
    let tree = reopened
        .session("session-1")
        .expect("session should reload");
    let branch = tree.current_branch();
    assert_eq!(branch.len(), 2);
    assert_eq!(
        *branch[1],
        SessionEntry::ToolActivity(ToolActivityRecord {
            call_id: "call-1".into(),
            tool_name: "read".into(),
            outcome: ToolActivityOutcome::Result {
                result: CborValue::Text("README".to_owned()),
            },
        })
    );
}

#[test]
fn socket_clients_are_denied_forbidden_subscriptions() {
    let mut bus = EventBus::new();
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: "socket-ui".to_owned(),
            kind: ClientKind::Ui,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(MemorySink { inbox }),
    );
    let connection_id = bus.connect(connection);

    let error = bus
        .set_subscriptions(
            &connection_id,
            vec![EventSelector::Prefix("lifecycle.".to_owned())],
        )
        .expect_err("socket lifecycle subscription should be denied");
    assert_eq!(
        error,
        RouteError::SubscriptionDenied {
            connection_id,
            reason: "socket clients may only subscribe to allowed event families".to_owned(),
        }
    );
}

#[test]
fn policy_store_persists_allowed_socket_subscriptions() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let policy_path = tempdir.path().join("policy.cbor");
    let store = PolicyStore::open(&policy_path).expect("policy store should open");
    let mut bus =
        EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(store)));
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: "socket-ui".to_owned(),
            kind: ClientKind::Ui,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(MemorySink { inbox }),
    );
    let connection_id = bus.connect(connection);

    bus.set_subscriptions(
        &connection_id,
        vec![EventSelector::Prefix("tool.".to_owned())],
    )
    .expect("allowed socket subscription should persist");

    let reopened = PolicyStore::open(&policy_path).expect("policy store should reopen");
    assert_eq!(
        reopened.approvals(),
        [SubscriptionApproval {
            connection_name: "socket-ui".to_owned(),
            connection_origin: ConnectionOrigin::Socket,
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
        }]
        .as_slice()
    );
}

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
        tau_ext_shell::run(tool_reader, tool_runtime_stream, true)
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

    let agent_hello = agent_reader
        .read_event()
        .expect("read")
        .expect("agent hello should arrive");
    assert!(matches!(agent_hello, Event::LifecycleHello(_)));
    let _ = bus.publish_from(Some(&agent_id), agent_hello);
    let agent_subscribe = agent_reader
        .read_event()
        .expect("read")
        .expect("agent subscribe should arrive");
    let _ = bus.publish_from(Some(&agent_id), agent_subscribe);
    let agent_ready = agent_reader
        .read_event()
        .expect("read")
        .expect("agent ready should arrive");
    assert!(matches!(agent_ready, Event::LifecycleReady(_)));

    let tool_hello = tool_reader
        .read_event()
        .expect("read")
        .expect("tool hello should arrive");
    assert!(matches!(tool_hello, Event::LifecycleHello(_)));
    let _ = bus.publish_from(Some(&tool_id), tool_hello);
    let tool_subscribe = tool_reader
        .read_event()
        .expect("read")
        .expect("tool subscribe should arrive");
    let _ = bus.publish_from(Some(&tool_id), tool_subscribe);
    let mut registered_tool_names = Vec::new();
    loop {
        let startup_event = tool_reader
            .read_event()
            .expect("read")
            .expect("tool startup event should arrive");
        match startup_event {
            Event::ToolRegister(tool_register) => {
                let register_report = registry.register(&tool_id, tool_register.tool.clone());
                assert!(register_report.warnings.is_empty());
                registered_tool_names.push(tool_register.tool.name);
            }
            Event::LifecycleReady(_) => break,
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
    };
    let _ = bus.send_to(&agent_id, None, Event::SessionPromptCreated(prompt));

    // Without a model, the agent should report an error.
    let response = loop {
        let ev = agent_reader
            .read_event()
            .expect("read")
            .expect("agent event should arrive");
        if let Event::AgentResponseFinished(r) = ev {
            break r;
        }
    };
    assert!(response.text.as_deref().unwrap_or("").contains("no model"));
    assert!(response.tool_calls.is_empty());

    bus.send_to(
        &agent_id,
        Some(&ui_id),
        Event::LifecycleDisconnect(tau_proto::LifecycleDisconnect {
            reason: Some("test complete".to_owned()),
        }),
    )
    .expect("agent disconnect should route");
    bus.send_to(
        &tool_id,
        Some(&ui_id),
        Event::LifecycleDisconnect(tau_proto::LifecycleDisconnect {
            reason: Some("test complete".to_owned()),
        }),
    )
    .expect("tool disconnect should route");

    agent_thread.join().expect("agent thread should finish");
    tool_thread.join().expect("tool thread should finish");
}

// -----------------------------------------------------------------------
// EventLog tests
// -----------------------------------------------------------------------

#[test]
fn event_log_append_and_get() {
    let log = EventLog::new();
    let seq = log.append(
        Some("conn-1".into()),
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "hello".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );
    assert_eq!(seq, 0);
    assert_eq!(log.next_seq(), 1);

    let entry = log.get_next_from(0).expect("entry should exist");
    assert_eq!(entry.seq, 0);
    assert_eq!(entry.source, Some("conn-1".into()));

    assert!(log.get_next_from(1).is_none());
}

#[test]
fn event_log_get_next_from_skips_earlier() {
    let log = EventLog::new();
    log.append(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "a".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );
    log.append(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "b".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );
    log.append(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "c".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );

    let entry = log.get_next_from(1).expect("entry should exist");
    assert_eq!(entry.seq, 1);
    let Event::HarnessInfo(info) = &entry.event else {
        panic!("expected HarnessInfo");
    };
    assert_eq!(info.message, "b");
}

#[test]
fn event_log_wait_next_from_blocks_then_returns() {
    let log = EventLog::new();
    let log2 = Arc::clone(&log);

    let handle = thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(20));
        log2.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "delayed".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );
    });

    let entry = log.wait_next_from(0);
    assert_eq!(entry.seq, 0);
    handle.join().expect("append thread");
}

#[test]
fn event_log_wait_next_from_returns_immediately_if_available() {
    let log = EventLog::new();
    log.append(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "already here".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );

    let entry = log.wait_next_from(0);
    assert_eq!(entry.seq, 0);
}

#[test]
fn event_log_prune_below_removes_old_entries() {
    let log = EventLog::new();
    for i in 0..5 {
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: format!("msg-{i}"),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );
    }
    assert_eq!(log.next_seq(), 5);

    log.prune_below(3);

    assert!(log.get_next_from(0).is_some());
    // The first available entry should be seq 3.
    let entry = log.get_next_from(0).expect("entry after prune");
    assert_eq!(entry.seq, 3);

    // Entries 0, 1, 2 are gone.
    assert!(log.get_next_from(2).map(|e| e.seq) == Some(3));
}

#[test]
fn event_log_multiple_waiters_wake_on_append() {
    let log = EventLog::new();
    let mut handles = Vec::new();
    for _ in 0..3 {
        let log = Arc::clone(&log);
        handles.push(thread::spawn(move || {
            let entry = log.wait_next_from(0);
            entry.seq
        }));
    }

    thread::sleep(std::time::Duration::from_millis(20));
    log.append(
        None,
        Event::HarnessInfo(tau_proto::HarnessInfo {
            message: "wake all".to_owned(),

            level: tau_proto::HarnessInfoLevel::Normal,
        }),
    );

    for h in handles {
        assert_eq!(h.join().expect("waiter thread"), 0);
    }
}
