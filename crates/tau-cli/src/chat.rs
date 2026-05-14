//! Interactive chat as a socket client of the harness daemon: input
//! loop, draft debouncer, and the threading glue that joins them.

use std::fs::OpenOptions;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_config::settings::CliBindingAction;
use tau_harness::SessionLaunchStatus;
use tau_proto::{
    ClientKind, Disconnect, Event, EventSelector, Frame, FrameReader, FrameWriter, Hello, Message,
    PROTOCOL_VERSION, Subscribe, UiPromptDraft, UiPromptSubmitted,
};

use crate::daemon::{DaemonOutput, resolve_daemon};
use crate::event_renderer::EventRenderer;
use crate::tool_render::ui_dir_block;
use crate::{CliError, MUTEX_POISONED, build_banner, locked, ui_logging};

/// Shared writer handle: the input loop and the prompt-draft debounce
/// thread both need to send events on the same socket. Stream
/// `write()` calls are atomic only up to `PIPE_BUF` (~4 KB on
/// AF_UNIX) so we serialize whole-event writes through a `Mutex`
/// instead of risking a long draft burst interleaving with a
/// `UiPromptSubmitted` mid-byte. Contention is essentially zero —
/// debounce fires at most once per second per typing burst.
pub(crate) type WriterHandle = Arc<Mutex<FrameWriter<BufWriter<UnixStream>>>>;

/// Lock the writer, write one frame and flush. Returns the underlying
/// `io::Error` on failure so callers can use `?` or discard with
/// `let _ = …`.
fn send_frame(writer: &WriterHandle, frame: &Frame) -> io::Result<()> {
    let mut w = locked(writer);
    w.write_frame(frame).map_err(io::Error::other)?;
    w.flush()
}

/// Convenience wrapper around [`send_frame`] for [`Event`] payloads.
fn send_event(writer: &WriterHandle, event: &Event) -> io::Result<()> {
    send_frame(writer, &Frame::Event(event.clone()))
}

/// Debounce period for `UiPromptDraft` emission while the user is
/// typing. Kept generous on purpose: the only consumer today
/// (std-notifications) only cares about second-or-better resolution
/// to bump its idle deadline.
const DRAFT_DEBOUNCE: Duration = Duration::from_secs(1);

/// Single-slot mailbox the input loop pushes the latest prompt
/// snapshot into; the debounce thread drains it. `pending = None` +
/// `done = false` means "nothing to send, keep waiting"; `done =
/// true` is the shutdown signal.
#[derive(Default)]
pub(crate) struct DraftSlot {
    pub(crate) pending: Option<(u64, UiPromptDraft)>,
    pub(crate) epoch: u64,
    pub(crate) done: bool,
}

/// Shared handle for the debounce mailbox. Wakeups are coordinated
/// via the `Condvar`; the debounce thread waits on it for new drafts
/// or a shutdown signal, the input loop notifies it on every
/// `BufferChanged`.
type DraftHandle = Arc<(Mutex<DraftSlot>, Condvar)>;

/// Trailing-edge debounce: wait for at least one draft to appear,
/// send the *latest* one (any older draft was overwritten by a more
/// recent typing burst), then sleep `DRAFT_DEBOUNCE` before looking
/// at the slot again. The sleep is interruptible via the `done`
/// shutdown signal so process exit is prompt.
///
/// Never drops a notification: a draft pushed during the
/// sleep stays in the slot and is sent on the next iteration.
fn debounce_loop(handle: DraftHandle, writer: WriterHandle) {
    let (mtx, cv) = &*handle;
    loop {
        // Wait for a draft to send, or shutdown.
        let snapshot = {
            let mut g = locked(mtx);
            while g.pending.is_none() && !g.done {
                g = cv.wait(g).expect(MUTEX_POISONED);
            }
            if g.done && g.pending.is_none() {
                return;
            }
            g.pending.take()
        };
        if let Some((epoch, draft)) = snapshot
            && should_send_draft_snapshot(handle.as_ref(), epoch)
        {
            // Best-effort: a write failure means the socket is gone,
            // and the input loop will notice on its next write.
            let _ = send_event(&writer, &Event::UiPromptDraft(draft));
        }
        // Coalesce subsequent typing into one event per window. Wake
        // early on shutdown so we don't spend a second sleeping after
        // the user already typed `/quit`.
        let g = locked(mtx);
        let (g, _timed_out) = cv
            .wait_timeout_while(g, DRAFT_DEBOUNCE, |s| !s.done)
            .expect(MUTEX_POISONED);
        if g.done && g.pending.is_none() {
            return;
        }
    }
}

