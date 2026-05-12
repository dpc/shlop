use super::*;

fn representative_events() -> Vec<Event> {
    vec![
        Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("echo"),
                description: Some("Echo a payload".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        }),
        Event::ToolRequest(ToolRequest {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            originator: PromptOriginator::User,
        }),
        Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            originator: PromptOriginator::User,
        }),
        Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            result: CborValue::Text("hello".to_owned()),
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            message: "no live provider".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: Some(ProgressUpdate {
                current: Some(1),
                total: Some(10),
            }),
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".to_owned(),
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: "You are helpful.".to_owned(),
            messages: vec![ConversationMessage {
                role: ConversationRole::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
                phase: None,
            }],
            tools: vec![ToolDefinition {
                name: ToolName::new("read"),
                description: Some("Read a file".to_owned()),
                parameters: None,
            }],
            model: None,
            model_params: ModelParams::default(),
            tool_choice: ToolChoice::default(),
            originator: PromptOriginator::User,
            ctx_id: None,
            previous_response: None,
        }),
        Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("Hi there".to_owned()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            token_usage: None,
            originator: PromptOriginator::User,

            backend: None,
            response_id: None,
            phase: None,
        }),
        Event::ExtensionStarting(ExtensionStarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionReady(ExtensionReady {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionExited(ExtensionExited {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            exit_code: Some(0),
            signal: None,
        }),
        Event::ExtensionRestarting(ExtensionRestarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            attempt: 2,
            reason: Some("hot reload".to_owned()),
        }),
        Event::ExtSkillAvailable(ExtSkillAvailable {
            name: "brave-search".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
            add_to_prompt: true,
        }),
        Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
            file_path: "/home/user/src/project/AGENTS.md".into(),
            content: "# Project instructions\n- Run tests".to_owned(),
        }),
        Event::ExtensionContextReady(ExtensionContextReady {
            session_id: "s1".into(),
        }),
        Event::ExtensionEvent(CustomEvent {
            name: "demo.progress".parse().expect("event name"),
            session_id: Some("s1".into()),
            payload: CborValue::Text("working".to_owned()),
        }),
        Event::HarnessVerbosityChanged(HarnessVerbosityChanged {
            level: Verbosity::Low,
        }),
        Event::HarnessVerbositiesAvailable(HarnessVerbositiesAvailable {
            levels: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
        }),
        Event::HarnessThinkingSummaryChanged(HarnessThinkingSummaryChanged {
            level: ThinkingSummary::Concise,
        }),
        Event::HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable {
            levels: vec![
                ThinkingSummary::Off,
                ThinkingSummary::Auto,
                ThinkingSummary::Concise,
                ThinkingSummary::Detailed,
            ],
        }),
        Event::UiSetVerbosity(UiSetVerbosity {
            level: Verbosity::High,
        }),
        Event::UiSetThinkingSummary(UiSetThinkingSummary {
            level: ThinkingSummary::Auto,
        }),
    ]
}

fn representative_messages() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "agent".into(),
            client_kind: ClientKind::Agent,
        }),
        Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
        }),
        Message::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority::new(0),
        }),
        Message::Ready(Ready {
            message: Some("ready".to_owned()),
        }),
        Message::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        Message::Configure(Configure {
            config: CborValue::Null,
        }),
        Message::ConfigError(ConfigError {
            message: "bad config".to_owned(),
        }),
        Message::Emit(Emit {
            event: Box::new(Event::ExtensionEvent(CustomEvent {
                name: "demo.transient_progress".parse().expect("event name"),
                session_id: Some("s1".into()),
                payload: CborValue::Text("working".to_owned()),
            })),
            transient: true,
        }),
        Message::InterceptRequest(InterceptRequest {
            event: Box::new(Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: SessionStartReason::Initial,
            })),
            transient: false,
        }),
        Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        }),
        Message::LogEvent(LogEvent {
            id: LogEventId::new(42),
            recorded_at: UnixMicros::new(1_700_000_000_000_000),
            event: Box::new(Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: SessionStartReason::Initial,
            })),
        }),
        Message::Ack(Ack {
            up_to: LogEventId::new(42),
        }),
    ]
}

fn representative_frames() -> Vec<Frame> {
    let mut out: Vec<Frame> = representative_events()
        .into_iter()
        .map(Frame::Event)
        .collect();
    out.extend(representative_messages().into_iter().map(Frame::Message));
    out
}

#[test]
fn event_name_round_trips_from_string() {
    for event in representative_events() {
        let name = event.name();
        let serialized = name.to_string();
        assert_eq!(serialized.parse::<EventName>(), Ok(name));
    }
}

