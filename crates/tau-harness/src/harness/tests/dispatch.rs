use super::*;

#[test]
fn cross_session_prompt_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.selected_model = Some("test/model".into());
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

#[test]
fn pure_mutating_pure_serializes_through_dispatch_state_machine() {
    use tau_proto::ToolSideEffects::{Mutating, Pure};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.selected_model = Some("test/model".into());
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
                display: None,
            },
            AgentToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: write_args,
                display: None,
            },
            AgentToolCall {
                id: "c3".into(),
                name: "read".into(),
                arguments: read_args,
                display: None,
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
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
    h.selected_model = Some("test/model".into());

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
                display: None,
            },
            AgentToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: write_args("b.txt"),
                display: None,
            },
            AgentToolCall {
                id: "c3".into(),
                name: "write".into(),
                arguments: write_args("c.txt"),
                display: None,
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
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
    h.selected_model = Some("test/model".into());

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
            display: None,
        }],
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

#[test]
fn linear_session_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("hi".to_owned()),
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
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.model_params, prompt1.model_params);
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

/// When the agent reports a `response_id` on a finished turn, the
/// next `SessionPromptCreated` for that conversation must carry a
/// `previous_response` pointing back at it — that's the hook the
/// Responses backend uses to switch into stateful-chain mode and
/// send just the delta upstream. `message_index` must equal the
/// assembled message count at the moment the anchor was captured,
/// so the delta slice is exactly the messages added since.
#[test]
fn response_id_anchors_next_prompt_with_previous_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("first answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        response_id: Some("resp_abc".to_owned()),
        phase: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    let prev = prompt2.previous_response.expect("chain anchor on prompt 2");
    assert_eq!(prev.id, "resp_abc");
    // After turn 1 finished and was folded, the assembled count is:
    //   user "first" + assistant "first answer" = 2 messages.
    // That's the slice point — `messages[2..]` on prompt 2 is just
    // the new "second" user turn (1 message).
    assert_eq!(prev.message_index, prompt1.messages.len() + 1);
    assert_eq!(prev.message_index + 1, prompt2.messages.len());

    h.shutdown().expect("shutdown");
}

/// Switching `selected_model` mid-conversation must bust the chain.
/// The prior response was produced by a different model — its
/// stored state on the upstream API is meaningless for the new
/// model, and sending `previous_response_id` would either error or
/// silently mix incompatible reasoning.
#[test]
fn model_switch_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model-a".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("first answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        response_id: Some("resp_abc".to_owned()),
        phase: None,
    })
    .expect("finish first");

    // User switches models.
    h.selected_model = Some("test/model-b".into());

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response.is_none(),
        "model switch must clear the previous-response anchor"
    );

    h.shutdown().expect("shutdown");
}

/// A turn that didn't yield a `response_id` (Chat Completions
/// backend, an error, etc.) must NOT anchor a chain. The next prompt
/// has to be a full replay — pretending we have a chain we don't
/// would make the upstream API reject the next call.
#[test]
fn missing_response_id_leaves_chain_unset() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let spid1: SessionPromptId = "sp-0".into();

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("first answer".to_owned()),
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
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let spid2: SessionPromptId = "sp-1".into();
    let prompt2 = read_prompt_created(&h, &spid2);

    assert!(
        prompt2.previous_response.is_none(),
        "no response_id on the prior turn means no chain"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_prompt_extends_completed_first_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

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
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
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
    h.selected_model = Some("test/model".into());

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

/// While a parent's `delegate` tool call is in flight, the harness
/// must still dispatch the spawned side conversation's prompt
/// immediately — the parent's `ToolsRunning` turn state is logically
/// independent from the side conv's own turn. The two failure modes
/// this test pins down: (1) the side prompt gets queued behind the
/// parent's pending tool result and never goes out (deadlock), and
/// (2) the parent's `ToolsRunning` state gets clobbered when the
/// side conv finishes, leaving the parent unable to receive its
/// `ToolResult`. Uses the real delegate shape (`tool_call_id: Some`).
#[test]
fn ext_agent_query_dispatches_while_tool_is_running_and_restores_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
            display: None,
        }],
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
            tool_call_id: Some("delegate-call".into()),
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
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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

