use super::*;

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

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
        InterceptionPriority::new(0),
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
        token_usage: None,
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
                priority: InterceptionPriority::new(priority),
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
                priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(-100),
        })),
    )
    .expect("prefix registration");
    h.handle_extension_event(
        "exact",
        Frame::Message(Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(100),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
            priority: InterceptionPriority::new(0),
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