#[test]
fn representative_frames_round_trip_through_cbor() {
    for frame in representative_frames() {
        let encoded = encode_frame_to_vec(&frame).expect("frame should encode");
        let decoded = decode_frame_from_slice(&encoded).expect("frame should decode");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn multiple_frames_can_share_one_stream() {
    let frames = representative_frames();
    let mut writer = FrameWriter::new(Vec::new());
    for frame in &frames {
        writer.write_frame(frame).expect("frame should encode");
    }
    writer.flush().expect("stream should flush");

    let bytes = writer.into_inner();
    let mut reader = FrameReader::new(std::io::Cursor::new(bytes));
    let mut decoded = Vec::new();
    for _ in 0..frames.len() {
        decoded.push(
            reader
                .read_frame()
                .expect("read should succeed")
                .expect("frame should arrive"),
        );
    }

    assert_eq!(decoded, frames);
}

#[test]
fn message_wire_form_uses_flat_message_tag() {
    let msg = Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "agent".into(),
        client_kind: ClientKind::Agent,
    });
    let json = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(json["message"], "hello");
    assert!(json.get("payload").is_some());
}

#[test]
fn event_wire_form_uses_dotted_event_tag() {
    let event = Event::ToolInvoke(ToolInvoke {
        call_id: "call-1".into(),
        tool_name: ToolName::new("echo"),
        arguments: CborValue::Text("hi".to_owned()),
        originator: PromptOriginator::User,
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "tool.invoke");
    assert!(json.get("payload").is_some());
}

#[test]
fn tool_name_accepts_valid_names() {
    assert!(ToolName::try_new("read").is_some());
    assert!(ToolName::try_new("shell").is_some());
    assert!(ToolName::try_new("my_tool_2").is_some());
    assert!(ToolName::try_new("Echo").is_some());
}

#[test]
fn tool_name_rejects_invalid_names() {
    assert!(ToolName::try_new("").is_none());
    assert!(ToolName::try_new("fs.read").is_none());
    assert!(ToolName::try_new("my tool").is_none());
    assert!(ToolName::try_new("a-b").is_none());
    assert!(ToolName::try_new("tool/name").is_none());
}

#[test]
#[should_panic(expected = "invalid tool name")]
fn tool_name_new_panics_on_invalid() {
    let _ = ToolName::new("bad.name");
}

#[test]
fn tool_name_maybe_classifies_inputs() {
    assert!(matches!(
        ToolNameMaybe::from("read"),
        ToolNameMaybe::Valid(_)
    ));
    assert!(matches!(
        ToolNameMaybe::from(""),
        ToolNameMaybe::Invalid(ref s) if s.is_empty()
    ));
    assert!(matches!(
        ToolNameMaybe::from("fs.read"),
        ToolNameMaybe::Invalid(ref s) if s == "fs.read"
    ));
}

#[test]
fn tool_name_rejects_overlong_input() {
    // ASCII alphanumerics that exceed the cap must be rejected even
    // though they pass the character-class check.
    let long = "a".repeat(ToolName::MAX_LEN + 1);
    assert!(ToolName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolName::MAX_LEN);
    assert!(ToolName::try_new(at_cap).is_some());
}

#[test]
fn frame_peel_log_extracts_log_event_id_and_inner_event() {
    let inner = Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    });
    let frame = Frame::Message(Message::LogEvent(LogEvent {
        id: LogEventId::new(7),
        recorded_at: UnixMicros::new(1_700_000_000_000_000),
        event: Box::new(inner.clone()),
    }));

    let (peeled_id, rest) = frame.peel_log();
    assert_eq!(peeled_id, Some(LogEventId::new(7)));
    assert_eq!(rest, Frame::Event(inner));
}

#[test]
fn frame_peel_log_passes_non_log_frames_through_unchanged() {
    // A bare event must not be mistaken for a log envelope, and the
    // returned frame must be byte-for-byte the same value the caller
    // handed in.
    let event = Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    });
    let original = Frame::Event(event);
    let (peeled_id, rest) = original.clone().peel_log();
    assert_eq!(peeled_id, None);
    assert_eq!(rest, original);

    // Likewise for a non-LogEvent message.
    let msg = Frame::Message(Message::Ready(Ready {
        message: Some("ready".to_owned()),
    }));
    let (peeled_id, rest) = msg.clone().peel_log();
    assert_eq!(peeled_id, None);
    assert_eq!(rest, msg);
}

#[test]
fn event_defaults_to_transient_marks_progress_kinds() {
    // The set named by `defaults_to_transient` is the contract the
    // harness relies on to decide which events skip the durable
    // session event log when an extension publishes them without
    // explicit transient metadata. Lock it down here so any future
    // edit to the matcher is intentional.
    let transient = [
        Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-1".into(),
            text: "partial".to_owned(),
            thinking: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: None,
        }),
        Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1".into(),
            text: "draft".to_owned(),
        }),
    ];
    for event in &transient {
        assert!(
            event.defaults_to_transient(),
            "{} should default to transient",
            event.name()
        );
    }

    let durable = [
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".to_owned(),
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
    ];
    for event in &durable {
        assert!(
            !event.defaults_to_transient(),
            "{} should be durable",
            event.name()
        );
    }
}