/// A tool-backed `ExtAgentQuery` (`tool_call_id: Some(...)`) is the
/// `delegate` path: it dispatches *while the parent's tool call is
/// still in flight*, so the parent conv's tip is a `ToolUse` block
/// with no matching `ToolResult` yet. The side conv must therefore
/// fork off the tree root with `head: None`, NOT inherit the
/// parent's branch — otherwise (a) the assembled prompt would carry
/// an orphan `ToolUse` block (provider 400s on unmatched tool_use),
/// and (b) the sub-agent would see the user's framing and might
/// recursively re-delegate the same task. (Contrast with the
/// non-tool path, where `tool_call_id: None` deliberately inherits
/// the parent — see `non_tool_ext_agent_query_inherits_parent_branch`.)
#[test]
fn ext_agent_query_during_tool_call_branches_off_unresolved_tool_use() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
            display: None,
        }],
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
    .expect("tool response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: Some("delegate-call".into()),
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

    // Tool-backed sub-agents (`tool_call_id: Some(...)`) get a fresh
    // context regardless of whether the parent is mid-tool-call: they
    // see only their own `query.instruction`, never the parent's
    // unresolved `delegate` tool_use (which would be an orphan ToolUse
    // the provider rejects), and never the user's task framing (which
    // would invite recursive re-delegation).
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

/// A non-tool `ExtAgentQuery` (`tool_call_id: None`, e.g.
/// `std-notifications`' idle summary) is **not** a delegate. Its
/// purpose is to summarize what the user just did, so the side conv
/// must inherit the parent conversation's branch — assembling the
/// user's recent UserMessage / AgentMessage history *plus* the new
/// instruction. The whole feature falls back to a useless generic
/// greeting if the model is asked to summarize an empty transcript.
///
/// This is also why we don't strip tools / system prompt for these
/// queries: the side conv's request reuses the parent's cached prefix
/// (system_prompt + tools + full transcript) and adds only the
/// instruction as a delta. Verified here by comparing the assembled
/// prompt to what the parent conv sees.
#[test]
fn non_tool_ext_agent_query_inherits_parent_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());

    // Drive the user's main conversation through one full
    // user-message → agent-final-response turn so the parent conv has
    // a non-empty history when the idle summary fires.
    let cid = h.default_conversation_id.clone();
    let main_spid: SessionPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_conversations
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "find the bug in foo.rs".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: main_spid,
        text: Some("I fixed the off-by-one in foo.rs".to_owned()),
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
    .expect("main response");
    let parent_head_before = h.conversations.get(&cid).expect("default conv").head;
    assert!(
        parent_head_before.is_some(),
        "parent conv should have advanced its head after the agent's reply",
    );

    // std-notifications-shaped query: no tool_call_id, just an
    // instruction asking the model to summarize.
    h.handle_ext_agent_query(
        "conn-notifications",
        ExtAgentQuery {
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("ext query");

    let side_spid = h
        .prompt_conversations
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    // The assembled side prompt must contain the user's original
    // task, the agent's final answer, AND the new instruction — in
    // that order. Without inheritance the side prompt would only
    // hold the instruction and the model would default to a generic
    // "I'm ready for your next task" reply.
    let user_task_present = side_prompt.messages.iter().any(|m| {
        matches!(m.role, tau_proto::ConversationRole::User) && m.content.iter().any(|b| {
            matches!(
                b,
                tau_proto::ContentBlock::Text { text } if text.contains("find the bug in foo.rs")
            )
        })
    });
    let agent_answer_present = side_prompt.messages.iter().any(|m| {
        matches!(m.role, tau_proto::ConversationRole::Assistant) && m.content.iter().any(|b| {
            matches!(
                b,
                tau_proto::ContentBlock::Text { text } if text.contains("I fixed the off-by-one")
            )
        })
    });
    let instruction_present = side_prompt.messages.iter().any(|m| {
        matches!(m.role, tau_proto::ConversationRole::User)
            && m.content.iter().any(|b| {
                matches!(
                    b,
                    tau_proto::ContentBlock::Text { text } if text == "Summarize in one sentence."
                )
            })
    });
    assert!(
        user_task_present,
        "side prompt must inherit the user's original task message: {:?}",
        side_prompt.messages,
    );
    assert!(
        agent_answer_present,
        "side prompt must inherit the agent's final reply: {:?}",
        side_prompt.messages,
    );
    assert!(
        instruction_present,
        "side prompt must contain the summarize-instruction itself: {:?}",
        side_prompt.messages,
    );

    // Tool calls are disabled on this turn so the summarizer can't
    // accidentally fire `write` / `edit` / `delegate` — and the
    // tools list still goes out unchanged so the cache prefix
    // matches the parent conv's.
    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::None,
        "non-tool ext-agent query must disable tool calls for the side conv's turn",
    );

    // The parent conv's head must not have moved sideways because of
    // the side conv's publish — both convs are now downstream of the
    // parent's previous tip, but the side conv folded onto its own
    // child node.
    let parent_head_after = h.conversations.get(&cid).expect("default conv").head;
    assert_eq!(
        parent_head_before, parent_head_after,
        "side conv's UserMessage must not advance the parent conv's head",
    );

    h.shutdown().expect("shutdown");
}

