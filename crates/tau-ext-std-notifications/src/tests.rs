use std::io::Cursor;
use std::sync::Once;

use tau_proto::{AgentResponseFinished, Event, FrameReader, FrameWriter, UiPromptSubmitted};
use tracing_subscriber::EnvFilter;

use super::*;

fn message_variant(msg: &Message) -> &'static str {
    match msg {
        Message::Hello(_) => "Hello",
        Message::Subscribe(_) => "Subscribe",
        Message::Intercept(_) => "Intercept",
        Message::Ready(_) => "Ready",
        Message::Disconnect(_) => "Disconnect",
        Message::Configure(_) => "Configure",
        Message::ConfigError(_) => "ConfigError",
        Message::Emit(_) => "Emit",
        Message::InterceptRequest(_) => "InterceptRequest",
        Message::InterceptReply(_) => "InterceptReply",
        Message::LogEvent(_) => "LogEvent",
        Message::Ack(_) => "Ack",
    }
}

/// Install a `tracing` subscriber for tests. Pick up `TAU_LOG` (same
/// env var the extension uses in production); default to off so a
/// plain `cargo test` is silent. Run a hanging test like
/// `TAU_LOG=trace cargo test -p tau-ext-std-notifications $name -- --nocapture`
/// to see every frame the extension received and every event the
/// test side read or skipped.
fn init_test_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = EnvFilter::try_from_env("TAU_LOG").unwrap_or_else(|_| EnvFilter::new("off"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .with_target(true)
            .try_init();
    });
}

/// Test-side wrapper around [`FrameReader`] that exposes an `Event`-flavoured
/// API (peels `LogEvent`, drops other messages).
struct EventReader<R> {
    inner: FrameReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        init_test_tracing();
        Self {
            inner: FrameReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_frame()? {
                None => {
                    tracing::trace!(target: "tau::test", "EventReader: end of stream");
                    return Ok(None);
                }
                Some(frame) => match frame.peel_log().1 {
                    Frame::Event(event) => {
                        tracing::trace!(target: "tau::test", name = %event.name(), "EventReader: event");
                        return Ok(Some(event));
                    }
                    Frame::Message(msg) => {
                        tracing::trace!(target: "tau::test", kind = message_variant(&msg), "EventReader: skipping message");
                        continue;
                    }
                },
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<Frame>, tau_proto::DecodeError> {
        self.inner.read_frame()
    }
}

/// Test-side wrapper around [`FrameWriter`] that accepts `Event` directly.
struct EventWriter<W> {
    inner: FrameWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: FrameWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_frame(&Frame::Event(event.clone()))
    }

    fn write_frame(&mut self, frame: &Frame) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_frame(frame)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Build a `Frame::Message(Disconnect)` for tests that previously sent
/// `Event::LifecycleDisconnect`.
fn disconnect_frame(reason: Option<String>) -> Frame {
    Frame::Message(Message::Disconnect(tau_proto::Disconnect { reason }))
}

/// Build a `Frame::Message(Configure)` for tests that previously sent
/// `Event::LifecycleConfigure`.
fn configure_frame(config: tau_proto::CborValue) -> Frame {
    Frame::Message(Message::Configure(tau_proto::Configure { config }))
}

/// Test marker for "we're past the lifecycle handshake". The hello /
/// subscribe / ready messages are point-to-point `Frame::Message`s
/// (filtered out by `EventReader`), so reading from `EventReader`
/// after this returns will block until the extension emits an
/// actual `Event`. Calling this is therefore a no-op — but if a
/// test ever blocks here suspiciously, set `TAU_LOG=trace` and run
/// with `--nocapture` to see what `EventReader` is skipping vs.
/// surfacing.
fn drain_lifecycle<R: std::io::Read>(_reader: &mut EventReader<R>) {}

#[test]
fn emits_start_and_end_user_var_in_order() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    // Explicit disconnect so the loop exits without waiting on
    // the (otherwise long) idle deadline triggered by the
    // `AgentResponseFinished`.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let start = reader.read_event().expect("read").expect("start event");
    match start {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.name, SOUND_VAR_NAME);
            assert_eq!(osc.value, VALUE_AGENT_START);
        }
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }

    let end = reader.read_event().expect("read").expect("end event");
    match end {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.name, SOUND_VAR_NAME);
            assert_eq!(osc.value, VALUE_AGENT_END);
        }
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }
}

