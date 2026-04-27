//! Bridge agent prompt-start / response-finish events into iTerm2-style
//! OSC 1337 `SetUserVar` notifications, mirroring the dpc-personal
//! `notification-sounds.ts` and `user-text-notification.sh` Pi
//! extensions.
//!
//! Events emitted (all via `Osc1337SetUserVar`):
//! - `agent.prompt_submitted` → `user-notification = protoss-probe-ack`
//! - `agent.response_finished` → `user-notification = protoss-upgrade-complete`
//! - After `idle_seconds` (default 60) of inactivity following a response →
//!   `user-text-notification = {"urgency": "...", "title": "...", "body":
//!   "..."}`. The idle timer resets on every `ui.prompt_submitted` /
//!   `agent.prompt_submitted`. Tunable via the extension's
//!   `config.idle_seconds` field in `harness.json5`.
//!
//! The downstream tooling (typically a terminal multiplexer status
//! line or a `user-notification.sh` consumer wired to a sound file)
//! is what actually plays the sounds / pops the desktop notification;
//! this extension just publishes the user-var change so a UI further
//! up the stack can forward it to the terminal.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleConfigError,
    LifecycleHello, LifecycleReady, LifecycleSubscribe, Osc1337SetUserVar, PROTOCOL_VERSION,
};

/// `tracing` target for events emitted from this extension. Matches
/// the convention described in [`tau_extension`]: a short identifier
/// the user can name in `TAU_EXT_LOG=dpc_notifications=trace`.
pub const LOG_TARGET: &str = "dpc_notifications";

/// User-var name for sound notifications (matches `user-notification.sh`).
pub const SOUND_VAR_NAME: &str = "user-notification";

/// User-var name for text/desktop notifications (matches
/// `user-text-notification.sh`).
pub const TEXT_VAR_NAME: &str = "user-text-notification";

/// Sound key emitted at the start of an agent turn.
pub const VALUE_AGENT_START: &str = "protoss-probe-ack";

/// Sound key emitted at the end of an agent turn.
pub const VALUE_AGENT_END: &str = "protoss-upgrade-complete";

/// Default idle window before the extension nudges the user via a
/// text notification, in seconds. Override via the `idle_seconds`
/// field of the extension's `config` block in `harness.json5`.
pub const DEFAULT_IDLE_SECONDS: u64 = 60;

/// User-supplied configuration for this extension. Mirrors the
/// schema documented next to `DEFAULT_IDLE_SECONDS`.
#[derive(serde::Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Idle window, in seconds.
    idle_seconds: u64,
}

impl Default for ExtConfig {
    fn default() -> Self {
        Self {
            idle_seconds: DEFAULT_IDLE_SECONDS,
        }
    }
}

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    run_with_idle(reader, writer, Duration::from_secs(DEFAULT_IDLE_SECONDS))
}

/// Inbound message on the main thread's channel: either a decoded
/// event from the reader thread, or a terminal condition that ends
/// the loop.
enum InMsg {
    Event(Event),
    EndOfStream,
}