#[test]
fn tool_name_maybe_serializes_as_transparent_string() {
    // The wire format must be a plain string — same bytes as if
    // the field were declared `String`. That's what lets us
    // introduce `ToolNameMaybe` without a protocol bump.
    let valid = ToolNameMaybe::from("read");
    let invalid = ToolNameMaybe::from("bad.name");
    assert_eq!(
        serde_json::to_string(&valid).expect("serialize valid"),
        "\"read\""
    );
    assert_eq!(
        serde_json::to_string(&invalid).expect("serialize invalid"),
        "\"bad.name\""
    );

    // Round-trip via JSON picks the right variant.
    let reparsed: ToolNameMaybe = serde_json::from_str("\"read\"").expect("deserialize valid");
    assert!(matches!(reparsed, ToolNameMaybe::Valid(_)));
    let reparsed: ToolNameMaybe =
        serde_json::from_str("\"bad.name\"").expect("deserialize invalid");
    assert!(matches!(reparsed, ToolNameMaybe::Invalid(_)));
}

/// `Verbosity::next_in` mirrors `Effort::next_in`. Even though the CLI
/// doesn't bind a cycle key for verbosity today, the helper is part of
/// the public API and the protocol tests should pin the same wrap /
/// skip / empty-allowed-set behaviour effort relies on.
#[test]
fn verbosity_next_in_skips_disallowed_levels_and_wraps() {
    use Verbosity::*;
    let canonical = [Low, Medium, High];

    assert_eq!(Low.next_in(&canonical), Medium);
    assert_eq!(High.next_in(&canonical), Low);

    let only_low_high = [Low, High];
    assert_eq!(Low.next_in(&only_low_high), High);
    assert_eq!(High.next_in(&only_low_high), Low);

    let pinned = [Medium];
    assert_eq!(Low.next_in(&pinned), Medium);
    assert_eq!(Medium.next_in(&pinned), Medium);

    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// `ThinkingSummary` parses from / displays through the canonical
/// wire forms the slash command and `models.json5` rely on.
#[test]
fn thinking_summary_round_trips_through_display_and_from_str() {
    use ThinkingSummary::*;
    for level in [Off, Auto, Concise, Detailed] {
        let s = level.to_string();
        assert_eq!(s.parse::<ThinkingSummary>().ok(), Some(level));
    }
    assert!("bogus".parse::<ThinkingSummary>().is_err());
}

/// `ModelParams` serializes its bundled knobs as a flat object that
/// drops fields at their default value. Lets `harness.json5`
/// snapshots stay tiny and avoids surprising callers that introspect
/// the wire shape.
#[test]
fn model_params_serializes_skipping_defaults() {
    let json = serde_json::to_value(ModelParams::default()).expect("serialize");
    assert_eq!(json, serde_json::json!({}));

    let json = serde_json::to_value(ModelParams {
        effort: Effort::High,
        verbosity: Verbosity::Low,
        thinking_summary: ThinkingSummary::Concise,
    })
    .expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "effort": "high",
            "verbosity": "low",
            "thinking_summary": "concise",
        })
    );
}

/// `Effort::next_in` powers the Shift+Tab cycle on the CLI side: it
/// must skip levels that aren't in the harness's allowed set so the
/// cycle doesn't trap when (say) `xhigh` is missing for the current
/// model. Locking the behaviour with explicit cases so a future
/// refactor of the cycle helper can't silently regress the UX.
#[test]
fn effort_next_in_skips_disallowed_levels_and_wraps() {
    use Effort::*;
    let canonical = [Off, Minimal, Low, Medium, High];
    let with_xhigh = [Off, Minimal, Low, Medium, High, XHigh];

    // Without xhigh, High wraps back to Off — XHigh is skipped.
    assert_eq!(High.next_in(&canonical), Off);
    // With xhigh, High advances to XHigh and XHigh wraps to Off.
    assert_eq!(High.next_in(&with_xhigh), XHigh);
    assert_eq!(XHigh.next_in(&with_xhigh), Off);

    // Sparse allowed set (provider with no reasoning effort) — Off
    // is the only legal level, so any input lands there.
    let only_off = [Off];
    assert_eq!(High.next_in(&only_off), Off);
    assert_eq!(Off.next_in(&only_off), Off);

    // Empty allowed set falls through to plain `next()` so callers
    // that haven't received `HarnessEffortsAvailable` yet still
    // make progress on Shift+Tab presses.
    assert_eq!(Medium.next_in(&[]), Medium.next());
}
