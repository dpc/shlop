//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tau_harness::runtime_dir;
use tau_proto::{
    CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleDisconnect,
    LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION, UiPromptSubmitted,
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

const STARTUP_PUNS: &[&str] = &[
    "Tau is like Pi, but twice as much.",
    "A new angle on coding agents.",
    "Tau day is every day if you care about circles enough.",
    "Come for the agent, stay for the circumference discourse.",
    "Tau is the irrational choice for rational Unix hackers.",
    "Small tools, loosely joined — that’s the Tau of Unix.",
    "In Tau, what goes around comes around over stdio.",
    "We’ve come full τurn.",
    "Tau keeps the loop tight and the pipes honest.",
    "Every extension gets its turn in Tau.",
    "Tau speaks fluent stdio with a circular accent.",
    "Agents, tools, sockets, loops: a well-rounded lineup.",
    "Ready, set, Tau!",
    "Tau day to code.",
    "Tau-tal control.",
    "Tau-tally operational.",
    "Tau much power in one terminal.",
    "Tau infinity and beyond.",
    "Tau the line between human and agent.",
    "Tau’s what I’m talking about.",
    "One shell to Tau them all.",
    "Tau small step for code, one giant leap for CLI-kind.",
    "Tau-powered, terminal-native.",
    "Complete revolution.",
    "Wrapping around nicely.",
    "Continuous on S¹, probably.",
    "Cohomology remains left as exercise.",
];

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the CLI.
#[derive(Debug)]
pub enum CliError {
    Io(io::Error),
    Encode(tau_proto::EncodeError),
    Harness(tau_harness::HarnessError),
    DaemonStartTimeout,
    DaemonExited(String),
    NoRunningDaemon,
    Participant(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Encode(source) => write!(f, "encode error: {source}"),
            Self::Harness(source) => write!(f, "harness error: {source}"),
            Self::DaemonStartTimeout => {
                f.write_str("timed out waiting for harness daemon to start")
            }
            Self::DaemonExited(msg) => write!(f, "harness daemon exited: {msg}"),
            Self::NoRunningDaemon => f.write_str(
                "no harness daemon running for this project — \
                 drop `--attach` to spawn one",
            ),
            Self::Participant(msg) => write!(f, "participant error: {msg}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<tau_harness::HarnessError> for CliError {
    fn from(source: tau_harness::HarnessError) -> Self {
        Self::Harness(source)
    }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

/// How this CLI invocation is related to its harness daemon.
///
/// - `Owned`: we spawned the daemon; Drop kills it unless the UI detached
///   (calls [`DaemonHandle::leak`]), in which case we forget the `Child` so the
///   daemon outlives us.
/// - `Attached`: we joined a daemon someone else owns. Drop never touches it.
enum DaemonHandle {
    /// `child` is `Some` until [`leak`] pulls it out.
    Owned {
        child: Option<std::process::Child>,
        daemon_dir: PathBuf,
    },
    Attached {
        daemon_dir: PathBuf,
    },
}

impl DaemonHandle {
    fn socket_path(&self) -> PathBuf {
        runtime_dir::socket_path(self.daemon_dir())
    }

    fn daemon_dir(&self) -> &Path {
        match self {
            Self::Owned { daemon_dir, .. } | Self::Attached { daemon_dir } => daemon_dir,
        }
    }

    /// Consume the handle without killing the child.
    ///
    /// Used by `/detach`: we want the daemon to outlive this CLI,
    /// whether we spawned it or attached to it. For `Owned` this
    /// `mem::forget`s the `Child` — on Linux its parent becomes init
    /// on our exit, which is exactly what we want for a long-lived
    /// daemon.
    fn leak(mut self) {
        if let Self::Owned { child, .. } = &mut self {
            if let Some(child) = child.take() {
                std::mem::forget(child);
            }
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Self::Owned {
            child: Some(child), ..
        } = self
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Attached, or Owned-after-leak: do nothing. The daemon keeps
        // running so other UIs can still use it, or this same UI can
        // `tau run -a` back in later.
    }
}

/// Resolves a harness daemon to talk to, either by attaching to an
/// existing one for this project or by spawning a fresh one.
///
/// The fresh-spawn path passes `session_id` to the daemon via the
/// `TAU_SESSION_ID` env var, so its eager-init targets the right session.
/// Resolves the session id for one `tau run` invocation.
///
/// - `None` → mint `<basename(cwd)>-<rand6>`.
/// - `Some("")` (bare `-r`) → resume the most recent session whose
///   `meta.json.cwd` matches cwd; if none, mint fresh.
/// - `Some(id)` → resume that explicit id.
fn resolve_run_session_id(resume: Option<&str>) -> Result<String, CliError> {
    let cwd = std::env::current_dir()?;
    match resume {
        None => Ok(mint_session_id(&cwd)),
        Some("") => Ok(find_most_recent_session(&cwd).unwrap_or_else(|| mint_session_id(&cwd))),
        Some(id) => Ok(id.to_owned()),
    }
}

fn mint_session_id(cwd: &Path) -> String {
    use rand::Rng;
    let basename = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    let suffix: String = (0..6)
        .map(|_| {
            let n: u8 = rand::thread_rng().gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + (n - 10)) as char
            }
        })
        .collect();
    format!("{basename}-{suffix}")
}

fn find_most_recent_session(cwd: &Path) -> Option<String> {
    let state_dir = tau_harness::default_state_dir();
    let metas = tau_harness::list_session_metas(&state_dir).ok()?;
    metas
        .into_iter()
        .filter(|(_, meta): &(_, tau_harness::SessionMeta)| meta.cwd.as_deref() == Some(cwd))
        .max_by_key(|(_, meta)| meta.last_touched)
        .map(|(sid, _)| sid.as_str().to_owned())
}

fn resolve_daemon(attach: bool, session_id: &str) -> Result<DaemonHandle, CliError> {
    let project_root = std::env::current_dir()?;
    if attach {
        let daemon_dir =
            runtime_dir::find_harness_for_dir(&project_root).ok_or(CliError::NoRunningDaemon)?;
        return Ok(DaemonHandle::Attached { daemon_dir });
    }
    start_daemon(session_id)
}

/// Spawns a new harness daemon and waits for its socket to be ready.
fn start_daemon(session_id: &str) -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;

    let mut child = Command::new(&tau_binary)
        .arg("ext")
        .arg("harness")
        .env("TAU_SESSION_ID", session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;

    let daemon_dir = runtime_dir::root_runtime_dir().join(child.id().to_string());
    let dir_marker = daemon_dir.join("tau.dir");
    let started_at = Instant::now();

    loop {
        if dir_marker.exists() {
            return Ok(DaemonHandle::Owned {
                child: Some(child),
                daemon_dir,
            });
        }
        if let Some(status) = child.try_wait()? {
            return Err(CliError::DaemonExited(format!("exit status: {status}")));
        }
        if DAEMON_START_TIMEOUT <= started_at.elapsed() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CliError::DaemonStartTimeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Chat as socket client
// ---------------------------------------------------------------------------

fn run_chat(session_id: &str, attach: bool) -> Result<(), CliError> {
    use tau_cli_term::{HighTerm, SlashCommand};

    let daemon = resolve_daemon(attach, session_id)?;
    let socket_path = daemon.socket_path();

    // Connect and split into independent reader/writer — no mutex
    // needed since they operate on cloned halves of the same stream.
    let stream = UnixStream::connect(&socket_path)?;
    let read_stream = stream.try_clone()?;
    let mut writer = EventWriter::new(BufWriter::new(stream));

    // Handshake.
    writer
        .write_event(&Event::LifecycleHello(LifecycleHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tau-chat".into(),
            client_kind: ClientKind::Ui,
        }))
        .map_err(CliError::Encode)?;
    writer
        .write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
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
        }))
        .map_err(CliError::Encode)?;
    writer.flush()?;

    // Background socket reader — decodes events and sends them to
    // a channel as `RendererCmd::Remote`. The input thread pushes
    // `RendererCmd::Local` variants (e.g. `/diff` toggles) into the
    // same channel so the renderer thread sees a single ordered
    // stream and never needs to share state with the input thread.
    let (event_tx, event_rx) = mpsc::channel::<RendererCmd>();
    let socket_event_tx = event_tx.clone();
    let _socket_reader = std::thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(read_stream));
        loop {
            match reader.read_event() {
                Ok(Some(event)) => {
                    // Peel the LogEvent wrapper so downstream renderers
                    // see the inner payload directly. The UI is a
                    // best-effort consumer and does not ack.
                    let (_log_id, inner) = event.peel_log();
                    if socket_event_tx.send(RendererCmd::Remote(inner)).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    });

    // Terminal setup.
    let commands = vec![
        SlashCommand::new("/quit", "Exit the chat session"),
        SlashCommand::new(
            "/detach",
            "Leave the UI but keep the harness running for later reattach",
        ),
        SlashCommand::new("/model", "Switch model (e.g. /model provider/model-id)"),
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
            "/diff",
            "Toggle expanded vs compact display of file edit diffs",
        ),
    ];
    let theme = tau_themes::Theme::builtin();
    let settings = tau_config::settings::load_cli_settings().unwrap_or_default();
    let prompt_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER);
    let prompt = tau_cli_term::Span::new("> ", prompt_style);
    let cursor_shape = if settings.bar_cursor {
        tau_cli_term::CursorShape::Bar
    } else {
        tau_cli_term::CursorShape::Block
    };
    let (mut term, handle, completion_data) =
        HighTerm::new(prompt, commands, theme.clone(), cursor_shape)?;

    // Show logo if enabled.
    if settings.show_logo {
        use tau_cli_term::{Span, StyledBlock, StyledText};
        let accent = tau_cli_term::resolve::resolve(&theme, tau_themes::names::BANNER_ACCENT);
        let pun = random_startup_pun();
        let banner = StyledText::from(vec![
            Span::new("▀█▀▀ ", accent),
            Span::plain(format!("tau {}", env!("CARGO_PKG_VERSION"))),
            Span::new("\n", Default::default()),
            Span::new(" █▄▖ ", accent),
            Span::plain(pun),
        ]);
        handle.print_output(StyledBlock::new(banner));
    }

    handle.redraw();

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    let renderer_completion_data = completion_data;
    // Pre-build the renderer so we can grab its `effort_state`
    // handle for the input loop's Shift+Tab cycle.
    let renderer = EventRenderer::new(renderer_handle, renderer_completion_data, theme.clone());
    let effort_state = renderer.effort_state();
    let _renderer = std::thread::spawn(move || {
        let mut renderer = renderer;
        while let Ok(cmd) = renderer_rx.recv() {
            match cmd {
                RendererCmd::Remote(event) => renderer.handle(&event),
                RendererCmd::ToggleDiffs => renderer.toggle_diffs_expanded(),
            }
        }
    });

    // Terminal input loop — owns the writer, no locking needed. Theme
    // clone is for printing local validation errors (e.g. `/effort
    // foo`) through the same TermHandle as remote events, so they
    // don't garble the TUI like `eprintln!` would.
    let mut active_session_id = session_id.to_owned();
    let exit = terminal_input_loop(
        &mut term,
        &mut writer,
        &mut active_session_id,
        effort_state,
        theme,
        event_tx,
    )?;

    // Send disconnect (best effort). Reason differs so the daemon's
    // debug log makes the distinction visible.
    let reason = match exit {
        InputLoopExit::Quit => "quit",
        InputLoopExit::Detach => "detach",
    };
    let _ = writer.write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
        reason: Some(reason.to_owned()),
    }));
    let _ = writer.flush();

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

    Ok(())
}