/// Test-friendly entry point. Lets unit tests drop the idle window
/// to a few hundred milliseconds so the timeout path is observable
/// without slowing the suite.
pub fn run_with_idle<R, W>(
    reader: R,
    writer: W,
    mut idle_duration: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-dpc-notifications".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_RESPONSE_FINISHED),
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_SUBMITTED),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_CONFIGURE),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("dpc-notifications ready".to_owned()),
    }))?;
    writer.flush()?;

    // Spawn a reader thread so the main loop can wait on either an
    // incoming event or an idle deadline via `recv_timeout`. The
    // reader exits naturally when stdin closes, then the channel
    // disconnects and the main loop sees EndOfStream.
    let (tx, rx) = mpsc::channel::<InMsg>();
    let _reader_handle = thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(reader));
        loop {
            match reader.read_event() {
                Ok(Some(event)) => {
                    if tx.send(InMsg::Event(event)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
                Err(_) => {
                    // Treat decode errors as end-of-stream. The
                    // socket layer above will surface the failure
                    // through its own channels.
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
            }
        }
    });

    let mut idle_deadline: Option<Instant> = None;
    let mut input_closed = false;
    loop {
        let recv_result = match (idle_deadline, input_closed) {
            // Channel still live; wait either bounded (deadline set)
            // or unbounded (no deadline) for the next event.
            (Some(deadline), false) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                rx.recv_timeout(wait)
            }
            (None, false) => match rx.recv() {
                Ok(msg) => Ok(msg),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            },
            // Input closed but a notification is still pending: the
            // output side (the UI / terminal) is independent, so
            // honor the deadline instead of dropping the warning.
            // `recv_timeout` on a disconnected channel returns
            // immediately, so explicitly sleep instead.
            (Some(deadline), true) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
                Err(mpsc::RecvTimeoutError::Timeout)
            }
            (None, true) => break,
        };

        match recv_result {
            Ok(InMsg::Event(event)) => {
                let (_, inner) = event.peel_log();
                tracing::trace!(target: LOG_TARGET, name = %inner.name(), "event received");
                match inner {
                    Event::LifecycleConfigure(msg) => {
                        match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                            Ok(cfg) => {
                                idle_duration = Duration::from_secs(cfg.idle_seconds);
                                tracing::info!(
                                    target: LOG_TARGET,
                                    idle_seconds = cfg.idle_seconds,
                                    "applied config",
                                );
                            }
                            Err(message) => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    error = %message,
                                    "rejecting config",
                                );
                                writer.write_event(&Event::LifecycleConfigError(
                                    LifecycleConfigError {
                                        message: message.clone(),
                                    },
                                ))?;
                                writer.flush()?;
                            }
                        }
                    }
                    Event::AgentPromptSubmitted(_) => {
                        idle_deadline = None;
                        writer.write_event(&sound_event(VALUE_AGENT_START))?;
                        writer.flush()?;
                    }
                    Event::UiPromptSubmitted(_) => {
                        idle_deadline = None;
                    }
                    Event::AgentResponseFinished(finished) => {
                        // The agent emits one `AgentResponseFinished`
                        // per LLM call. When `tool_calls` is non-empty,
                        // the harness will run the tools and feed the
                        // results back as a new prompt — the *turn*
                        // isn't actually done yet. Only fire the
                        // end-of-turn sound + idle timer when the
                        // agent returned a final answer with no
                        // pending tool work.
                        if !finished.tool_calls.is_empty() {
                            tracing::trace!(
                                target: LOG_TARGET,
                                tool_calls = finished.tool_calls.len(),
                                "skipping mid-turn AgentResponseFinished",
                            );
                            continue;
                        }
                        writer.write_event(&sound_event(VALUE_AGENT_END))?;
                        writer.flush()?;
                        idle_deadline = Some(Instant::now() + idle_duration);
                        tracing::debug!(
                            target: LOG_TARGET,
                            seconds = idle_duration.as_secs(),
                            "idle deadline armed",
                        );
                    }
                    Event::LifecycleDisconnect(_) => {
                        tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
                        break;
                    }
                    other => tracing::trace!(
                        target: LOG_TARGET,
                        name = %other.name(),
                        "ignoring unhandled event",
                    ),
                }
            }
            Ok(InMsg::EndOfStream) => {
                input_closed = true;
                if idle_deadline.is_none() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::info!(
                    target: LOG_TARGET,
                    "idle deadline elapsed, emitting text notification",
                );
                writer.write_event(&idle_text_event())?;
                writer.flush()?;
                idle_deadline = None;
                if input_closed {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                input_closed = true;
                if idle_deadline.is_none() {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn sound_event(value: &str) -> Event {
    Event::Osc1337SetUserVar(Osc1337SetUserVar {
        name: SOUND_VAR_NAME.to_owned(),
        value: value.to_owned(),
    })
}

fn idle_text_event() -> Event {
    let body = serde_json::json!({
        "urgency": "normal",
        "title": "Tau",
        "body": "Agent is waiting for input",
    })
    .to_string();
    Event::Osc1337SetUserVar(Osc1337SetUserVar {
        name: TEXT_VAR_NAME.to_owned(),
        value: body,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tau_proto::{
        AgentPromptSubmitted, AgentResponseFinished, Event, EventReader, EventWriter,
        LifecycleDisconnect,
    };

    use super::*;

    fn drain_lifecycle<R: std::io::Read>(reader: &mut EventReader<R>) {
        // Hello, Subscribe, Ready.
        for _ in 0..3 {
            reader.read_event().expect("read").expect("lifecycle event");
        }
    }

    #[test]
    fn emits_start_and_end_user_var_in_order() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                session_prompt_id: "sp-0".into(),
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
            }))
            .expect("write");
        // Explicit disconnect so the loop exits without waiting on
        // the (otherwise long) idle deadline triggered by the
        // `AgentResponseFinished`.
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
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
            .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                session_prompt_id: "sp-0".into(),
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
                }],
                input_tokens: None,
                cached_tokens: None,
                thinking: Some("planning".into()),
            }))
            .expect("write");
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        // We expect the start sound but NO end sound, because the
        // tool-bearing AgentResponseFinished is mid-turn.
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
    #[test]
    fn idle_timeout_fires_text_notification() {
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
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(50)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        // First the end-of-turn sound.
        let end = reader.read_event().expect("read").expect("end event");
        let Event::Osc1337SetUserVar(osc) = end else {
            panic!("expected end sound OSC");
        };
        assert_eq!(osc.name, SOUND_VAR_NAME);
        assert_eq!(osc.value, VALUE_AGENT_END);

        // Then, after the (short) idle window, the text notification.
        let idle = reader.read_event().expect("read").expect("idle event");
        let Event::Osc1337SetUserVar(osc) = idle else {
            panic!("expected idle text OSC");
        };
        assert_eq!(osc.name, TEXT_VAR_NAME);
        let payload: serde_json::Value =
            serde_json::from_str(&osc.value).expect("idle payload is JSON");
        assert_eq!(payload["urgency"], "normal");
        assert_eq!(payload["title"], "Tau");
        assert_eq!(payload["body"], "Agent is waiting for input");
    }

    /// A bogus `config` value (one that doesn't match `ExtConfig`)
    /// must trigger a `LifecycleConfigError` carrying a human-readable
    /// message, so the harness can surface it to the user.
    #[test]
    fn invalid_config_emits_lifecycle_config_error() {
        use tau_proto::{LifecycleConfigure, LifecycleDisconnect};

        // Build a config CBOR value that doesn't match ExtConfig:
        // an unknown field, which `deny_unknown_fields` rejects.
        let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
            "totally_unknown_field": 7,
        }));

        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer
            .write_event(&Event::LifecycleConfigure(LifecycleConfigure {
                config: bad_config,
            }))
            .expect("write");
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: None,
            }))
            .expect("write");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        drain_lifecycle(&mut reader);

        let err = reader
            .read_event()
            .expect("read")
            .expect("config error event");
        match err {
            Event::LifecycleConfigError(e) => {
                assert!(!e.message.is_empty(), "config error must carry a message",);
            }
            other => panic!("expected LifecycleConfigError, got {other:?}"),
        }
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
            }))
            .expect("write");
        writer
            .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "another question".into(),
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

        // Nothing else should follow — the user prompt cancelled the
        // idle deadline and stdin then closed.
        assert!(reader.read_event().expect("read eof").is_none());
    }
}