/// Counterpart to `non_tool_ext_agent_query_inherits_parent_branch`.
/// The harness picks `tool_choice` per conversation in
/// `send_prompt_to_agent_for`; if that discriminator ever
/// over-matches (e.g. flips on `originator.is_extension()` alone),
/// delegate sub-agents would receive `tool_choice: "none"` and be
/// unable to call any tool — silently turning every delegated task
/// into a one-shot text response. Asserts the inverse leg: when
/// `tool_call_id: Some(...)`, `ToolChoice::Auto` is preserved.
#[test]
fn delegate_ext_agent_query_keeps_tool_choice_auto() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
        tool_calls: vec![AgentToolCall {
            id: "delegate-call".into(),
            name: "delegate".into(),
            arguments: CborValue::Map(Vec::new()),
            display: None,
        }],
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
    .expect("main response");

    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: Some("delegate-call".into()),
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
    assert_eq!(
        prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "delegated sub-agent must keep tool access (ToolChoice::Auto)",
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

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
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
            display: None,
        }],
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
    .expect("main response");

    // Delegate extension turns it into an ExtAgentQuery; the harness
    // spawns a side conversation and dispatches its prompt.
    h.handle_ext_agent_query(
        "conn-delegate",
        ExtAgentQuery {
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            tool_call_id: Some("delegate-call".into()),
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
                display: None,
            },
            AgentToolCall {
                id: "ro-2".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: read_only_args.clone(),
                display: None,
            },
        ],
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
    h2.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h2, "conn-delegate");
    h2.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
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
                display: None,
            },
            AgentToolCall {
                id: "mut-2".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
                display: None,
            },
        ],
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

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("delegate"),
            description: None,
            parameters: None,
            side_effects: ToolSideEffects::Mutating,
        },
    );
    let _websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
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
            display: None,
        }],
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
            display: None,
        }],
        input_tokens: Some(1234),
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q1".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
            tool_name: tau_proto::ToolName::new("websearch"),
            result: CborValue::Text("fake result".to_owned()),
            display: None,
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
            display: None,
        }],
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-nested".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
    })
    .expect("nested final");

    // The delegate extension would route the nested ExtAgentQueryResult
    // back as a ToolResult — simulate that here.
    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "nested-call".into(),
            tool_name: tau_proto::ToolName::new("delegate"),
            result: CborValue::Text("nested answer".to_owned()),
            display: None,
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
            display: None,
        }],
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
            display: None,
        }],
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
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-outer".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
    })
    .expect("side final");

    h.handle_extension_event(
        "conn-delegate",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "outer-call".into(),
            tool_name: tau_proto::ToolName::new("delegate"),
            result: CborValue::Text("outer answer".to_owned()),
            display: None,
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
            display: None,
        }],
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-top".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
                display: None,
            },
            AgentToolCall {
                id: "main-B".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Map(Vec::new()),
                display: None,
            },
        ],
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-A".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
            display: None,
        }],
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
            display: None,
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-delegate".into(),
            query_id: "q-sub".to_owned(),
        },

        backend: None,
        response_id: None,
        phase: None,
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