fn random_startup_pun() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize % STARTUP_PUNS.len())
        .unwrap_or(0);
    STARTUP_PUNS[idx]
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
/// local UI commands like `ToggleDiffs`. Keeping it one channel
/// removes the need for shared state between the two threads.
enum RendererCmd {
    Remote(Event),
    ToggleDiffs,
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &mut EventWriter<BufWriter<UnixStream>>,
    session_id: &mut String,
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    theme: tau_themes::Theme,
    renderer_tx: std::sync::mpsc::Sender<RendererCmd>,
) -> Result<InputLoopExit, CliError> {
    // Cloned `TermHandle` so we can `print_output` for client-side
    // validation errors (`/effort foo`, `/tree blah`) from this
    // thread without borrowing `term` while the loop also holds
    // `&mut term` for `get_next_event`.
    let local_handle = term.handle().clone();
    let print_local = |message: &str| {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        local_handle.print_output(themed_block(&theme, names::SYSTEM_INFO, message.to_owned()));
    };
    use tau_cli_term::Event as TermEvent;

    loop {
        match term.get_next_event()? {
            TermEvent::Line(line) => {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                if text == "/quit" {
                    return Ok(InputLoopExit::Quit);
                }
                if text == "/detach" {
                    // Tell the harness to stay alive after we leave,
                    // then exit the UI. If the write fails we still
                    // exit — the daemon will notice the disconnect
                    // and fall back to its default behavior.
                    let _ =
                        writer.write_event(&Event::UiDetachRequest(tau_proto::UiDetachRequest {}));
                    let _ = writer.flush();
                    return Ok(InputLoopExit::Detach);
                }
                if text == "/new" {
                    let cwd = std::env::current_dir()?;
                    let new_id = mint_session_id(&cwd);
                    let _ =
                        writer.write_event(&Event::UiSwitchSession(tau_proto::UiSwitchSession {
                            new_session_id: new_id.as_str().into(),
                            reason: tau_proto::SessionStartReason::New,
                        }));
                    let _ = writer.flush();
                    *session_id = new_id;
                    continue;
                }
                if text == "/tree" {
                    let _ = writer.write_event(&Event::UiTreeRequest(tau_proto::UiTreeRequest {
                        session_id: session_id.as_str().into(),
                    }));
                    let _ = writer.flush();
                    continue;
                }
                if let Some(arg) = text.strip_prefix("/tree ") {
                    match arg.trim().parse::<u64>() {
                        Ok(node_id) => {
                            let _ = writer.write_event(&Event::UiNavigateTree(
                                tau_proto::UiNavigateTree {
                                    session_id: session_id.as_str().into(),
                                    node_id,
                                },
                            ));
                            let _ = writer.flush();
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
                            let _ = writer
                                .write_event(&Event::UiSetEffort(tau_proto::UiSetEffort { level }));
                            let _ = writer.flush();
                        }
                        Err(msg) => print_local(&format!("/effort: {msg}")),
                    }
                    continue;
                }
                if text == "/effort" {
                    print_local("/effort <level> — one of: off, minimal, low, medium, high, xhigh");
                    continue;
                }
                if text == "/diff" {
                    let _ = renderer_tx.send(RendererCmd::ToggleDiffs);
                    continue;
                }
                if let Some(model) = text.strip_prefix("/model ") {
                    let model = model.trim();
                    if !model.is_empty() {
                        let _ =
                            writer.write_event(&Event::UiModelSelect(tau_proto::UiModelSelect {
                                model: model.into(),
                            }));
                        let _ = writer.flush();
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
                    if !command.is_empty() {
                        let _ = send_shell_command(writer, session_id, command, false);
                    }
                    continue;
                }
                if let Some(command) = text.strip_prefix('!') {
                    let command = command.trim();
                    if !command.is_empty() {
                        let _ = send_shell_command(writer, session_id, command, true);
                    }
                    continue;
                }

                if writer
                    .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                        session_id: session_id.as_str().into(),
                        text: text.to_owned(),
                    }))
                    .is_err()
                {
                    return Ok(InputLoopExit::Quit);
                }
                if writer.flush().is_err() {
                    return Ok(InputLoopExit::Quit);
                }
            }
            TermEvent::Eof => return Ok(InputLoopExit::Quit),
            TermEvent::Resize { .. } | TermEvent::BufferChanged => {}
            TermEvent::BackTab => {
                // Pi-style: cycle effort. Read the current
                // level from the shared atomic the renderer keeps in
                // sync with `HarnessEffortChanged`, advance,
                // send the request. The harness echoes back and the
                // renderer updates the status block.
                let current =
                    effort_from_u8(effort_state.load(std::sync::atomic::Ordering::Relaxed));
                let next = current.next();
                let _ =
                    writer.write_event(&Event::UiSetEffort(tau_proto::UiSetEffort { level: next }));
                let _ = writer.flush();
            }
        }
    }
}

fn effort_to_u8(level: tau_proto::Effort) -> u8 {
    match level {
        tau_proto::Effort::Off => 0,
        tau_proto::Effort::Minimal => 1,
        tau_proto::Effort::Low => 2,
        tau_proto::Effort::Medium => 3,
        tau_proto::Effort::High => 4,
        tau_proto::Effort::XHigh => 5,
    }
}

fn effort_from_u8(value: u8) -> tau_proto::Effort {
    match value {
        1 => tau_proto::Effort::Minimal,
        2 => tau_proto::Effort::Low,
        3 => tau_proto::Effort::Medium,
        4 => tau_proto::Effort::High,
        5 => tau_proto::Effort::XHigh,
        _ => tau_proto::Effort::Off,
    }
}

/// Format the context-usage chip for the status bar. Three cases:
/// - context window known → `" {percent}%/{window}"` (e.g. `" 6%/200k"`)
/// - window unknown but token count reported → `" {tokens}/?"`
/// - nothing known yet → empty string (chip suppressed)
fn format_context_chip(
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    percent: Option<u8>,
    window: Option<u64>,
) -> String {
    let cache_suffix = match cached_tokens {
        Some(0) | None => String::new(),
        Some(tokens) => format!(" cache:{}", format_token_count(tokens)),
    };
    match (window, percent, input_tokens) {
        (Some(w), Some(p), _) => format!(" {p}%/{}{}", format_token_count(w), cache_suffix),
        // Window not configured — fall back to raw token count so the
        // user can see usage exists and add `contextWindow` to fix it.
        (None, _, Some(t)) => format!(" {}/?{}", format_token_count(t), cache_suffix),
        _ => String::new(),
    }
}

fn format_turn_metrics_chip(latency: Option<Duration>, cache_hit_percent: Option<u8>) -> String {
    let mut chip = String::new();
    if let Some(percent) = cache_hit_percent {
        chip.push_str(&format!(" hit:{percent}%"));
    }
    if let Some(latency) = latency {
        chip.push_str(&format!(" resp:{}", format_latency(latency)));
    }
    chip
}

fn format_latency(latency: Duration) -> String {
    if latency < Duration::from_secs(1) {
        return format!("{}ms", latency.as_millis());
    }
    if latency < Duration::from_secs(10) {
        let tenths = latency.as_millis() / 100;
        let whole = tenths / 10;
        let fractional = tenths % 10;
        return format!("{whole}.{fractional}s");
    }
    format!("{}s", latency.as_secs())
}