/// Mid-turn `AgentResponseFinished` events (those carrying
/// pending tool calls) must NOT trigger the end-of-turn sound.
/// The agent emits one of those per LLM call when it's looping
/// through tool use; the *turn* only ends with a final
/// `AgentResponseFinished` that has empty `tool_calls`.
#[test]
fn mid_turn_finish_with_tool_calls_does_not_emit_end_sound() {
    use tau_proto::{AgentToolCall, CborValue, ToolNameMaybe};
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");
    // Mid-turn finish: text=None, tool_calls non-empty. No
    // notification should fire.
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: None,
            tool_calls: vec![AgentToolCall {
                id: "call-1".into(),
                name: ToolNameMaybe::from_raw("shell"),
                arguments: CborValue::Null,
                display: None,
            }],
            input_tokens: None,
            cached_tokens: None,
            thinking: Some("planning".into()),
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // We expect the user-submit sound but NO end sound, because
    // the tool-bearing AgentResponseFinished is mid-turn.
    let start = reader.read_event().expect("read").expect("start");
    match start {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.value, VALUE_AGENT_START);
        }
        other => panic!("expected start OSC, got {other:?}"),
    }
    let next = reader.read_event().expect("read");
    assert!(
        next.is_none(),
        "no further OSC events expected after mid-turn finish, got {next:?}",
    );
}

/// After AgentResponseFinished we should see the end-sound OSC
/// and then, after the configured idle window expires with no
/// further input, the text-notification OSC carrying a JSON
/// payload that mirrors `user-text-notification.sh`.
/// Idle window elapsing must trigger an `ExtAgentQuery` to the
/// agent for a one-sentence summary. When no result arrives
/// within the summary timeout, the extension falls back to the
/// static [`FALLBACK_BODY`] so the user still gets nudged.
#[test]
fn idle_timeout_requests_summary_then_falls_back() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle_and_summary_timeout(
        Cursor::new(input),
        &mut output,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // First the end-of-turn sound.
    let end = reader.read_event().expect("read").expect("end event");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected end sound OSC");
    };
    assert_eq!(osc.name, SOUND_VAR_NAME);
    assert_eq!(osc.value, VALUE_AGENT_END);

    // Then, after the (short) idle window, the side-query for a
    // summary.
    let query = reader.read_event().expect("read").expect("ext-query event");
    let Event::ExtAgentQuery(query) = query else {
        panic!("expected ExtAgentQuery, got {query:?}");
    };
    assert!(
        !query.query_id.is_empty(),
        "extension must mint a non-empty query_id",
    );
    assert!(query.instruction.contains("summarize") || query.instruction.contains("Summarize"));

    // Then, after the (short) summary timeout with no response,
    // the static fallback text notification.
    let fallback = reader.read_event().expect("read").expect("fallback event");
    let Event::Osc1337SetUserVar(osc) = fallback else {
        panic!("expected fallback OSC, got {fallback:?}");
    };
    assert_eq!(osc.name, TEXT_VAR_NAME);
    let payload: serde_json::Value =
        serde_json::from_str(&osc.value).expect("fallback payload is JSON");
    assert_eq!(payload["urgency"], "normal");
    assert_eq!(
        payload["title"]
            .as_str()
            .expect("title is a string")
            .starts_with("Agent idle: "),
        true,
        "title should start with `Agent idle: `, got {:?}",
        payload["title"],
    );
    assert_eq!(payload["body"], FALLBACK_BODY);
    assert_eq!(payload["app_name"], NOTIFY_APP_NAME);
}