pub(crate) fn should_send_draft_snapshot(handle: &(Mutex<DraftSlot>, Condvar), epoch: u64) -> bool {
    let (mtx, _cv) = handle;
    let g = locked(mtx);
    !g.done && g.epoch == epoch
}

fn encode_binding_action(action: &CliBindingAction) -> String {
    let Some(command) = action.command.as_deref().filter(|c| !c.is_empty()) else {
        return action.action.clone();
    };
    format!(
        "{}:{}:{}",
        action.action,
        if action.trim { "trim" } else { "raw" },
        command,
    )
}

pub(crate) fn run_chat(
    session_id: &str,
    attach: bool,
    session_status: SessionLaunchStatus,
) -> Result<(), CliError> {
    use tau_cli_term::{HighTerm, SlashCommand};

    let state_dir = tau_session_inspect::default_state_dir();
    let ui_logging = ui_logging::init(&state_dir)?;
    tracing::info!(
        target: "tau_cli::ui",
        ui_id = ui_logging.ui_id(),
        ui_dir = %ui_logging.dir().display(),
        log_path = %ui_logging.log_path().display(),
        session_id,
        attach,
        "terminal UI starting"
    );

    let startup_started_at = Instant::now();
    let daemon_output = if attach {
        None
    } else {
        // Route the daemon's stdout+stderr (where its tracing
        // subscriber writes) into the per-session harness log so it
        // sits next to per-extension logs under
        // `<session>/logs/`. The CLI's own tracing still goes to
        // `ui.log`; the two streams are intentionally separated so a
        // session post-mortem doesn't need to pull from two places.
        let sessions_dir = tau_session_inspect::default_sessions_dir();
        let harness_log = tau_harness::harness_log_path(&sessions_dir, session_id);
        if let Some(parent) = harness_log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start_offset = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&harness_log)?
            .metadata()?
            .len();
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&harness_log)
            .map(Stdio::from)?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&harness_log)
            .map(Stdio::from)?;
        Some(DaemonOutput {
            stdout,
            stderr,
            log_path: harness_log,
            start_offset,
        })
    };
    let daemon = resolve_daemon(attach, session_id, session_status, daemon_output)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness daemon resolved");
    let socket_path = daemon.socket_path();

    // Connect and split into independent reader/writer — no mutex
    // needed since they operate on cloned halves of the same stream.
    tracing::debug!(target: "tau_cli::startup", socket_path = %socket_path.display(), "connecting to harness daemon socket");
    let stream = UnixStream::connect(&socket_path)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "connected to harness daemon socket");
    let read_stream = stream.try_clone()?;
    let writer: WriterHandle = Arc::new(Mutex::new(FrameWriter::new(BufWriter::new(stream))));

    // Handshake.
    send_frame(
        &writer,
        &Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tau-chat".into(),
            client_kind: ClientKind::Ui,
        })),
    )
    .map_err(CliError::Io)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "sent hello");
    send_frame(
        &writer,
        &Frame::Message(Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("ui.".to_owned()),
                EventSelector::Prefix("session.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("tool.".to_owned()),
                EventSelector::Prefix("extension.".to_owned()),
                EventSelector::Prefix("harness.".to_owned()),
                EventSelector::Prefix("shell.".to_owned()),
                EventSelector::Prefix("term.".to_owned()),
            ],
        })),
    )
    .map_err(CliError::Io)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "sent subscribe");

    // Background socket reader — decodes events and sends them to
    // a channel as `RendererCmd::Remote`. The input thread pushes
    // `RendererCmd::Set` variants (e.g. `/set show-diff true`) into the
    // same channel so the renderer thread sees a single ordered
    // stream and never needs to share state with the input thread.
    let (event_tx, event_rx) = mpsc::channel::<RendererCmd>();
    let socket_event_tx = event_tx.clone();
    let _socket_reader = std::thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(read_stream));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    // Peel the LogEvent wrapper so downstream renderers
                    // see the inner payload directly. The UI is a
                    // best-effort consumer and does not ack.
                    let (_log_id, inner) = frame.peel_log();
                    let cmd = match inner {
                        Frame::Event(event) => RendererCmd::Remote(Box::new(event)),
                        Frame::Message(Message::Disconnect(d)) => {
                            RendererCmd::RemoteDisconnect(d.reason)
                        }
                        Frame::Message(_) => continue,
                    };
                    if socket_event_tx.send(cmd).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(target: "tau_cli::ui", %error, "socket reader exiting");
                    return;
                }
            }
        }
    });

    // Terminal setup.
    let commands = vec![
        SlashCommand::new("/quit", "Exit the chat session"),
        SlashCommand::new("/cancel", "Cancel the current in-flight prompt"),
        SlashCommand::new(
            "/detach",
            "Leave the UI but keep the harness running for later reattach",
        ),
        SlashCommand::new("/model", "Switch agent role (e.g. /model smart)"),
        SlashCommand::new("/role", "Create, edit, or delete an agent role"),
        SlashCommand::new(
            "/new",
            "Start a fresh session in this harness (current session is left as-is on disk)",
        ),
        SlashCommand::new(
            "/tree",
            "Print the session tree (`/tree <id>` rewinds head to that node)",
        ),
        SlashCommand::new(
            "/effort",
            "Set reasoning effort: off, minimal, low, medium, high, xhigh (Shift+Tab to cycle)",
        ),
        SlashCommand::new(
            "/verbosity",
            "Set output verbosity: low, medium, high (provider-dependent)",
        ),
        SlashCommand::new("/fast", "Toggle Fast mode"),
        SlashCommand::new(
            "/thinking-summary",
            "Set reasoning summary mode: off, auto, concise, detailed",
        ),
        SlashCommand::new(
            "/set",
            "Set a UI setting (e.g. /set show-diff true); Tab cycles names + values",
        ),
        SlashCommand::new(
            "/provider-auth",
            "Refresh OAuth for a provider (runs `tau provider login [name]`)",
        ),
    ];
    let theme = tau_themes::Theme::builtin();
    // Fail fast on a malformed `cli.json5`. The fields here drive
    // keybindings, prompt symbol, and cursor shape — silently falling
    // back to defaults would leave the user with broken keybindings
    // and no clue why. Refuse to start the TUI instead.
    let settings = tau_config::settings::load_cli_settings()
        .map_err(|error| CliError::Participant(format!("cli.json5 failed to parse:\n{error}")))?;
    let prompt_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER);
    let prompt = tau_cli_term::Span::new(format!("{} ", settings.prompt_symbol), prompt_style);
    let cursor_shape = if settings.bar_cursor {
        tau_cli_term::CursorShape::Bar
    } else {
        tau_cli_term::CursorShape::Block
    };
    let bindings = settings
        .bind
        .iter()
        .map(|(key, action)| (key.clone(), encode_binding_action(action)));
    let (mut term, handle, completion_data) =
        HighTerm::new(prompt, commands, theme.clone(), cursor_shape, bindings)?;

    // Show logo if enabled.
    if settings.show_logo {
        handle.print_output(tau_cli_term::StyledBlock::new(build_banner(&theme)));
    }
    handle.print_output(ui_dir_block(&theme, ui_logging.dir()));

    handle.redraw();

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    // Pre-build the renderer so we can grab its `effort_state`
    // handle for the input loop's Shift+Tab cycle. Load the
    // persisted `cli.json` state so `/set show-*` toggles survive
    // restarts.
    let dirs = tau_config::settings::TauDirs::default();
    let cli_state = tau_config::settings::CliState::load(&dirs);
    let renderer = EventRenderer::new_with_state(
        renderer_handle,
        completion_data.clone(),
        theme.clone(),
        cli_state,
        dirs,
        settings.submitted_prompt_symbol,
    );
    // Register `/set`'s context-aware arg completer. The first-arg
    // menu shows each setting's *current* value (read through the
    // renderer's shared mirror), and the second-arg menu shows
    // value-with-meaning for the selected setting.
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/set"),
        build_set_arg_completer(renderer.cli_state_mirror()),
    );
    let effort_state = renderer.effort_state();
    let fast_mode_state = renderer.fast_mode_state();
    let current_role_state = renderer.current_role_state();
    let roles_available = renderer.roles_available();
    let efforts_available = renderer.efforts_available();
    let editor_context = renderer.editor_context();
    term.set_editor_context_handle(editor_context.clone());
    let _renderer = std::thread::spawn(move || {
        let mut renderer = renderer;
        while let Ok(cmd) = renderer_rx.recv() {
            match cmd {
                RendererCmd::Remote(event) => renderer.handle(&event),
                RendererCmd::RemoteDisconnect(reason) => renderer.handle_disconnect(reason),
                RendererCmd::Set { name, value } => renderer.apply_setting(&name, &value),
            }
        }
    });

    // Spawn the prompt-draft debounce thread. The input loop signals
    // it on every `BufferChanged` event with the latest buffer
    // contents; the thread coalesces a typing burst into one
    // `UiPromptDraft` per `DRAFT_DEBOUNCE` window and sends it on the
    // shared writer.
    let draft_handle: DraftHandle = Arc::new((Mutex::new(DraftSlot::default()), Condvar::new()));
    let debounce_thread = {
        let handle = draft_handle.clone();
        let writer = writer.clone();
        std::thread::spawn(move || debounce_loop(handle, writer))
    };

    // Terminal input loop — shares the writer with the debounce
    // thread via `WriterHandle`. Theme clone is for printing local
    // validation errors (e.g. `/effort foo`) through the same
    // TermHandle as remote events, so they don't garble the TUI like
    // `eprintln!` would.
    let mut active_session_id = session_id.to_owned();
    let exit = terminal_input_loop(
        &mut term,
        &writer,
        &mut active_session_id,
        TerminalInputLoopCtx {
            effort_state,
            fast_mode_state,
            current_role_state,
            roles_available,
            efforts_available,
            theme,
            renderer_tx: event_tx,
            editor_context,
            draft_handle: draft_handle.clone(),
        },
    )?;

    // Tell the debounce thread to exit and wait for it so we don't
    // race with the disconnect below (the thread might otherwise
    // emit one final draft on the closing socket and trip an `EPIPE`).
    {
        let (mtx, cv) = &*draft_handle;
        let mut g = locked(mtx);
        g.done = true;
        cv.notify_all();
    }
    let _ = debounce_thread.join();

    // Send disconnect (best effort). Reason differs so the daemon's
    // debug log makes the distinction visible.
    let reason = match exit {
        InputLoopExit::Quit => "quit",
        InputLoopExit::Detach => "detach",
    };
    let _ = send_frame(
        &writer,
        &Frame::Message(Message::Disconnect(Disconnect {
            reason: Some(reason.to_owned()),
        })),
    );

    // Drop the writer (closes the write half) which will cause the
    // socket reader to get EOF and exit. The renderer drains remaining
    // events and exits when the channel closes.
    drop(writer);

    // On detach, we explicitly leak the daemon child (if we own one)
    // so it outlives this process. `DaemonHandle::Drop` would otherwise
    // kill the child we spawned; `/detach` is exactly the case where
    // we want it to keep running.
    match exit {
        InputLoopExit::Quit => drop(daemon),
        InputLoopExit::Detach => daemon.leak(),
    }

    tracing::info!(target: "tau_cli::ui", reason, "terminal UI exiting");

    Ok(())
}