fn cache_hit_percent(input_tokens: Option<u64>, cached_tokens: Option<u64>) -> Option<u8> {
    let input_tokens = input_tokens?;
    let cached_tokens = cached_tokens?;
    if input_tokens == 0 {
        return Some(0);
    }
    let clamped_cached_tokens = cached_tokens.min(input_tokens);
    let percent = clamped_cached_tokens.saturating_mul(100) / input_tokens;
    Some(percent.min(100) as u8)
}

/// Build the iTerm2 OSC 1337 `SetUserVar` escape sequence for the
/// given (name, value) pair, with `value` base64-encoded.
///
/// When `in_tmux` is true the sequence is wrapped in
/// `\x1bPtmux;...\x1b\\` and the inner ESC is doubled so tmux passes
/// the OSC through to the outer terminal instead of consuming it.
/// Mirrors the shape used by the `user-notification.sh` reference
/// script. Caller is responsible for detecting tmux (typically by
/// checking `$TMUX`).
fn build_osc1337_set_user_var(name: &str, value: &str, in_tmux: bool) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let encoded = STANDARD.encode(value.as_bytes());
    if in_tmux {
        format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar={name}={encoded}\x07\x1b\\")
    } else {
        format!("\x1b]1337;SetUserVar={name}={encoded}\x07")
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenth = (tokens % 1_000) / 100;
        if tenth == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenth}k");
    }
    let whole = tokens / 1_000_000;
    let tenth = (tokens % 1_000_000) / 100_000;
    if tenth == 0 {
        return format!("{whole}m");
    }
    format!("{whole}.{tenth}m")
}

/// Mint a fresh `command_id` and emit a `UiShellCommand` for a
/// `!`/`!!` line. Returns `Err` only on write failure (same caller
/// pattern as the other slash commands — input loop keeps going).
fn send_shell_command(
    writer: &mut EventWriter<BufWriter<UnixStream>>,
    session_id: &str,
    command: &str,
    include_in_context: bool,
) -> Result<(), ()> {
    let command_id = format!(
        "ui-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    writer
        .write_event(&Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: session_id.into(),
            command_id: command_id.into(),
            command: command.to_owned(),
            include_in_context,
        }))
        .map_err(|_| ())?;
    writer.flush().map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Tool display helpers
// ---------------------------------------------------------------------------

fn cbor_text_field(value: &CborValue, key: &str) -> Option<String> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let (CborValue::Text(k), CborValue::Text(v)) = (k, v) {
                if k == key {
                    return Some(v.clone());
                }
            }
        }
    }
    None
}

fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let (CborValue::Text(k), CborValue::Integer(n)) = (k, v) {
                if k == key {
                    return Some((*n).into());
                }
            }
        }
    }
    None
}

/// Returns the sub-`CborValue` at `key` in a map, if present.
fn cbor_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let CborValue::Text(k) = k {
                if k == key {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Format the `+N/-M` chip from a `DiffSummary` sub-tree on a tool
/// result as themed suffix segments. `+N` is painted with the
/// diff-added style and `-M` with the diff-removed style, matching
/// `git diff --shortstat`. The parens and slash stay in the muted info
/// style. Returns `None` if the diff is missing or empty.
fn format_diff_chip_segments(details: &CborValue) -> Option<Vec<ToolSuffixSegment>> {
    let diff = cbor_field(details, "diff")?;
    let added = cbor_int_field(diff, "added").unwrap_or(0);
    let removed = cbor_int_field(diff, "removed").unwrap_or(0);
    if added == 0 && removed == 0 {
        return None;
    }
    Some(vec![
        info_suffix("(".to_owned()),
        ToolSuffixSegment {
            text: format!("+{added}"),
            status: ToolStatus::DiffAdded,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: "/".to_owned(),
            status: ToolStatus::Info,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: format!("-{removed}"),
            status: ToolStatus::DiffRemoved,
            no_leading_space: true,
        },
        ToolSuffixSegment {
            text: ")".to_owned(),
            status: ToolStatus::Info,
            no_leading_space: true,
        },
    ])
}

/// Decode a `DiffSummary` sub-tree from a tool result, if present and
/// non-empty. Round-trips the CBOR sub-value through ciborium.
fn extract_diff(details: &CborValue) -> Option<tau_proto::DiffSummary> {
    let diff = cbor_field(details, "diff")?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(diff, &mut buf).ok()?;
    let summary: tau_proto::DiffSummary = ciborium::de::from_reader(buf.as_slice()).ok()?;
    if summary.added == 0 && summary.removed == 0 {
        return None;
    }
    Some(summary)
}

/// Formats a tool call for display while it is running.
/// Which status-suffix style the completion block should use.
#[derive(Clone, Copy)]
enum ToolStatus {
    Success,
    Error,
    Info,
    DiffAdded,
    DiffRemoved,
}

#[derive(Clone)]
struct ToolSuffixSegment {
    text: String,
    status: ToolStatus,
    /// When true, suppress the implicit space the renderer normally
    /// inserts before this segment. Used to glue parts of a multi-span
    /// chip (e.g. the colored `+N/-M` diff stat) into one continuous
    /// run.
    no_leading_space: bool,
}

/// Decomposed tool-call label, painted as themed spans:
/// `<tool_name> <args> <suffix...>`.
#[derive(Clone)]
struct ToolCallDisplay {
    tool_name: String,
    args: String,
    suffixes: Vec<ToolSuffixSegment>,
}

/// Builds the display record for a tool call that is still running.
fn format_tool_call(tool_name: &str, arguments: &CborValue) -> ToolCallDisplay {
    let args = match tool_name {
        "shell" => cbor_text_field(arguments, "command").unwrap_or_default(),
        "read" | "write" | "edit" => cbor_text_field(arguments, "path").unwrap_or_default(),
        "find" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            format!("{pattern} in {path}")
        }
        "grep" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            let mut args = format!("{pattern:?} in {path}");
            if let Some(glob) = cbor_text_field(arguments, "glob") {
                args.push_str(&format!(" [{glob}]"));
            }
            args
        }
        "ls" => cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned()),
        "skill" => cbor_text_field(arguments, "name").unwrap_or_default(),
        _ => String::new(),
    };
    let suffix = running_suffix_after(&args);
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffixes: vec![suffix],
    }
}

fn tool_suffix(text: String, status: ToolStatus) -> ToolSuffixSegment {
    ToolSuffixSegment {
        text,
        status,
        no_leading_space: false,
    }
}

fn info_suffix(text: String) -> ToolSuffixSegment {
    tool_suffix(text, ToolStatus::Info)
}

/// Build the running-call ellipsis with the same leading-space rule
/// [`append_streaming_indicator`] applies: skip the implicit space the
/// renderer would otherwise insert when the preceding text (`args`)
/// already ends in whitespace. Empty `args` keeps the space, since the
/// label preceding the suffix is then the tool name (never whitespace).
fn running_suffix_after(args: &str) -> ToolSuffixSegment {
    let no_leading_space = args.chars().next_back().is_some_and(char::is_whitespace);
    ToolSuffixSegment {
        text: "…".to_owned(),
        status: ToolStatus::Info,
        no_leading_space,
    }
}

fn ok_suffix() -> ToolSuffixSegment {
    tool_suffix("ok".to_owned(), ToolStatus::Success)
}

fn err_suffix(message: Option<&str>) -> ToolSuffixSegment {
    match message {
        Some(msg) if !msg.is_empty() => tool_suffix(format!("err: {msg}"), ToolStatus::Error),
        _ => tool_suffix("err".to_owned(), ToolStatus::Error),
    }
}

/// Append the streaming-progress indicator (`…`) to `text`, prefixing
/// a space only when needed so the indicator doesn't double up
/// whitespace or get stranded one column off the left margin on a
/// fresh line. The space is skipped when `text` is empty or its last
/// character is whitespace (which includes newlines).
fn append_streaming_indicator(text: &mut String) {
    if text.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
        text.push(' ');
    }
    text.push('…');
}

fn output_stats_suffix(text: &str) -> ToolSuffixSegment {
    info_suffix(format!("({}L, {}B)", text.lines().count(), text.len()))
}

/// Error-path display: `<tool_name> <args> <err>`.
fn format_tool_error(tool_name: &str, args: String, error_message: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffixes: vec![err_suffix(Some(error_message))],
    }
}

