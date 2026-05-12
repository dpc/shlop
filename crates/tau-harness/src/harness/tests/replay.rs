use super::*;

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
        events
            .iter()
            .all(|entry| !entry.event.defaults_to_transient()),
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
            model_params: Default::default(),
            tool_choice: Default::default(),
            originator: Default::default(),
            ctx_id: None,
            previous_response: None,
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
            output_tokens: None,
            thinking: None,
            token_usage: None,

            backend: None,
            response_id: None,
            phase: None,
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

#[test]
fn resumed_harness_replays_persisted_session_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    {
        let mut h = echo_harness_for("s1", &sp).expect("start");
        h.selected_model = Some("test/model".into());

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
            output_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
            response_id: None,
            phase: None,
        })
        .expect("persist agent response");

        h.shutdown().expect("shutdown");
    }

    let mut resumed = echo_harness_for("s1", &sp).expect("resume");
    resumed.selected_model = Some("test/model".into());

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
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "first");

    let spid1 = h.send_prompt_to_agent("s1");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: Some("The user is asking ...".to_owned()),
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
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