/// How the input loop ended. Controls daemon disposition on exit.
enum InputLoopExit {
    /// User typed `/quit`, hit Ctrl-D, or the socket dropped. The
    /// daemon should be killed (if we own it) or just disconnected
    /// from (if we were attached).
    Quit,
    /// User typed `/detach`. We leave the daemon running whether we
    /// spawned it or attached to it.
    Detach,
}

/// Commands the renderer thread drains from a single ordered channel.
/// The socket reader pushes `Remote(event)`; the input loop pushes
/// local UI commands like `Set`. Keeping it one channel
/// removes the need for shared state between the two threads.
enum RendererCmd {
    /// `/set <name> <value>` — validated by the input loop before send.
    Set {
        name: String,
        value: String,
    },
    Remote(Box<Event>),
    /// The harness sent a `Disconnect` message over the wire.
    RemoteDisconnect(Option<String>),
}

struct TerminalInputLoopCtx {
    effort_state: Arc<std::sync::atomic::AtomicU8>,
    fast_mode_state: Arc<std::sync::atomic::AtomicBool>,
    current_role_state: Arc<Mutex<Option<String>>>,
    roles_available: Arc<Mutex<Vec<String>>>,
    /// Set of effort levels the harness currently accepts, kept in
    /// sync with `HarnessEffortsAvailable` by the renderer. The
    /// Shift+Tab cycle reads it so we don't ask for a level the
    /// model doesn't support (which the harness would clamp,
    /// trapping the cycle in place).
    efforts_available: Arc<Mutex<std::collections::BTreeSet<tau_proto::Effort>>>,
    theme: tau_themes::Theme,
    renderer_tx: mpsc::Sender<RendererCmd>,
    editor_context: Arc<Mutex<tau_cli_term::EditorContext>>,
    draft_handle: DraftHandle,
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &WriterHandle,
    session_id: &mut String,
    ctx: TerminalInputLoopCtx,
) -> Result<InputLoopExit, CliError> {
    // Cloned `TermHandle` so we can `print_output` for client-side
    // validation errors (`/effort foo`, `/tree blah`) from this
    // thread without borrowing `term` while the loop also holds
    // `&mut term` for `get_next_event`.
    let local_handle = term.handle().clone();
    let print_local = |message: &str| {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        local_handle.print_output(themed_block(
            &ctx.theme,
            names::SYSTEM_INFO,
            message.to_owned(),
        ));
    };
    use tau_cli_term::Event as TermEvent;

    loop {
        match term.get_next_event()? {
            TermEvent::Line(line) => {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                if let Ok(mut context) = ctx.editor_context.lock() {
                    context.previous_prompt = Some(text.to_owned());
                }
                if text == "/quit" {
                    return Ok(InputLoopExit::Quit);
                }
                if text == "/cancel" {
                    let _ = send_event(
                        writer,
                        &Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
                            session_id: session_id.as_str().into(),
                            // Broadcast cancel — abort whatever's in
                            // flight, regardless of spid. The targeted
                            // variant is used by the harness for
                            // surgical preempts.
                            session_prompt_id: None,
                        }),
                    );
                    continue;
                }
                if text == "/detach" {
                    // Tell the harness to stay alive after we leave,
                    // then exit the UI. If the write fails we still
                    // exit — the daemon will notice the disconnect
                    // and fall back to its default behavior.
                    let _ = send_event(
                        writer,
                        &Event::UiDetachRequest(tau_proto::UiDetachRequest {}),
                    );
                    return Ok(InputLoopExit::Detach);
                }
                if text == "/new" {
                    let cwd = std::env::current_dir()?;
                    let new_id = crate::daemon::mint_session_id(&cwd);
                    let _ = send_event(
                        writer,
                        &Event::UiSwitchSession(tau_proto::UiSwitchSession {
                            new_session_id: new_id.as_str().into(),
                            reason: tau_proto::SessionStartReason::New,
                        }),
                    );
                    *session_id = new_id;
                    continue;
                }
                if text == "/tree" {
                    let _ = send_event(
                        writer,
                        &Event::UiTreeRequest(tau_proto::UiTreeRequest {
                            session_id: session_id.as_str().into(),
                        }),
                    );
                    continue;
                }
                if let Some(arg) = text.strip_prefix("/tree ") {
                    match arg.trim().parse::<u64>() {
                        Ok(node_id) => {
                            let _ = send_event(
                                writer,
                                &Event::UiNavigateTree(tau_proto::UiNavigateTree {
                                    session_id: session_id.as_str().into(),
                                    node_id,
                                }),
                            );
                        }
                        Err(_) => {
                            print_local("/tree <id>: id must be a non-negative integer");
                        }
                    }
                    continue;
                }
                if let Some(arg) = text.strip_prefix("/effort ") {
                    match arg.trim().parse::<tau_proto::Effort>() {
                        Ok(level) => {
                            let _ = send_event(
                                writer,
                                &Event::UiSetEffort(tau_proto::UiSetEffort { level }),
                            );
                        }
                        Err(msg) => print_local(&format!("/effort: {msg}")),
                    }
                    continue;
                }
                if text == "/effort" {
                    print_local("/effort <level> — one of: off, minimal, low, medium, high, xhigh");
                    continue;
                }
                if text == "/fast" {
                    let enabled = ctx
                        .fast_mode_state
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let service_tier = if enabled {
                        None
                    } else {
                        Some(tau_proto::ServiceTier::Fast)
                    };
                    let _ = send_event(
                        writer,
                        &Event::UiSetServiceTier(tau_proto::UiSetServiceTier { service_tier }),
                    );
                    continue;
                }
                if text.starts_with("/fast ") {
                    print_local("/fast toggles Fast mode");
                    continue;
                }
                if let Some(arg) = text.strip_prefix("/verbosity ") {
                    match arg.trim().parse::<tau_proto::Verbosity>() {
                        Ok(level) => {
                            let _ = send_event(
                                writer,
                                &Event::UiSetVerbosity(tau_proto::UiSetVerbosity { level }),
                            );
                        }
                        Err(error) => print_local(&format!("/verbosity: {error}")),
                    }
                    continue;
                }
                if text == "/verbosity" {
                    print_local("/verbosity <level> — one of: low, medium, high");
                    continue;
                }
                if let Some(arg) = text.strip_prefix("/thinking-summary ") {
                    match arg.trim().parse::<tau_proto::ThinkingSummary>() {
                        Ok(level) => {
                            let _ = send_event(
                                writer,
                                &Event::UiSetThinkingSummary(tau_proto::UiSetThinkingSummary {
                                    level,
                                }),
                            );
                        }
                        Err(error) => print_local(&format!("/thinking-summary: {error}")),
                    }
                    continue;
                }
                if text == "/thinking-summary" {
                    print_local("/thinking-summary <mode> — one of: off, auto, concise, detailed");
                    continue;
                }
                if let Some(provider) = text.strip_prefix("/provider-auth ") {
                    let provider = provider.trim();
                    if !provider.is_empty() {
                        run_provider_auth(provider, &print_local);
                    }
                    continue;
                }
                if text == "/provider-auth" {
                    run_provider_auth("", &print_local);
                    continue;
                }
                if text == "/set" || text.starts_with("/set ") {
                    handle_set_command(text, &ctx.renderer_tx, &print_local);
                    continue;
                }
                if text == "/role" || text.starts_with("/role ") {
                    handle_role_command(text, writer, &print_local);
                    continue;
                }
                if let Some(role) = text.strip_prefix("/model ") {
                    let role = role.trim();
                    if !role.is_empty() {
                        let _ = send_event(
                            writer,
                            &Event::UiRoleSelect(tau_proto::UiRoleSelect {
                                role: role.to_owned(),
                            }),
                        );
                    }
                    continue;
                }
                if text == "/model" {
                    // No argument — just a reminder.
                    continue;
                }

                // `!!<cmd>` / `!<cmd>`: run a shell command locally.
                // `!!` excludes the result from the agent's context;
                // `!` (single bang) includes it.
                if let Some(command) = text.strip_prefix("!!") {
                    let command = command.trim();
                    if !command.is_empty()
                        && let Err(error) = send_shell_command(writer, session_id, command, false)
                    {
                        tracing::warn!(target: "tau_cli::ui", %error, "failed to send !! shell command");
                    }
                    continue;
                }
                if let Some(command) = text.strip_prefix('!') {
                    let command = command.trim();
                    if !command.is_empty()
                        && let Err(error) = send_shell_command(writer, session_id, command, true)
                    {
                        tracing::warn!(target: "tau_cli::ui", %error, "failed to send ! shell command");
                    }
                    continue;
                }

                // Submission terminates the in-flight draft window —
                // the buffer just got cleared by the user pressing
                // Enter, so any pending draft is now stale. Invalidate
                // before sending the submission so a debounce thread that
                // already took an older snapshot can't emit it afterward.
                {
                    let (mtx, cv) = &*ctx.draft_handle;
                    if let Ok(mut g) = mtx.lock() {
                        g.epoch = g.epoch.wrapping_add(1);
                        g.pending = None;
                        cv.notify_one();
                    }
                }
                if send_event(
                    writer,
                    &Event::UiPromptSubmitted(UiPromptSubmitted {
                        session_id: session_id.as_str().into(),
                        text: text.to_owned(),
                        originator: tau_proto::PromptOriginator::User,
                        ctx_id: None,
                    }),
                )
                .is_err()
                {
                    return Ok(InputLoopExit::Quit);
                }
            }
            TermEvent::Eof => return Ok(InputLoopExit::Quit),
            TermEvent::Resize { .. } => {
                tracing::debug!(target: "tau_cli::ui", "terminal resized");
            }
            TermEvent::BufferChanged => {
                // Trailing-edge debounce: stash the latest buffer
                // contents and wake the debounce thread; it will
                // coalesce a typing burst into one `UiPromptDraft`
                // per `DRAFT_DEBOUNCE` window.
                let text = term.handle().get_buffer();
                let (mtx, cv) = &*ctx.draft_handle;
                if let Ok(mut g) = mtx.lock() {
                    g.pending = Some((
                        g.epoch,
                        UiPromptDraft {
                            session_id: session_id.as_str().into(),
                            text,
                        },
                    ));
                    tracing::trace!(target: "tau_cli::ui", "prompt draft updated");
                    cv.notify_one();
                }
            }
            TermEvent::FastToggle => {
                let enabled = ctx
                    .fast_mode_state
                    .load(std::sync::atomic::Ordering::Relaxed);
                let service_tier = if enabled {
                    None
                } else {
                    Some(tau_proto::ServiceTier::Fast)
                };
                let _ = send_event(
                    writer,
                    &Event::UiSetServiceTier(tau_proto::UiSetServiceTier { service_tier }),
                );
            }
            TermEvent::RoleCycle => {
                let roles = match ctx.roles_available.lock() {
                    Ok(roles) => roles.clone(),
                    Err(_) => Vec::new(),
                };
                if roles.is_empty() {
                    print_local("role-cycle: no agent roles are available yet");
                    continue;
                }
                let current = ctx
                    .current_role_state
                    .lock()
                    .ok()
                    .and_then(|role| role.clone());
                let next = match current
                    .as_deref()
                    .and_then(|current| roles.iter().position(|role| role == current))
                {
                    Some(index) => roles[(index + 1) % roles.len()].clone(),
                    None => roles[0].clone(),
                };
                let _ = send_event(
                    writer,
                    &Event::UiRoleSelect(tau_proto::UiRoleSelect { role: next }),
                );
            }
            TermEvent::BackTab => {
                // Pi-style: cycle effort. Read the current level
                // from the shared atomic the renderer keeps in sync
                // with `HarnessEffortChanged`, advance through the
                // currently-allowed set (mirrored from
                // `HarnessEffortsAvailable`), send the request. The
                // harness echoes back and the renderer updates the
                // status block. Skipping unavailable levels avoids
                // a stuck cycle when the model lacks `xhigh`.
                let current = tau_proto::Effort::from_u8(
                    ctx.effort_state.load(std::sync::atomic::Ordering::Relaxed),
                )
                .unwrap_or_default();
                let allowed: Vec<tau_proto::Effort> = match ctx.efforts_available.lock() {
                    Ok(set) => set.iter().copied().collect(),
                    Err(_) => Vec::new(),
                };
                if allowed.is_empty() {
                    // No allowed set known yet (pre-handshake or no
                    // model selected). Don't send a request the
                    // harness would just clamp.
                    continue;
                }
                let next = current.next_in(&allowed);
                if next == current {
                    continue;
                }
                let _ = send_event(
                    writer,
                    &Event::UiSetEffort(tau_proto::UiSetEffort { level: next }),
                );
            }
        }
    }
}