/// Formats a completed tool call for display.
fn format_tool_completion(
    tool_name: &str,
    details: &CborValue,
    error_message: Option<&str>,
) -> ToolCallDisplay {
    match tool_name {
        "shell" => format_shell_completion(details, error_message),
        "read" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("read", path, msg)
            } else {
                let content = cbor_text_field(details, "content").unwrap_or_default();
                ToolCallDisplay {
                    tool_name: "read".into(),
                    args: path,
                    suffixes: vec![output_stats_suffix(&content), ok_suffix()],
                }
            }
        }
        "write" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("write", path, msg)
            } else {
                // Prefer the colored +N/-M diff chip; fall back to byte
                // count for tools that don't ship a diff (or no-op
                // writes).
                let mut suffixes = format_diff_chip_segments(details).unwrap_or_else(|| {
                    let bytes = cbor_int_field(details, "bytes_written").unwrap_or(0);
                    vec![info_suffix(format!("({bytes}B)"))]
                });
                suffixes.push(ok_suffix());
                ToolCallDisplay {
                    tool_name: "write".into(),
                    args: path,
                    suffixes,
                }
            }
        }
        "edit" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("edit", path, msg)
            } else {
                let mut suffixes = format_diff_chip_segments(details).unwrap_or_else(|| {
                    let count = cbor_int_field(details, "edits_applied").unwrap_or(0);
                    vec![info_suffix(format!("({count} edits applied)"))]
                });
                suffixes.push(ok_suffix());
                ToolCallDisplay {
                    tool_name: "edit".into(),
                    args: path,
                    suffixes,
                }
            }
        }
        "find" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            let pattern = cbor_text_field(details, "pattern").unwrap_or_default();
            let args = format!("{pattern} in {path}");
            if let Some(msg) = error_message {
                format_tool_error("find", args, msg)
            } else {
                let count = cbor_int_field(details, "matches").unwrap_or(0);
                ToolCallDisplay {
                    tool_name: "find".into(),
                    args,
                    suffixes: vec![info_suffix(format!("({count} matches)")), ok_suffix()],
                }
            }
        }
        "grep" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            let pattern = cbor_text_field(details, "pattern").unwrap_or_default();
            let glob = cbor_text_field(details, "glob");
            let args = match glob {
                Some(g) => format!("{pattern:?} in {path} [{g}]"),
                None => format!("{pattern:?} in {path}"),
            };
            if let Some(msg) = error_message {
                format_tool_error("grep", args, msg)
            } else {
                let output = cbor_text_field(details, "output").unwrap_or_default();
                let status = cbor_int_field(details, "status");
                let mut suffixes = vec![output_stats_suffix(&output)];
                suffixes.push(match status {
                    Some(0) => ok_suffix(),
                    Some(1) => tool_suffix("ok: no matches".to_owned(), ToolStatus::Success),
                    Some(code) => err_suffix(Some(&code.to_string())),
                    None => info_suffix("ok?".to_owned()),
                });
                ToolCallDisplay {
                    tool_name: "grep".into(),
                    args,
                    suffixes,
                }
            }
        }
        "ls" => {
            let path = cbor_text_field(details, "path").unwrap_or_else(|| ".".to_owned());
            if let Some(msg) = error_message {
                format_tool_error("ls", path, msg)
            } else {
                let count = cbor_int_field(details, "entries").unwrap_or(0);
                ToolCallDisplay {
                    tool_name: "ls".into(),
                    args: path,
                    suffixes: vec![info_suffix(format!("({count} entries)")), ok_suffix()],
                }
            }
        }
        "skill" => {
            let name = cbor_text_field(details, "name").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("skill", name, msg)
            } else {
                ToolCallDisplay {
                    tool_name: "skill".into(),
                    args: name,
                    suffixes: vec![info_suffix("loaded".to_owned()), ok_suffix()],
                }
            }
        }
        _ => ToolCallDisplay {
            tool_name: tool_name.to_owned(),
            args: String::new(),
            suffixes: vec![match error_message {
                Some(msg) => err_suffix(Some(msg)),
                None => ok_suffix(),
            }],
        },
    }
}

fn format_shell_completion(details: &CborValue, error_message: Option<&str>) -> ToolCallDisplay {
    let cmd = cbor_text_field(details, "command").unwrap_or_default();
    if !matches!(details, CborValue::Map(_)) {
        if let Some(msg) = error_message {
            return format_tool_error("shell", cmd, msg);
        }
    }

    let stdout = cbor_text_field(details, "stdout").unwrap_or_default();
    let stderr = cbor_text_field(details, "stderr").unwrap_or_default();
    let combined = if stdout.is_empty() {
        stderr.clone()
    } else if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };

    let status = cbor_int_field(details, "status");
    let mut suffixes = Vec::new();
    suffixes.push(output_stats_suffix(&combined));
    suffixes.push(match status {
        Some(0) => ok_suffix(),
        Some(code) => err_suffix(Some(&code.to_string())),
        None => info_suffix("ok?".to_owned()),
    });
    ToolCallDisplay {
        tool_name: "shell".into(),
        args: cmd,
        suffixes,
    }
}

/// Paints a [`ToolCallDisplay`] onto a themed block.
fn render_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let name_style = resolve(theme, names::TOOL_NAME);
    let args_style = resolve(theme, names::TOOL_ARGS);

    let mut spans = vec![Span::new(display.tool_name.clone(), name_style)];
    if !display.args.is_empty() {
        spans.push(Span::new(" ", args_style));
        spans.push(Span::new(display.args.clone(), args_style));
    }
    for suffix in &display.suffixes {
        let status_name = match suffix.status {
            ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
            ToolStatus::Error => names::TOOL_STATUS_ERROR,
            ToolStatus::Info => names::TOOL_STATUS_INFO,
            ToolStatus::DiffAdded => names::DIFF_ADDED,
            ToolStatus::DiffRemoved => names::DIFF_REMOVED,
        };
        let status_style = resolve(theme, status_name);
        if !suffix.no_leading_space && !suffix.text.starts_with(':') {
            spans.push(Span::new(" ", args_style));
        }
        spans.push(Span::new(suffix.text.clone(), status_style));
    }
    StyledBlock::new(StyledText::from(spans))
}