/// When a matching `ExtAgentQueryResult` arrives before the
/// summary timeout, the text notification's body must be the
/// agent's summary text rather than the static fallback.
///
/// Coordinates with the running extension via a UnixStream pair:
/// the test thread reads each emitted event and only writes the
/// `ExtAgentQueryResult` *after* observing the `ExtAgentQuery`,
/// so the result lands while the extension is in the
/// `WaitingSummary` state (not the earlier `WaitingIdle`).
#[test]
fn summary_result_populates_notification_body() {
    use std::os::unix::net::UnixStream;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    // end-of-turn sound, then the side-query.
    let _end = reader.read_event().expect("read").expect("end");
    let query = reader.read_event().expect("read").expect("query");
    let Event::ExtAgentQuery(query) = query else {
        panic!("expected ExtAgentQuery, got {query:?}");
    };

    writer
        .write_event(&Event::ExtAgentQueryResult(
            tau_proto::ExtAgentQueryResult {
                query_id: query.query_id.clone(),
                text: "  refactoring the harness state, awaiting next prompt  ".into(),
                error: None,
            },
        ))
        .expect("write");
    writer.flush().expect("flush");

    let text = reader.read_event().expect("read").expect("text");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected populated text OSC, got {text:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
    assert_eq!(
        payload["body"], "refactoring the harness state, awaiting next prompt",
        "summary body should be trimmed",
    );

    // Cleanly disconnect so the extension exits.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// Trailing-edge typing pings (`UiPromptDraft`) arriving during
/// the `WaitingIdle` window must extend the deadline so the
/// idle notification doesn't fire while the user is still
/// composing. Without this, a slow typer would get the
/// "what were you working on?" notification mid-sentence.
#[test]
fn prompt_draft_extends_idle_deadline() {
    use std::os::unix::net::UnixStream;

    use tau_proto::UiPromptDraft;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    // Arm the idle deadline.
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    // end-of-turn sound.
    let _end = reader.read_event().expect("read").expect("end");

    // Send several drafts ~100ms apart. Each one resets the
    // 200ms idle deadline; if the extension honors them
    // correctly no `ExtAgentQuery` should fire during this
    // window.
    for i in 0..5 {
        writer
            .write_event(&Event::UiPromptDraft(UiPromptDraft {
                session_id: "s1".into(),
                text: format!("partial draft {i}"),
            }))
            .expect("write");
        writer.flush().expect("flush");
        thread::sleep(Duration::from_millis(100));
    }

    // Stop typing. The next event the extension emits must be
    // the side-query — and crucially, the elapsed time before
    // it fires must be >= the original 200ms (because we kept
    // resetting the deadline) plus the final ~200ms wait.
    let started = Instant::now();
    let query = reader.read_event().expect("read").expect("query");
    let elapsed = started.elapsed();
    let Event::ExtAgentQuery(_) = query else {
        panic!("expected ExtAgentQuery, got {query:?}");
    };
    // Without the deadline reset, the query would have fired
    // at idle_duration (200ms) into the typing window — i.e.
    // ~300ms before we started timing — so the read here would
    // return ~immediately. With the reset, the most recent
    // draft (sent ~100ms ago) bumped the deadline ~200ms into
    // the future, so the read should block for roughly 100ms.
    // 30ms is a deliberately loose lower bound so CI jitter
    // doesn't flake the test.
    assert!(
        elapsed >= Duration::from_millis(30),
        "ExtAgentQuery fired too soon ({elapsed:?}); idle deadline wasn't reset",
    );

    // Disconnect to let the extension exit.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// `UiPromptDraft` arriving while a side-query summary is
/// already in flight must NOT cancel it (we don't yet have
/// prompt cancellation). The summary completes normally and
/// surfaces as the notification body.
#[test]
fn prompt_draft_during_waiting_summary_does_not_cancel() {
    use std::os::unix::net::UnixStream;

    use tau_proto::UiPromptDraft;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let _end = reader.read_event().expect("read").expect("end");
    let query = reader.read_event().expect("read").expect("query");
    let Event::ExtAgentQuery(query) = query else {
        panic!("expected ExtAgentQuery, got {query:?}");
    };

    // User starts typing AFTER we've dispatched the side query.
    // The summary must still be allowed to land.
    writer
        .write_event(&Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1".into(),
            text: "typing while summary is in flight".into(),
        }))
        .expect("write");
    writer.flush().expect("flush");

    // Now deliver the summary result.
    writer
        .write_event(&Event::ExtAgentQueryResult(
            tau_proto::ExtAgentQueryResult {
                query_id: query.query_id,
                text: "the model's summary".into(),
                error: None,
            },
        ))
        .expect("write");
    writer.flush().expect("flush");

    // Notification must use the summary body, not be cancelled.
    let text = reader.read_event().expect("read").expect("text");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected populated text OSC, got {text:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
    assert_eq!(payload["body"], "the model's summary");

    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// When `idle_command` is configured, it must run alongside the
/// OSC notification, receive the title as `argv[1]`, the body
/// on stdin, and the `NOTIFY_*` env vars set. Uses a tiny shell
/// command that writes its argv + env + stdin into a temp file
/// the test reads back.
#[test]
fn idle_command_runs_with_title_body_and_env() {
    use std::os::unix::net::UnixStream;

    use tempfile::TempDir;

    let td = TempDir::new().expect("tempdir");
    let out_path = td.path().join("out.txt");

    // bash one-liner: writes title (arg 1) + env vars + stdin
    // into the output file, separated by `|||` so the test can
    // assert each piece.
    let cmd = format!(
        "printf '%s|||%s|||%s|||' \"$1\" \"$NOTIFY_URGENCY\" \"$NOTIFY_APP_NAME\" >> {dest}; \
             cat >> {dest}",
        dest = out_path.display(),
    );

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    // Configure the extension with the test command.
    let cfg = tau_proto::json_to_cbor(&serde_json::json!({
        "idle_seconds": 0,
        "idle_command": ["bash", "-c", cmd, "_marker"],
    }));
    writer.write_frame(&configure_frame(cfg)).expect("write");
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    // Drain: end-sound, ExtAgentQuery, fallback OSC. We don't
    // care about the exact contents — what we want is the
    // command to run as a side effect.
    let _ = reader.read_event().expect("read").expect("end");
    let _ = reader.read_event().expect("read").expect("query");
    let _ = reader.read_event().expect("read").expect("fallback");

    // The command runs in a detached thread; poll the output
    // file briefly until it appears (max 2s).
    let started = Instant::now();
    loop {
        if out_path.exists()
            && let Ok(contents) = std::fs::read_to_string(&out_path)
            && contents.contains("|||")
        {
            let mut parts = contents.splitn(4, "|||");
            let title = parts.next().expect("title field");
            let urgency = parts.next().expect("urgency field");
            let app_name = parts.next().expect("app_name field");
            let body = parts.next().expect("body field");
            assert!(title.starts_with("Agent idle: "), "title arg {title:?}",);
            assert_eq!(urgency, "normal");
            assert_eq!(app_name, NOTIFY_APP_NAME);
            assert_eq!(body, FALLBACK_BODY);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "idle_command never produced output",
        );
        thread::sleep(Duration::from_millis(20));
    }

    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// A bogus `config` value (one that doesn't match `ExtConfig`)
/// must trigger a `LifecycleConfigError` carrying a human-readable
/// message, so the harness can surface it to the user.
#[test]
fn invalid_config_emits_lifecycle_config_error() {
    // Build a config CBOR value that doesn't match ExtConfig:
    // an unknown field, which `deny_unknown_fields` rejects.
    let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
        "totally_unknown_field": 7,
    }));

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(bad_config))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    // Skip startup messages (hello, subscribe, ready) until we reach
    // the ConfigError reply.
    let err_frame = loop {
        let frame = reader
            .read_frame()
            .expect("read")
            .expect("config error frame");
        if matches!(frame, Frame::Message(Message::ConfigError(_))) {
            break frame;
        }
    };
    let Frame::Message(Message::ConfigError(e)) = err_frame else {
        unreachable!()
    };
    assert!(!e.message.is_empty(), "config error must carry a message");
}