/// Build the `/set` argument completer. The first arg is a setting
/// name (description = current value); the second arg is one of that
/// setting's allowed values (description = value meaning). Returns
/// no candidates from the third arg onward.
fn build_set_arg_completer(
    cli_state: Arc<Mutex<tau_config::settings::CliState>>,
) -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    use crate::settings_registry;

    Arc::new(move |args: &[&str]| match args.len() {
        1 => {
            // Snapshot the current state once so every name's
            // description sees a consistent view.
            let snapshot = cli_state.lock().ok().map(|g| g.clone());
            let needle = args[0].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for def in settings_registry::SETTINGS {
                let lower = def.name.to_lowercase();
                let current = snapshot.as_ref().map(|s| (def.get)(s)).unwrap_or("?");
                let description = format!("[{current}] {}", def.description);
                let item = CompletionItem::new(def.name, description);
                if needle.is_empty() || lower.starts_with(&needle) {
                    prefix_matches.push(item);
                } else if lower.contains(&needle) {
                    substr_matches.push(item);
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        }
        2 => {
            let Some(def) = settings_registry::find(args[0]) else {
                return Vec::new();
            };
            let needle = args[1].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for v in def.values {
                let lower = v.value.to_lowercase();
                let item = CompletionItem::new(v.value, v.description);
                if needle.is_empty() || lower.starts_with(&needle) {
                    prefix_matches.push(item);
                } else if lower.contains(&needle) {
                    substr_matches.push(item);
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        }
        _ => Vec::new(),
    })
}

/// Parse and dispatch `/set <name> <value>`. Validation lives here
/// (input-loop thread) so the renderer can trust `RendererCmd::Set`
/// to always be a known name and an allowed value.
fn handle_set_command(
    text: &str,
    renderer_tx: &mpsc::Sender<RendererCmd>,
    print_local: &impl Fn(&str),
) {
    use crate::settings_registry;

    let rest = text.strip_prefix("/set").unwrap_or("").trim();
    let mut parts = rest.split_whitespace();
    let name = parts.next();
    let value = parts.next();
    let extra = parts.next();

    let usage = || {
        let names: Vec<&str> = settings_registry::SETTINGS.iter().map(|s| s.name).collect();
        print_local(&format!("/set <name> <value>; names: {}", names.join(", ")));
    };

    let (Some(name), Some(value)) = (name, value) else {
        usage();
        return;
    };
    if extra.is_some() {
        print_local("/set: too many arguments");
        return;
    }
    let Some(def) = settings_registry::find(name) else {
        print_local(&format!("/set: unknown setting `{name}`"));
        return;
    };
    if !def.values.iter().any(|v| v.value == value) {
        let allowed: Vec<&str> = def.values.iter().map(|v| v.value).collect();
        print_local(&format!(
            "/set {name}: invalid value `{value}` (allowed: {})",
            allowed.join(", "),
        ));
        return;
    }
    let _ = renderer_tx.send(RendererCmd::Set {
        name: name.to_owned(),
        value: value.to_owned(),
    });
}

fn handle_role_command(text: &str, writer: &WriterHandle, print_local: &impl Fn(&str)) {
    let rest = text.strip_prefix("/role").unwrap_or("").trim();
    let mut parts = rest.split_whitespace();
    let role = parts.next();
    let command = parts.next();
    let value = parts.next();
    let extra = parts.next();
    let (Some(role), Some(command)) = (role, command) else {
        print_local(
            "/role <role> <delete|model|effort|verbosity|thinking-summary|fast-mode> [value]",
        );
        return;
    };
    if command == "delete" {
        if value.is_some() {
            print_local("/role <role> delete takes no value");
            return;
        }
        let _ = send_event(
            writer,
            &Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
                role: role.to_owned(),
                action: tau_proto::UiRoleUpdateAction::Delete,
            }),
        );
        return;
    }
    let Some(value) = value else {
        print_local("/role <role> <setting> <value>");
        return;
    };
    if extra.is_some() {
        print_local("/role: too many arguments");
        return;
    }
    let allowed = [
        "model",
        "effort",
        "verbosity",
        "thinking-summary",
        "fast-mode",
    ];
    if !allowed.contains(&command) {
        print_local("/role: unknown setting");
        return;
    }
    let _ = send_event(
        writer,
        &Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
            role: role.to_owned(),
            action: tau_proto::UiRoleUpdateAction::Set {
                setting: command.to_owned(),
                value: value.to_owned(),
            },
        }),
    );
}

fn run_provider_auth(provider: &str, print_local: &impl Fn(&str)) {
    print_local("starting provider auth; follow prompts in the terminal");
    let mut args = vec!["provider".to_owned(), "login".to_owned()];
    if !provider.is_empty() {
        args.push(provider.to_owned());
    }
    match tau_provider_cli::run(&args) {
        Ok(()) => print_local("provider auth refreshed; new prompts will use updated credentials"),
        Err(error) => print_local(&format!("provider auth failed: {error}")),
    }
}

fn send_shell_command(
    writer: &WriterHandle,
    session_id: &str,
    command: &str,
    include_in_context: bool,
) -> io::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let command_id = format!(
        "ui-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    send_event(
        writer,
        &Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: session_id.into(),
            command_id: command_id.into(),
            command: command.to_owned(),
            include_in_context,
        }),
    )
}