/// Like [`render_tool_block`] but appends an expanded unified-diff
/// body when `expanded` is true and `diff` has hunks. The first line
/// is always the existing three-span tool header (with `+N/-M` chip);
/// the body, if rendered, comes after a `\n` so `layout_lines` wraps
/// each diff line independently.
fn render_diff_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    diff: &tau_proto::DiffSummary,
    expanded: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    // Reuse the header from render_tool_block, then keep its spans so
    // we can append diff lines below it.
    let header = render_tool_block(theme, display);
    let mut spans: Vec<Span> = header.content.spans().to_vec();

    if !expanded || diff.hunks.is_empty() {
        return StyledBlock::new(StyledText::from(spans));
    }

    let added_style = resolve(theme, names::DIFF_ADDED);
    let removed_style = resolve(theme, names::DIFF_REMOVED);
    let context_style = resolve(theme, names::DIFF_CONTEXT);
    let header_style = resolve(theme, names::DIFF_HUNK_HEADER);
    let added_inline_style = resolve(theme, names::DIFF_ADDED_INLINE);
    let removed_inline_style = resolve(theme, names::DIFF_REMOVED_INLINE);

    for hunk in &diff.hunks {
        spans.push(Span::new("\n", context_style));
        spans.push(Span::new(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
            header_style,
        ));
        for line in &hunk.lines {
            spans.push(Span::new("\n", context_style));
            match line {
                tau_proto::DiffLine::Equal { text } => {
                    spans.push(Span::new(format!("  {text}"), context_style));
                }
                tau_proto::DiffLine::Add { text } => {
                    spans.push(Span::new(format!("+ {text}"), added_style));
                }
                tau_proto::DiffLine::Remove { text } => {
                    spans.push(Span::new(format!("- {text}"), removed_style));
                }
                tau_proto::DiffLine::Modify { old, new } => {
                    spans.push(Span::new("- ".to_owned(), removed_style));
                    push_segments(&mut spans, old, removed_style, removed_inline_style);
                    spans.push(Span::new("\n".to_owned(), context_style));
                    spans.push(Span::new("+ ".to_owned(), added_style));
                    push_segments(&mut spans, new, added_style, added_inline_style);
                }
            }
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

fn push_segments(
    spans: &mut Vec<tau_cli_term::Span>,
    segments: &[tau_proto::DiffSegment],
    base: tau_cli_term::Style,
    inline: tau_cli_term::Style,
) {
    use tau_cli_term::Span;
    for seg in segments {
        match seg {
            tau_proto::DiffSegment::Equal { text } => {
                spans.push(Span::new(text.clone(), base));
            }
            // Within a Modify line, only the *changed* sub-slice on
            // each side is meaningful. Hide the *other* side's slice
            // so we don't double up (e.g. the - line shouldn't show
            // the new tokens, only the old).
            tau_proto::DiffSegment::Remove { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
            tau_proto::DiffSegment::Add { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
        }
    }
}

/// Render a user `!`/`!!` shell block: a `shell <cmd>` header in the
/// same three-span theme used for tool calls, with streaming output
/// below in the default style.
///
/// `status_suffix`:
///   - `Some("running")` while the command is in-flight (info style),
///   - `Some("[0]")` / `Some("[N]")` on completion (success / error style,
///     keyed off exit code),
///   - `Some("cancelled")` on cancel (info style).
fn render_shell_block(
    theme: &tau_themes::Theme,
    command: &str,
    output: &str,
    status_suffix: Option<&str>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let name_style = resolve(theme, names::TOOL_NAME);
    let args_style = resolve(theme, names::TOOL_ARGS);
    let status_name = match status_suffix {
        Some(s) if s.starts_with("[0]") => names::TOOL_STATUS_SUCCESS,
        Some(s) if s.starts_with('[') => names::TOOL_STATUS_ERROR,
        _ => names::TOOL_STATUS_INFO,
    };
    let status_style = resolve(theme, status_name);

    let mut spans = vec![
        Span::new("shell", name_style),
        Span::new(" ", args_style),
        Span::new(command.to_owned(), args_style),
    ];
    if let Some(suffix) = status_suffix {
        spans.push(Span::new(" ", args_style));
        spans.push(Span::new(suffix.to_owned(), status_style));
    }
    if !output.is_empty() {
        spans.push(Span::new("\n", args_style));
        spans.push(Span::new(output.to_owned(), args_style));
    }
    StyledBlock::new(StyledText::from(spans))
}

/// Event renderer. Maps session_prompt_id → block_id for in-place
/// updates. No flags, no suppression — just ID-based lookups.
fn render_harness_info(
    theme: &tau_themes::Theme,
    info: &tau_proto::HarnessInfo,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_block;
    use tau_themes::names;

    let style_name = match info.level {
        tau_proto::HarnessInfoLevel::Normal => names::SYSTEM_INFO,
        tau_proto::HarnessInfoLevel::Important => names::SYSTEM_INFO_IMPORTANT,
    };
    themed_block(theme, style_name, &info.message)
}

struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    completion_data: tau_cli_term::CompletionData,
    theme: tau_themes::Theme,
    prompt_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Block ID of the last user message (for moving on queue).
    last_user_block: Option<tau_cli_term::BlockId>,
    /// Queued user-message blocks (in above_sticky zone).
    /// When `SessionPromptCreated` fires for a dequeued prompt,
    /// the first entry is popped and moved back to history.
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    /// Live tool-call blocks keyed by call_id. Shown in
    /// above_active while running, moved to history on completion.
    tool_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Live user-shell blocks (from `!`/`!!`) keyed by command_id.
    /// Updated in place as progress chunks arrive, finalized on
    /// `ShellCommandFinished`.
    shell_blocks: HashMap<String, ShellBlockState>,
    /// Live extension blocks keyed by instance_id. Shown in
    /// above_active while starting, moved to history when ready.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, tau_cli_term::BlockId>,
    /// Persistent status bar block showing the current model + effort.
    model_status_block: Option<tau_cli_term::BlockId>,
    /// Live history of completed write/edit blocks plus the data
    /// needed to re-render them. `/diff` flips `diffs_expanded` and
    /// walks this list calling `set_block` so the entire transcript
    /// switches mode at once.
    diff_blocks: Vec<DiffBlockEntry>,
    /// Prompt dispatch timestamps keyed by `session_prompt_id`.
    prompt_started_at: HashMap<String, Instant>,
    /// Global expand-diffs toggle.
    diffs_expanded: bool,
    /// Current model id (cached so we can re-render the status bar
    /// when the effort changes, and vice versa).
    current_model: tau_proto::ModelId,
    /// Current effort. Mirrored into `effort_state` so the
    /// input thread can read it for Shift+Tab cycling.
    current_effort: tau_proto::Effort,
    /// Current model context usage percent. `None` when the context
    /// window is unknown for the selected model.
    current_context_percent: Option<u8>,
    /// Input tokens consumed by the most recent agent response. `None`
    /// until the first usage report for the current model.
    current_context_input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent
    /// response. `None` until the first cache-usage report.
    current_context_cached_tokens: Option<u64>,
    /// Current model context window, in tokens, if known.
    current_context_window: Option<u64>,
    /// End-to-end latency of the most recently completed prompt.
    last_turn_latency: Option<Duration>,
    /// Cache hit rate of the most recently completed prompt.
    last_turn_cache_hit_percent: Option<u8>,
    /// Shared effort mirror for the input thread.
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

/// One completed file-mutation tool block. Held so `/diff` can
/// re-render every diff in the chat history when the global
/// expand toggle flips.
struct DiffBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
    diff: tau_proto::DiffSummary,
}

/// In-flight state for a user `!`/`!!` shell block.
struct ShellBlockState {
    block_id: tau_cli_term::BlockId,
    command: String,
    include_in_context: bool,
    /// Output accumulated from `ShellCommandProgress` chunks. Rendered
    /// under the header each redraw.
    output: String,
}

impl EventRenderer {
    fn new(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
    ) -> Self {
        Self {
            handle,
            completion_data,
            theme,
            prompt_blocks: HashMap::new(),
            last_user_block: None,
            queued_user_blocks: VecDeque::new(),
            tool_blocks: HashMap::new(),
            shell_blocks: HashMap::new(),
            extension_blocks: HashMap::new(),
            model_status_block: None,
            diff_blocks: Vec::new(),
            prompt_started_at: HashMap::new(),
            diffs_expanded: false,
            current_model: tau_proto::ModelId::from(""),
            current_effort: tau_proto::Effort::Off,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_cached_tokens: None,
            current_context_window: None,
            last_turn_latency: None,
            last_turn_cache_hit_percent: None,
            effort_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(effort_to_u8(
                tau_proto::Effort::Off,
            ))),
        }
    }

    /// Returns a clone of the shared effort mirror, used by the
    /// input thread to read the current level for Shift+Tab cycling.
    fn effort_state(&self) -> std::sync::Arc<std::sync::atomic::AtomicU8> {
        self.effort_state.clone()
    }

    /// Flip the global expand-diffs flag and re-render every diff
    /// block in the chat history so the entire transcript switches
    /// mode at once.
    fn toggle_diffs_expanded(&mut self) {
        self.diffs_expanded = !self.diffs_expanded;
        for entry in &self.diff_blocks {
            let block = render_diff_tool_block(
                &self.theme,
                &entry.display,
                &entry.diff,
                self.diffs_expanded,
            );
            self.handle.set_block(entry.block_id, block);
        }
        self.handle.redraw();
    }

    fn render_model_status(&mut self) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        let label = if self.current_model.is_empty() {
            "no model selected".to_string()
        } else {
            let level = if matches!(self.current_effort, tau_proto::Effort::Off) {
                "none".to_owned()
            } else {
                self.current_effort.to_string()
            };
            let context = format_context_chip(
                self.current_context_input_tokens,
                self.current_context_cached_tokens,
                self.current_context_percent,
                self.current_context_window,
            );
            let turn_metrics =
                format_turn_metrics_chip(self.last_turn_latency, self.last_turn_cache_hit_percent);
            format!("{} ({level}){context}{turn_metrics}", self.current_model)
        };
        let block = themed_block(&self.theme, names::MODEL_STATUS, label);
        match self.model_status_block {
            Some(bid) => {
                self.handle.set_block(bid, block);
            }
            None => {
                let bid = self.handle.new_block(block);
                self.handle.push_below(bid);
                self.model_status_block = Some(bid);
            }
        }
        self.handle.redraw();
    }

    fn handle(&mut self, event: &Event) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        match event {
            Event::UiPromptSubmitted(prompt) => {
                let block = themed_block(
                    &self.theme,
                    names::USER_PROMPT,
                    format!("> {}", prompt.text),
                );
                let id = self.handle.print_output(block);
                self.last_user_block = Some(id);
            }
            Event::SessionPromptQueued(queued) => {
                if let Some(id) = self.last_user_block.take() {
                    self.handle.remove_block(id);
                    let block = themed_block(
                        &self.theme,
                        names::USER_PROMPT_QUEUED,
                        format!("> {} (queued)", queued.text),
                    );
                    let queued_id = self.handle.new_block(block);
                    self.handle.push_above_sticky(queued_id);
                    self.handle.redraw();
                    self.queued_user_blocks
                        .push_back((queued_id, queued.text.clone()));
                }
            }
            Event::SessionPromptCreated(prompt) => {
                self.prompt_started_at
                    .insert(prompt.session_prompt_id.to_string(), Instant::now());
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("> {text}"),
                    ));
                }

                let mut pending = String::new();
                append_streaming_indicator(&mut pending);
                let block = themed_block(&self.theme, names::AGENT_PENDING, pending);
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.prompt_blocks
                    .insert(prompt.session_prompt_id.to_string(), id);
            }
            Event::AgentPromptSubmitted(submitted) => {
                self.prompt_started_at
                    .insert(submitted.session_prompt_id.to_string(), Instant::now());
            }
            Event::AgentResponseUpdated(update) => {
                if let Some(&bid) = self.prompt_blocks.get(update.session_prompt_id.as_str()) {
                    let mut text = update.text.clone();
                    append_streaming_indicator(&mut text);
                    let block = themed_block(&self.theme, names::AGENT_RESPONSE, text);
                    self.handle.set_block(bid, block);
                    self.handle.redraw();
                }
            }
            Event::AgentResponseFinished(finished) => {
                self.last_turn_latency = self
                    .prompt_started_at
                    .remove(finished.session_prompt_id.as_str())
                    .map(|started_at| started_at.elapsed());
                self.last_turn_cache_hit_percent =
                    cache_hit_percent(finished.input_tokens, finished.cached_tokens);
                if let Some(bid) = self
                    .prompt_blocks
                    .remove(finished.session_prompt_id.as_str())
                {
                    self.handle.remove_block(bid);

                    let text = finished.text.as_deref().unwrap_or("");
                    if !text.is_empty() {
                        self.handle.print_output(themed_block(
                            &self.theme,
                            names::AGENT_RESPONSE,
                            text,
                        ));
                    }
                }

                for call in &finished.tool_calls {
                    let display = format_tool_call(call.name.as_str(), &call.arguments);
                    let block = render_tool_block(&self.theme, &display);
                    let id = self.handle.new_block(block);
                    self.handle.push_above_active(id);
                    self.tool_blocks.insert(call.id.to_string(), id);
                }
                if !finished.tool_calls.is_empty() {
                    self.handle.redraw();
                }
                self.render_model_status();
            }
            Event::ToolProgress(progress) => {
                if !self.tool_blocks.contains_key(progress.call_id.as_str()) {
                    let text = tau_harness::format_tool_progress(progress);
                    self.handle
                        .print_output(themed_block(&self.theme, names::TOOL_PROGRESS, text));
                }
            }
            Event::ToolResult(result) => {
                if let Some(bid) = self.tool_blocks.remove(result.call_id.as_str()) {
                    self.handle.remove_block(bid);
                }
                let display = format_tool_completion(&result.tool_name, &result.result, None);
                if let Some(diff) = extract_diff(&result.result) {
                    let block =
                        render_diff_tool_block(&self.theme, &display, &diff, self.diffs_expanded);
                    let bid = self.handle.print_output(block);
                    self.diff_blocks.push(DiffBlockEntry {
                        block_id: bid,
                        display,
                        diff,
                    });
                } else {
                    self.handle
                        .print_output(render_tool_block(&self.theme, &display));
                }
            }
            Event::ToolError(error) => {
                if let Some(bid) = self.tool_blocks.remove(error.call_id.as_str()) {
                    self.handle.remove_block(bid);
                }
                let cbor = error.details.as_ref();
                let display = format_tool_completion(
                    &error.tool_name,
                    cbor.unwrap_or(&CborValue::Null),
                    Some(&error.message),
                );
                self.handle
                    .print_output(render_tool_block(&self.theme, &display));
            }
            Event::UiShellCommand(cmd) => {
                // Create a running block now; the harness will echo
                // progress and a finished event back to us via the
                // bus. Both bangs render the same; the context bit
                // just labels the suffix.
                let label = if cmd.include_in_context {
                    "running".to_owned()
                } else {
                    "running [no context]".to_owned()
                };
                let block = render_shell_block(&self.theme, &cmd.command, "", Some(&label));
                let block_id = self.handle.new_block(block);
                self.handle.push_above_active(block_id);
                self.handle.redraw();
                self.shell_blocks.insert(
                    cmd.command_id.to_string(),
                    ShellBlockState {
                        block_id,
                        command: cmd.command.clone(),
                        include_in_context: cmd.include_in_context,
                        output: String::new(),
                    },
                );
            }
            Event::ShellCommandProgress(progress) => {
                if let Some(state) = self.shell_blocks.get_mut(progress.command_id.as_str()) {
                    state.output.push_str(&progress.chunk);
                    let label = if state.include_in_context {
                        "running".to_owned()
                    } else {
                        "running [no context]".to_owned()
                    };
                    let block = render_shell_block(
                        &self.theme,
                        &state.command,
                        &state.output,
                        Some(&label),
                    );
                    self.handle.set_block(state.block_id, block);
                    self.handle.redraw();
                }
            }
            Event::ShellCommandFinished(finished) => {
                let Some(state) = self.shell_blocks.remove(finished.command_id.as_str()) else {
                    return;
                };
                // Use the final, post-truncation output from the
                // extension rather than our streaming buffer so the
                // UI matches what the harness injected into context.
                self.handle.remove_block(state.block_id);
                let suffix = if finished.cancelled {
                    "cancelled".to_owned()
                } else {
                    match finished.exit_code {
                        Some(0) => "[0]".to_owned(),
                        Some(code) => format!("[{code}]"),
                        None => "[?]".to_owned(),
                    }
                };
                let suffix = if state.include_in_context {
                    suffix
                } else {
                    format!("{suffix} [no context]")
                };
                let block = render_shell_block(
                    &self.theme,
                    &finished.command,
                    &finished.output,
                    Some(&suffix),
                );
                self.handle.print_output(block);
            }
            Event::ExtensionStarting(starting) => {
                let block = themed_block(
                    &self.theme,
                    names::EXTENSION_LIFECYCLE,
                    format!("extension {} starting", starting.extension_name),
                );
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.extension_blocks.insert(starting.instance_id, id);
            }
            Event::ExtensionReady(ready) => {
                if let Some(bid) = self.extension_blocks.remove(&ready.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::EXTENSION_LIFECYCLE,
                    format!("extension {} ready", ready.extension_name),
                ));
            }
            Event::ExtensionExited(exited) => {
                if let Some(bid) = self.extension_blocks.remove(&exited.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::EXTENSION_LIFECYCLE,
                    format!("extension {} exited", exited.extension_name),
                ));
            }
            Event::ExtAgentsMdAvailable(agents) => {
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::SYSTEM_INFO,
                    format!("loaded: {}", agents.file_path.display()),
                ));
            }
            Event::ExtensionContextReady(_) => {
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::SYSTEM_INFO,
                    "session context ready".to_owned(),
                ));
            }
            Event::HarnessInfo(info) => {
                self.handle
                    .print_output(render_harness_info(&self.theme, info));
            }
            Event::HarnessModelsAvailable(models) => {
                let items: Vec<tau_cli_term::CompletionItem> = models
                    .models
                    .iter()
                    .map(|m| tau_cli_term::CompletionItem::plain(m.as_str()))
                    .collect();
                self.completion_data
                    .set_arg_completions(tau_cli_term::CommandName::new("/model"), items);
            }
            Event::HarnessModelSelected(selected) => {
                self.current_model = selected.model.clone();
                self.current_context_window = selected.context_window;
                self.last_turn_latency = None;
                self.last_turn_cache_hit_percent = None;
                self.render_model_status();
            }
            Event::HarnessContextUsageChanged(changed) => {
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_cached_tokens = changed.cached_tokens;
                self.current_context_percent = changed.percent_used;
                self.render_model_status();
            }
            Event::HarnessEffortChanged(changed) => {
                self.current_effort = changed.level;
                self.effort_state.store(
                    effort_to_u8(changed.level),
                    std::sync::atomic::Ordering::Relaxed,
                );
                self.render_model_status();
            }
            Event::Osc1337SetUserVar(req) => {
                let in_tmux = std::env::var_os("TMUX").is_some();
                let seq = build_osc1337_set_user_var(&req.name, &req.value, in_tmux);
                self.handle.print_terminal_escape(seq);
            }
            Event::HarnessEffortsAvailable(avail) => {
                let items: Vec<tau_cli_term::CompletionItem> = avail
                    .levels
                    .iter()
                    .map(|l| tau_cli_term::CompletionItem::plain(l.as_str()))
                    .collect();
                self.completion_data
                    .set_arg_completions(tau_cli_term::CommandName::new("/effort"), items);
            }
            Event::LifecycleDisconnect(disconnect) => {
                let reason = disconnect.reason.as_deref().unwrap_or("disconnected");
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::SYSTEM_DISCONNECT,
                    reason,
                ));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

