use super::*;

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
    let tool_name = ToolName::new("shell");
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
    h.selected_model = Some("test/model".into());
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("delegate"),
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
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

    h.selected_model = Some("test/model".into());
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("delegate"),
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
                display: None,
            },
            AgentToolCall {
                id: "".into(),
                name: "".into(),
                arguments: CborValue::Map(Vec::new()),
                display: None,
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
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

    let tools = h.extensions.get(tools_id.as_str()).expect("entry");
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
        .get(tools_id.as_str())
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
        .get(tools_id.as_str())
        .expect("entry")
        .last_acked;
    assert_eq!(before, after, "stale ack should not change cursor");
    h.shutdown().expect("shutdown");
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
            tool_name: ToolName::new("read"),
            result: tau_proto::CborValue::Text("stale data".to_owned()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}