/// A user prompt arriving inside the idle window must cancel the
/// pending text notification — only the end-sound OSC should be
/// emitted before stdin closes.
#[test]
fn user_prompt_during_idle_window_cancels_text_notification() {
    use tau_proto::UiPromptSubmitted;

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "another question".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    // Long idle window — if the cancel works, we never wait.
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let end = reader.read_event().expect("read").expect("end event");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected end sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    // The follow-up user prompt should emit the user-submit
    // sound and cancel the idle deadline.
    let next = reader
        .read_event()
        .expect("read")
        .expect("user-submit event");
    let Event::Osc1337SetUserVar(osc) = next else {
        panic!("expected user-submit sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    assert!(reader.read_event().expect("read eof").is_none());
}

/// Sub-agent (`PromptOriginator::Extension`) prompt + response
/// activity must not perturb the notifications extension. A
/// `delegate` flow runs an entire side conversation between the
/// user's prompt and the main agent's final response — none of those
/// side events should clear the idle timer or fire the end-of-turn
/// chime, since the user isn't seeing them.
#[test]
fn sub_agent_prompts_and_responses_are_ignored() {
    use tau_proto::{AgentPromptSubmitted, AgentToolCall, CborValue, ToolNameMaybe};
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);

    // User starts a turn → expect agent_start sound.
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");

    // Main agent emits a delegate tool_call (mid-turn).
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-main".into(),
            text: None,
            tool_calls: vec![AgentToolCall {
                id: "delegate-call".into(),
                name: ToolNameMaybe::from_raw("delegate"),
                arguments: CborValue::Null,
                display: None,
            }],
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");

    // Sub-agent activity — must not clear idle, fire chimes, or
    // touch `waiting_for_final_response`.
    writer
        .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            session_prompt_id: "sp-side".into(),
            originator: tau_proto::PromptOriginator::Extension {
                name: "core-delegate".into(),
                query_id: "q1".into(),
            },
        }))
        .expect("write");
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "side instruction".into(),
            originator: tau_proto::PromptOriginator::Extension {
                name: "core-delegate".into(),
                query_id: "q1".into(),
            },
            ctx_id: None,
        }))
        .expect("write");
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-side".into(),
            text: Some("delegated answer".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::Extension {
                name: "core-delegate".into(),
                query_id: "q1".into(),
            },

            backend: None,
        }))
        .expect("write");

    // Main agent finally finishes the user's turn → end sound.
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-main".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // Expect exactly two OSC events: agent_start (user prompt) and
    // agent_end (main agent's final response). Sub-agent activity
    // between them must NOT produce any sounds.
    let start = reader.read_event().expect("read").expect("start");
    let Event::Osc1337SetUserVar(osc) = start else {
        panic!("expected agent_start OSC, got {start:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    let end = reader.read_event().expect("read").expect("end");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected agent_end OSC, got {end:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    assert!(
        reader.read_event().expect("read eof").is_none(),
        "no further OSC events expected — sub-agent activity must be silent",
    );
}

#[test]
fn duplicate_ui_prompt_submitted_during_same_turn_emits_one_start_sound() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");
    writer
        .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "internal replay".into(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }))
        .expect("write");
    writer
        .write_event(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("done".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
        }))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let first = reader.read_event().expect("read").expect("first OSC");
    let Event::Osc1337SetUserVar(osc) = first else {
        panic!("expected first sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    let second = reader.read_event().expect("read").expect("second OSC");
    let Event::Osc1337SetUserVar(osc) = second else {
        panic!("expected second sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    assert!(reader.read_event().expect("read eof").is_none());
}