const SAMPLE_CLI: &str = include_str!("../../../config/cli.json5");
const SAMPLE_HARNESS: &str = include_str!("../../../config/harness.json5");
const SAMPLE_MODELS: &str = include_str!("../../../config/models.json5");

fn run_init(force: bool) -> Result<(), CliError> {
    let Some(dir) = tau_config::settings::config_dir() else {
        return Err(CliError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )));
    };
    std::fs::create_dir_all(&dir)?;

    let files = [
        ("cli.json5", SAMPLE_CLI),
        ("harness.json5", SAMPLE_HARNESS),
        ("models.json5", SAMPLE_MODELS),
    ];

    for (name, content) in &files {
        let path = dir.join(name);
        if path.exists() && !force {
            eprintln!(
                "skip: {} (exists, use --force to overwrite)",
                path.display()
            );
        } else {
            std::fs::write(&path, content)?;
            eprintln!("wrote: {}", path.display());
        }
    }

    eprintln!("next: use `tau provider add` to log in to a hosted LLM provider");

    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command.
pub fn main_with_args() -> std::process::ExitCode {
    use std::process::ExitCode;

    use clap::Parser;

    fn run() -> Result<(), CliError> {
        let parsed = cli::Cli::parse();

        let command = parsed.command.unwrap_or(cli::Command::Run {
            resume: None,
            config: None,
            attach: false,
        });

        match command {
            cli::Command::Run {
                resume,
                config: _config,
                attach,
            } => {
                let session_id = if attach {
                    let cwd = std::env::current_dir()?;
                    let daemon_dir =
                        runtime_dir::find_harness_for_dir(&cwd).ok_or(CliError::NoRunningDaemon)?;
                    let daemon_session_id =
                        runtime_dir::read_session_id(&daemon_dir).ok_or_else(|| {
                            CliError::Participant(
                                "running daemon did not publish its session id".to_owned(),
                            )
                        })?;
                    if let Some(requested) = resume.as_deref().filter(|s| !s.is_empty()) {
                        if requested != daemon_session_id {
                            return Err(CliError::Participant(format!(
                                "--attach: daemon is bound to session `{daemon_session_id}`, \
                                 cannot resume `{requested}` (start a fresh daemon for that)"
                            )));
                        }
                    }
                    daemon_session_id
                } else {
                    resolve_run_session_id(resume.as_deref())?
                };
                run_chat(&session_id, attach)
            }

            cli::Command::SessionList { state_dir } => {
                for line in tau_harness::session_list_lines(state_dir)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::SessionShow {
                session_id,
                state_dir,
            } => {
                for line in tau_harness::session_lines(state_dir, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::PolicyShow { state_dir } => {
                for line in tau_harness::policy_lines(state_dir.join("policy.cbor"))? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Init { force } => run_init(force),

            cli::Command::Provider { args } => {
                tau_provider::run(&args).map_err(|e| CliError::Participant(e.to_string()))
            }

            cli::Command::Ext { name } => {
                let runner: fn() -> Result<(), Box<dyn std::error::Error>> = match name.as_str() {
                    "agent" => tau_agent::run_stdio,
                    "ext-shell" => tau_ext_shell::run_stdio,
                    "ext-test-dummy" => tau_ext_test_dummy::run_stdio,
                    "ext-dpc-notifications" => tau_ext_dpc_notifications::run_stdio,
                    "harness" => tau_harness::run_component,
                    _ => {
                        return Err(CliError::Participant(format!(
                            "unknown extension: {name}\navailable: agent, ext-shell, ext-test-dummy, ext-dpc-notifications, harness"
                        )));
                    }
                };
                runner().map_err(|e| CliError::Participant(e.to_string()))
            }
        }
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tau_cli_term::TermHandle;
    use tau_cli_term_raw::Term;
    use tau_proto::{
        AgentResponseFinished, AgentResponseUpdated, CborValue, Event, SessionPromptCreated,
        SessionPromptQueued, ToolResult, UiPromptSubmitted,
    };

    use super::EventRenderer;

    /// Writer that feeds bytes into a vt100::Parser. Bytes are
    /// buffered per-write and flushed atomically to the parser on
    /// flush(), so the test thread never sees a partial render.
    #[derive(Clone)]
    struct VtWriter {
        parser: Arc<Mutex<vt100::Parser>>,
    }

    impl VtWriter {
        fn new(parser: vt100::Parser) -> Self {
            Self {
                parser: Arc::new(Mutex::new(parser)),
            }
        }

        fn screen_text(&self, w: u16) -> Vec<String> {
            self.parser
                .lock()
                .expect("vt")
                .screen()
                .rows(0, w)
                .collect()
        }

        fn screen_contains(&self, w: u16, needle: &str) -> bool {
            self.screen_text(w).iter().any(|r| r.contains(needle))
        }
    }

    impl std::io::Write for VtWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            // Process bytes directly into the parser. The mutex
            // ensures the test thread sees a consistent state.
            self.parser.lock().expect("vt").process(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn setup(w: u16, h: u16) -> (Term, TermHandle, VtWriter) {
        let vt = VtWriter::new(vt100::Parser::new(h, w, 100));
        let (term, handle, _input) = Term::new_virtual(
            w as usize,
            h as usize,
            "> ",
            Box::new(vt.clone()),
            tau_cli_term::CursorShape::Bar,
        );
        (term, handle, vt)
    }

    fn sync(handle: &TermHandle) {
        handle.redraw_sync();
    }

    #[test]
    fn single_prompt_response_cycle() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        // User submits prompt.
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".into(),
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "> hello"));

        // Harness creates session prompt.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "…"));

        // Agent streams response.
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "Hi there!".into(),
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "Hi there!"));

        // Agent finishes.
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("Hi there! How can I help?".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "Hi there! How can I help?"),
            "final response should be visible, got: {:?}",
            vt.screen_text(80)
        );
    }

    #[test]
    fn queued_prompt_renders_after_first_completes() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        // First prompt.
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "first".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));

        // Second prompt queued.
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "second".into(),
        }));
        renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
            session_id: "s1".into(),
            text: "second".into(),
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "second (queued)"),
            "queued indicator should show, got: {:?}",
            vt.screen_text(80)
        );

        // First finishes.
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("response one".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "response one"));

        // Second dispatched — "(queued)" should be removed.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        sync(&handle);
        assert!(
            !vt.screen_contains(80, "(queued)"),
            "queued indicator should be gone after dispatch, got: {:?}",
            vt.screen_text(80)
        );
        assert!(
            vt.screen_contains(80, "> second"),
            "dispatched prompt should show normally, got: {:?}",
            vt.screen_text(80)
        );

        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-1".into(),
            text: "response two".into(),
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "response two"),
            "second response should stream, got: {:?}",
            vt.screen_text(80)
        );

        // Second finishes.
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("response two complete".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "response two complete"),
            "final second response should show, got: {:?}",
            vt.screen_text(80)
        );
        // First response should still be visible.
        assert!(
            vt.screen_contains(80, "response one"),
            "first response should still show, got: {:?}",
            vt.screen_text(80)
        );
    }

    #[test]
    fn three_queued_prompts_render_sequentially() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        // Three rapid prompts.
        for i in 0..3 {
            renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: format!("msg-{i}"),
            }));
            if i == 0 {
                renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                    session_prompt_id: "sp-0".into(),
                    session_id: "s1".into(),
                    system_prompt: String::new(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    model: None,
                    effort: tau_proto::Effort::Off,
                }));
            } else {
                renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
                    session_id: "s1".into(),
                    text: format!("msg-{i}"),
                }));
            }
        }

        // Process all three sequentially, flushing between each.
        for i in 0..3 {
            let spid: tau_proto::SessionPromptId = format!("sp-{i}").into();
            if i > 0 {
                renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                    session_prompt_id: spid.clone(),
                    session_id: "s1".into(),
                    system_prompt: String::new(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    model: None,
                    effort: tau_proto::Effort::Off,
                }));
            }
            renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
                session_prompt_id: spid.clone(),
                text: format!("partial-{i}"),
            }));
            renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: spid,
                text: Some(format!("response-{i}")),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
            }));
            sync(&handle);
        }

        // All three responses should be visible.
        // Extra flush to catch any delayed renders.
        sync(&handle);
        for i in 0..3 {
            assert!(
                vt.screen_contains(80, &format!("response-{i}")),
                "response-{i} should be visible, got: {:?}",
                vt.screen_text(80)
            );
        }
        // No stale "..." blocks.
        assert!(
            !vt.screen_contains(80, "…"),
            "no '…' should remain, got: {:?}",
            vt.screen_text(80)
        );
    }

    #[test]
    fn streaming_indicator_appends_during_updates() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "…"));

        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "Hello".into(),
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "Hello …"));

        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("Hello".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "Hello"));
        assert!(!vt.screen_contains(80, "Hello …"));
    }

    #[test]
    fn running_tool_call_shows_ellipsis_until_result() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: None,
            tool_calls: vec![tau_proto::AgentToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
            }],
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "read src/main.rs …"));

        renderer.handle(&Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: "read".into(),
            result: CborValue::Map(vec![
                (
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                ),
                (
                    CborValue::Text("content".into()),
                    CborValue::Text("fn main() {}\n".into()),
                ),
            ]),
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "read src/main.rs (1L, 13B) ok"));
        assert!(!vt.screen_contains(80, "read src/main.rs …"));
    }

    #[test]
    fn streaming_block_does_not_duplicate_on_finish() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "hello!".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("hello!".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);

        // Count how many rows contain "hello!".
        let count = vt
            .screen_text(80)
            .iter()
            .filter(|r| r.contains("hello!"))
            .count();
        assert_eq!(
            count,
            1,
            "response should appear exactly once, got {count}: {:?}",
            vt.screen_text(80)
        );
    }

    #[test]
    fn grep_completion_uses_output_stats_and_status_chip() {
        let grep_details = CborValue::Map(vec![
            (
                CborValue::Text("pattern".into()),
                CborValue::Text("foo".into()),
            ),
            (CborValue::Text("path".into()), CborValue::Text(".".into())),
            (
                CborValue::Text("status".into()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("output".into()),
                CborValue::Text("a\nb\n".into()),
            ),
        ]);
        let grep = super::format_tool_completion("grep", &grep_details, None);
        assert_eq!(grep.suffixes.len(), 2);
        assert_eq!(grep.suffixes[0].text, "(2L, 4B)");
        assert!(matches!(grep.suffixes[0].status, super::ToolStatus::Info));
        assert_eq!(grep.suffixes[1].text, "ok: no matches");
        assert!(matches!(
            grep.suffixes[1].status,
            super::ToolStatus::Success
        ));

        let grep_ok_details = CborValue::Map(vec![
            (
                CborValue::Text("pattern".into()),
                CborValue::Text("foo".into()),
            ),
            (CborValue::Text("path".into()), CborValue::Text(".".into())),
            (
                CborValue::Text("status".into()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".into()),
                CborValue::Text("a\n".into()),
            ),
        ]);
        let grep_ok = super::format_tool_completion("grep", &grep_ok_details, None);
        assert_eq!(grep_ok.suffixes[1].text, "ok");
        assert!(matches!(
            grep_ok.suffixes[1].status,
            super::ToolStatus::Success
        ));
    }

    #[test]
    fn shell_completion_uses_output_stats_and_exit_color() {
        let shell_details = CborValue::Map(vec![
            (
                CborValue::Text("command".into()),
                CborValue::Text("echo hi".into()),
            ),
            (
                CborValue::Text("stdout".into()),
                CborValue::Text("hi\n".into()),
            ),
            (
                CborValue::Text("stderr".into()),
                CborValue::Text(String::new()),
            ),
            (
                CborValue::Text("status".into()),
                CborValue::Integer(7.into()),
            ),
        ]);
        let shell = super::format_tool_completion("shell", &shell_details, None);
        assert_eq!(shell.suffixes.len(), 2);
        assert_eq!(shell.suffixes[0].text, "(1L, 3B)");
        assert!(matches!(shell.suffixes[0].status, super::ToolStatus::Info));
        assert_eq!(shell.suffixes[1].text, "err: 7");
        assert!(matches!(shell.suffixes[1].status, super::ToolStatus::Error));

        let shell_error = super::format_tool_completion(
            "shell",
            &shell_details,
            Some("command exited with status 7"),
        );
        assert_eq!(shell_error.suffixes.len(), 2);
        assert_eq!(shell_error.suffixes[0].text, "(1L, 3B)");
        assert!(matches!(
            shell_error.suffixes[0].status,
            super::ToolStatus::Info
        ));
        assert_eq!(shell_error.suffixes[1].text, "err: 7");
        assert!(matches!(
            shell_error.suffixes[1].status,
            super::ToolStatus::Error
        ));
    }

    #[test]
    fn edit_completion_uses_path_on_error() {
        let edit_error = super::format_tool_completion(
            "edit",
            &CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("tmp/test-files/test1.txt".into()),
            )]),
            Some("not found"),
        );
        assert_eq!(edit_error.args, "tmp/test-files/test1.txt");
        assert_eq!(edit_error.suffixes.len(), 1);
        assert_eq!(edit_error.suffixes[0].text, "err: not found");
        assert!(matches!(
            edit_error.suffixes[0].status,
            super::ToolStatus::Error
        ));
    }

    #[test]
    fn build_osc1337_set_user_var_encodes_value_and_respects_tmux() {
        let plain = super::build_osc1337_set_user_var("user-notification", "hello", false);
        assert_eq!(plain, "\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07");
        let wrapped = super::build_osc1337_set_user_var("user-notification", "hello", true);
        assert_eq!(
            wrapped,
            "\x1bPtmux;\x1b\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07\x1b\\",
        );
    }

    #[test]
    fn format_context_chip_picks_format_by_known_fields() {
        // Both window and percent known → percent/window chip.
        assert_eq!(
            super::format_context_chip(Some(12_000), Some(9_000), Some(6), Some(200_000)),
            " 6%/200k cache:9k",
        );
        // Window unknown, tokens reported → tokens/? fallback.
        assert_eq!(
            super::format_context_chip(Some(12_000), None, None, None),
            " 12k/?",
        );
        // Window known but no usage report yet still shows the percent
        // (which the harness initialized to 0 on model select).
        assert_eq!(
            super::format_context_chip(None, None, Some(0), Some(200_000)),
            " 0%/200k",
        );
        // Nothing known → empty.
        assert_eq!(super::format_context_chip(None, None, None, None), "");
    }

    #[test]
    fn format_turn_metrics_chip_includes_hit_rate_and_latency() {
        assert_eq!(
            super::format_turn_metrics_chip(Some(Duration::from_millis(1_240)), Some(75)),
            " hit:75% resp:1.2s",
        );
        assert_eq!(
            super::format_turn_metrics_chip(Some(Duration::from_millis(420)), None),
            " resp:420ms",
        );
    }

    #[test]
    fn cache_hit_percent_clamps_to_input_tokens() {
        assert_eq!(super::cache_hit_percent(Some(2_000), Some(1_500)), Some(75));
        assert_eq!(
            super::cache_hit_percent(Some(2_000), Some(3_000)),
            Some(100)
        );
        assert_eq!(super::cache_hit_percent(Some(0), Some(0)), Some(0));
        assert_eq!(super::cache_hit_percent(Some(2_000), None), None);
    }

    #[test]
    fn append_streaming_indicator_handles_each_trailing_case() {
        let cases = [
            ("", "…"),
            ("Hello", "Hello …"),
            ("Hello ", "Hello …"),
            ("Hello\t", "Hello\t…"),
            ("line\n", "line\n…"),
            ("line\n  ", "line\n  …"),
        ];
        for (input, expected) in cases {
            let mut s = input.to_owned();
            super::append_streaming_indicator(&mut s);
            assert_eq!(s, expected, "input was {input:?}");
        }
    }

    #[test]
    fn edit_completion_uses_diff_chip() {
        let edit_details = CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("tmp/test-files/test1.txt".into()),
            ),
            (
                CborValue::Text("diff".into()),
                CborValue::Map(vec![
                    (
                        CborValue::Text("added".into()),
                        CborValue::Integer(2.into()),
                    ),
                    (
                        CborValue::Text("removed".into()),
                        CborValue::Integer(1.into()),
                    ),
                ]),
            ),
        ]);
        let edit = super::format_tool_completion("edit", &edit_details, None);
        assert_eq!(edit.suffixes.len(), 6);
        assert_eq!(edit.suffixes[0].text, "(");
        assert!(matches!(edit.suffixes[0].status, super::ToolStatus::Info));
        assert_eq!(edit.suffixes[1].text, "+2");
        assert!(matches!(
            edit.suffixes[1].status,
            super::ToolStatus::DiffAdded
        ));
        assert!(edit.suffixes[1].no_leading_space);
        assert_eq!(edit.suffixes[2].text, "/");
        assert!(matches!(edit.suffixes[2].status, super::ToolStatus::Info));
        assert!(edit.suffixes[2].no_leading_space);
        assert_eq!(edit.suffixes[3].text, "-1");
        assert!(matches!(
            edit.suffixes[3].status,
            super::ToolStatus::DiffRemoved
        ));
        assert!(edit.suffixes[3].no_leading_space);
        assert_eq!(edit.suffixes[4].text, ")");
        assert!(matches!(edit.suffixes[4].status, super::ToolStatus::Info));
        assert!(edit.suffixes[4].no_leading_space);
        assert_eq!(edit.suffixes[5].text, "ok");
        assert!(matches!(
            edit.suffixes[5].status,
            super::ToolStatus::Success
        ));
    }

    /// Reproduces the user-reported bug: send 3 prompts during the
    /// first response's streaming. After all responses complete, the
    /// prompt must be visible and all 3 responses rendered.
    #[test]
    fn three_prompts_during_streaming_all_render_correctly() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        // User sends first prompt.
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));

        // Agent starts streaming response 1.
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "Hello".into(),
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "Hello"),
            "streaming should show, got: {:?}",
            vt.screen_text(80)
        );

        // User sends 2nd and 3rd prompts while streaming.
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
            session_id: "s1".into(),
            text: "hi".into(),
        }));

        // More streaming updates (multi-line, like a real LLM).
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "Hello!\n\nHow can I help you today?".into(),
        }));
        sync(&handle);

        // Response 1 finishes.
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("Hello!\n\nHow can I help you today?".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "How can I help you today?"),
            "response 1 should be in history, got: {:?}",
            vt.screen_text(80)
        );

        // Second prompt dispatched.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-1".into(),
            text: "Hello again!\n\nHow can I help you?".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("Hello again!\n\nHow can I help you?".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(80, "How can I help you?"),
            "response 2 should be visible, got: {:?}",
            vt.screen_text(80)
        );

        // Third prompt dispatched.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-2".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-2".into(),
            text: "Hi there!\n\nWhat can I help you with?".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-2".into(),
            text: Some("Hi there!\n\nWhat can I help you with?".into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);

        // All three responses should be visible.
        assert!(
            vt.screen_contains(80, "How can I help you today?"),
            "response 1 missing, got: {:?}",
            vt.screen_text(80)
        );
        assert!(
            vt.screen_contains(80, "How can I help you?"),
            "response 2 missing, got: {:?}",
            vt.screen_text(80)
        );
        assert!(
            vt.screen_contains(80, "What can I help you with?"),
            "response 3 missing, got: {:?}",
            vt.screen_text(80)
        );

        // The prompt must be visible at the bottom.
        assert!(
            vt.screen_contains(80, "> "),
            "prompt should be visible after all responses, got: {:?}",
            vt.screen_text(80)
        );

        // No stale streaming blocks should remain.
        assert!(
            !vt.screen_contains(80, "…"),
            "no '…' should remain, got: {:?}",
            vt.screen_text(80)
        );
    }

    /// Emoji (wide characters) in responses must not corrupt the
    /// layout. Each emoji occupies 2 terminal columns; if we count
    /// them as 1, text after the emoji shifts right and wraps
    /// incorrectly.
    #[test]
    fn emoji_in_response_renders_correctly() {
        let (_term, handle, vt) = setup(40, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));

        // Response with emoji followed by text on next line.
        let response = "Hello! 👋\n\nHow can I help you today?";
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: response.into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some(response.into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);

        let text = vt.screen_text(40);

        // "Hello! 👋" should be on its own line, not merged with the
        // next line.
        assert!(
            vt.screen_contains(40, "Hello!"),
            "emoji line missing, got: {:?}",
            text
        );
        // The text after \n\n should start at column 0, not offset.
        assert!(
            text.iter().any(|r| r.starts_with("How can I help")),
            "text after emoji should start at column 0, got: {:?}",
            text
        );
        // Prompt must be visible.
        assert!(
            vt.screen_contains(40, "> "),
            "prompt missing, got: {:?}",
            text
        );
    }

    /// Multiple emoji in a single line must not cause column drift.
    #[test]
    fn multiple_emoji_no_column_drift() {
        let (_term, handle, vt) = setup(40, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));

        // 3 emoji = 6 columns + "end" = 9 columns total.
        let response = "🎉🎊🎈end\nnext line here";
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some(response.into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);

        let text = vt.screen_text(40);
        // "next line here" should start at column 0.
        assert!(
            text.iter().any(|r| r.starts_with("next line here")),
            "line after emoji should start at col 0, got: {:?}",
            text
        );
    }

    /// Replacing a long streaming block with its final settled output
    /// must not leave stale partial lines behind, even when the live
    /// block overflowed the viewport while streaming.
    #[test]
    fn overflowing_stream_replaced_cleanly_on_finish() {
        let (_term, handle, vt) = setup(40, 5);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            tau_themes::Theme::builtin(),
        );

        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "overflow please".into(),
        }));
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-0".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
            effort: tau_proto::Effort::Off,
        }));

        let partial = "stream 0\nstream 1\nstream 2\nstream 3\nPARTIAL ONLY";
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: partial.into(),
        }));
        sync(&handle);
        assert!(
            vt.screen_contains(40, "PARTIAL ONLY"),
            "partial overflowed response should be visible before finish, got: {:?}",
            vt.screen_text(40)
        );

        let final_text = "final 0\nfinal 1\nfinal 2";
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some(final_text.into()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
        }));
        sync(&handle);

        let text = vt.screen_text(40);
        assert!(
            vt.screen_contains(40, "final 0"),
            "final response missing, got: {:?}",
            text
        );
        assert!(
            vt.screen_contains(40, "final 2"),
            "final response tail missing, got: {:?}",
            text
        );
        assert!(
            !vt.screen_contains(40, "PARTIAL ONLY"),
            "stale partial content should be gone, got: {:?}",
            text
        );
        assert!(
            vt.screen_contains(40, "> "),
            "prompt should remain visible, got: {:?}",
            text
        );
    }
}
