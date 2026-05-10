//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

mod ui_logging;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tau_config::settings::CliBindingAction;
use tau_harness::{SessionLaunchStatus, runtime_dir};

fn encode_binding_action(action: &CliBindingAction) -> String {
    if action.command.is_empty() {
        return action.action.clone();
    }
    format!(
        "{}:{}:{}",
        action.action,
        if action.trim { "trim" } else { "raw" },
        action.command
    )
}

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

use tau_proto::{
    CborValue, ClientKind, Disconnect, Event, EventSelector, Frame, FrameReader, FrameWriter,
    Hello, Message, PROTOCOL_VERSION, Subscribe, UiPromptDraft, UiPromptSubmitted,
};

/// Shared writer handle: the input loop and the prompt-draft debounce
/// thread both need to send events on the same socket. Stream
/// `write()` calls are atomic only up to `PIPE_BUF` (~4 KB on
/// AF_UNIX) so we serialize whole-event writes through a `Mutex`
/// instead of risking a long draft burst interleaving with a
/// `UiPromptSubmitted` mid-byte. Contention is essentially zero —
/// debounce fires at most once per second per typing burst.
type WriterHandle = Arc<Mutex<FrameWriter<BufWriter<UnixStream>>>>;

/// Lock the writer, write one frame and flush. Returns the underlying
/// `io::Error` on failure so callers can use `?` or discard with
/// `let _ = …`.
fn send_frame(writer: &WriterHandle, frame: &Frame) -> io::Result<()> {
    let mut w = writer.lock().expect("writer mutex poisoned");
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
struct DraftSlot {
    pending: Option<(u64, UiPromptDraft)>,
    epoch: u64,
    done: bool,
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
            let mut g = mtx.lock().expect("draft slot mutex poisoned");
            while g.pending.is_none() && !g.done {
                g = cv.wait(g).expect("draft slot mutex poisoned");
            }
            if g.done && g.pending.is_none() {
                return;
            }
            g.pending.take()
        };
        if let Some((epoch, draft)) = snapshot {
            if should_send_draft_snapshot(handle.as_ref(), epoch) {
                // Best-effort: a write failure means the socket is gone,
                // and the input loop will notice on its next write.
                let _ = send_event(&writer, &Event::UiPromptDraft(draft));
            }
        }
        // Coalesce subsequent typing into one event per window. Wake
        // early on shutdown so we don't spend a second sleeping after
        // the user already typed `/quit`.
        let g = mtx.lock().expect("draft slot mutex poisoned");
        let (g, _timed_out) = cv
            .wait_timeout_while(g, DRAFT_DEBOUNCE, |s| !s.done)
            .expect("draft slot mutex poisoned");
        if g.done && g.pending.is_none() {
            return;
        }
    }
}

fn should_send_draft_snapshot(handle: &(Mutex<DraftSlot>, Condvar), epoch: u64) -> bool {
    let (mtx, _cv) = handle;
    let g = mtx.lock().expect("draft slot mutex poisoned");
    !g.done && g.epoch == epoch
}

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
    "Tau-tau control.",
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
        // `tau -a` back in later.
    }
}

/// Resolves a harness daemon to talk to, either by attaching to an
/// existing one for this project or by spawning a fresh one.
///
/// The fresh-spawn path passes `session_id` to the daemon via the
/// `TAU_SESSION_ID` env var, so its eager-init targets the right session.
/// Resolves the session id for one `tau` invocation.
///
/// - `None` → mint `<basename(cwd)>-<rand6>`.
/// - `Some("")` (bare `-r`) → resume the most recent session whose
///   `meta.json.cwd` matches cwd; if none, mint fresh.
/// - `Some(id)` → resume that explicit id.
fn resolve_run_session_id(resume: Option<&str>) -> Result<(String, SessionLaunchStatus), CliError> {
    let cwd = std::env::current_dir()?;
    match resume {
        None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        Some("") => match find_most_recent_session(&cwd) {
            Some(id) => Ok((id, SessionLaunchStatus::Resumed)),
            None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        },
        Some(id) => Ok((id.to_owned(), SessionLaunchStatus::Resumed)),
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

struct DaemonOutput {
    stdout: Stdio,
    stderr: Stdio,
    log_path: PathBuf,
    start_offset: u64,
}

fn resolve_daemon(
    attach: bool,
    session_id: &str,
    session_status: SessionLaunchStatus,
    daemon_output: Option<DaemonOutput>,
) -> Result<DaemonHandle, CliError> {
    tracing::debug!(target: "tau_cli::startup", attach, session_id, "resolving harness daemon");
    let project_root = std::env::current_dir()?;
    if attach {
        tracing::debug!(target: "tau_cli::startup", project_root = %project_root.display(), "looking for existing harness daemon");
        let daemon_dir =
            runtime_dir::find_harness_for_dir(&project_root).ok_or(CliError::NoRunningDaemon)?;
        tracing::debug!(target: "tau_cli::startup", daemon_dir = %daemon_dir.display(), "attached harness daemon resolved");
        return Ok(DaemonHandle::Attached { daemon_dir });
    }
    start_daemon(
        session_id,
        session_status,
        daemon_output.expect("daemon output for spawned harness"),
    )
}

fn build_revision() -> String {
    match (built_info::GIT_COMMIT_HASH_SHORT, built_info::GIT_DIRTY) {
        (Some(hash), Some(true)) => format!("{hash}-modified"),
        (Some(hash), _) => hash.to_owned(),
        _ => "unknown".to_owned(),
    }
}

fn build_last_modified() -> Option<String> {
    option_env!("TAU_LAST_MODIFIED")
        .filter(|date| !date.is_empty())
        .map(str::to_owned)
        .or_else(|| short_built_time(built_info::BUILT_TIME_UTC))
        .filter(|date| date != "1980-01-01 00:00")
}

fn short_built_time(time: &str) -> Option<String> {
    let input_format = time::macros::format_description!(
        "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    let output_format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::parse(time, input_format)
        .ok()?
        .format(output_format)
        .ok()
}

fn build_label_parts() -> (String, String) {
    let version = format!("tau {}", env!("CARGO_PKG_VERSION"));
    let build = match build_last_modified() {
        Some(date) => format!("({}, {})", build_revision(), date),
        None => format!("({})", build_revision()),
    };
    (version, build)
}

fn version_label() -> String {
    let (version, build) = build_label_parts();
    format!("{version} {build}")
}

fn display_path(path: &Path) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.display().to_string();
    };
    let home = Path::new(&home);
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    let Ok(suffix) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if suffix.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", suffix.display())
    }
}

fn read_daemon_output_since(path: &Path, start_offset: u64) -> io::Result<String> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut output = String::new();
    file.read_to_string(&mut output)?;
    Ok(output)
}

/// Spawns a new harness daemon and waits for its socket to be ready.
fn start_daemon(
    session_id: &str,
    session_status: SessionLaunchStatus,
    output: DaemonOutput,
) -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;
    tracing::debug!(target: "tau_cli::startup", tau_binary = %tau_binary.display(), session_id, "spawning harness daemon");

    let mut child = Command::new(&tau_binary)
        .arg("ext")
        .arg("harness")
        .env("TAU_SESSION_ID", session_id)
        .env("TAU_SESSION_STATUS", session_status.as_str())
        .env("TAU_VERSION", env!("CARGO_PKG_VERSION"))
        .env("TAU_BUILD", build_revision())
        .envs(build_last_modified().map(|date| ("TAU_LAST_MODIFIED", date)))
        // Default-enable harness startup debug in the child process so
        // `tau` captures timing without requiring an env var. Users
        // can still override/filter with `TAU_LOG`.
        .env(
            "TAU_LOG",
            std::env::var("TAU_LOG")
                .unwrap_or_else(|_| "tau_harness::startup=debug,tau_cli=info".to_owned()),
        )
        .stdin(Stdio::null())
        .stdout(output.stdout)
        .stderr(output.stderr)
        .spawn()?;

    tracing::debug!(target: "tau_cli::startup", pid = child.id(), "harness daemon spawned");
    let daemon_dir = runtime_dir::root_runtime_dir().join(child.id().to_string());
    let dir_marker = daemon_dir.join("tau.dir");
    let started_at = Instant::now();

    loop {
        if dir_marker.exists() {
            tracing::debug!(target: "tau_cli::startup", pid = child.id(), daemon_dir = %daemon_dir.display(), elapsed_ms = started_at.elapsed().as_millis(), "harness daemon marker observed");
            return Ok(DaemonHandle::Owned {
                child: Some(child),
                daemon_dir,
            });
        }
        if let Some(status) = child.try_wait()? {
            tracing::debug!(target: "tau_cli::startup", pid = child.id(), %status, elapsed_ms = started_at.elapsed().as_millis(), "harness daemon exited before marker");
            let captured = read_daemon_output_since(&output.log_path, output.start_offset)?;
            let mut message = format!("exit status: {status}");
            if !captured.trim().is_empty() {
                message.push_str("\n\nHarness output:\n");
                message.push_str(captured.trim_end());
            }
            return Err(CliError::DaemonExited(message));
        }
        if DAEMON_START_TIMEOUT <= started_at.elapsed() {
            tracing::debug!(target: "tau_cli::startup", pid = child.id(), elapsed_ms = started_at.elapsed().as_millis(), "harness daemon start timed out");
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

fn run_chat(
    session_id: &str,
    attach: bool,
    session_status: SessionLaunchStatus,
) -> Result<(), CliError> {
    use tau_cli_term::{HighTerm, SlashCommand};

    let state_dir = tau_harness::default_state_dir();
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
        let start_offset = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ui_logging.log_path())?
            .metadata()?
            .len();
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ui_logging.log_path())
            .map(Stdio::from)?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ui_logging.log_path())
            .map(Stdio::from)?;
        Some(DaemonOutput {
            stdout,
            stderr,
            log_path: ui_logging.log_path().to_owned(),
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
    // `RendererCmd::Local` variants (e.g. `/diff` toggles) into the
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
                        Frame::Event(event) => RendererCmd::Remote(event),
                        Frame::Message(Message::Disconnect(d)) => {
                            RendererCmd::RemoteDisconnect(d.reason)
                        }
                        Frame::Message(_) => continue,
                    };
                    if socket_event_tx.send(cmd).is_err() {
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
        SlashCommand::new("/cancel", "Cancel the current in-flight prompt"),
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
            "/show-diff",
            "Toggle expanded vs compact display of file edit diffs",
        ),
        SlashCommand::new(
            "/show-cache-stats",
            "Toggle provider prompt-cache hit stats in the status bar",
        ),
        SlashCommand::new(
            "/show-thinking",
            "Toggle visibility of the agent's reasoning summary blocks",
        ),
    ];
    let theme = tau_themes::Theme::builtin();
    let settings = tau_config::settings::load_cli_settings().unwrap_or_default();
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
        use tau_cli_term::{StyledBlock, StyledText};
        use tau_themes::names;

        let logo = tau_cli_term::resolve::resolve(&theme, names::BANNER_LOGO);
        let name = tau_cli_term::resolve::resolve(&theme, names::BANNER_NAME);
        let version_style = tau_cli_term::resolve::resolve(&theme, names::BANNER_VERSION);
        let build_style = tau_cli_term::resolve::resolve(&theme, names::BANNER_BUILD);
        let pun_style = tau_cli_term::resolve::resolve(&theme, names::BANNER_PUN);
        let pun = random_startup_pun();
        let (version, build) = build_label_parts();
        let banner = StyledText::from(vec![
            tau_cli_term::Span::new("▀█▀▀ ", logo),
            tau_cli_term::Span::new("tau", name),
            tau_cli_term::Span::new(version.trim_start_matches("tau"), version_style),
            tau_cli_term::Span::new(" ", Default::default()),
            tau_cli_term::Span::new(build, build_style),
            tau_cli_term::Span::new("\n", Default::default()),
            tau_cli_term::Span::new(" █▄▖ ", logo),
            tau_cli_term::Span::new(pun, pun_style),
        ]);
        handle.print_output(StyledBlock::new(banner));
    }
    handle.print_output(ui_dir_block(&theme, ui_logging.dir()));

    handle.redraw();

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    let renderer_completion_data = completion_data;
    // Pre-build the renderer so we can grab its `effort_state`
    // handle for the input loop's Shift+Tab cycle. Load the
    // persisted `cli.json` state so toggles like `/show-diff`,
    // `/show-thinking`, and `/show-cache-stats` survive restarts.
    let dirs = tau_config::settings::TauDirs::default();
    let cli_state = tau_config::settings::CliState::load(&dirs);
    let renderer = EventRenderer::new_with_state(
        renderer_handle,
        renderer_completion_data,
        theme.clone(),
        cli_state,
        dirs,
        settings.submitted_prompt_symbol,
    );
    let effort_state = renderer.effort_state();
    let editor_context = renderer.editor_context();
    term.set_editor_context_handle(editor_context.clone());
    let _renderer = std::thread::spawn(move || {
        let mut renderer = renderer;
        while let Ok(cmd) = renderer_rx.recv() {
            match cmd {
                RendererCmd::Remote(event) => renderer.handle(&event),
                RendererCmd::RemoteDisconnect(reason) => renderer.handle_disconnect(reason),
                RendererCmd::ToggleDiffs => renderer.toggle_diffs_expanded(),
                RendererCmd::ToggleThinking => renderer.toggle_thinking_visible(),
                RendererCmd::ToggleCacheStats => renderer.toggle_cache_stats_visible(),
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
        effort_state,
        theme,
        event_tx,
        editor_context,
        &draft_handle,
    )?;

    // Tell the debounce thread to exit and wait for it so we don't
    // race with the disconnect below (the thread might otherwise
    // emit one final draft on the closing socket and trip an `EPIPE`).
    {
        let (mtx, cv) = &*draft_handle;
        let mut g = mtx.lock().expect("draft slot mutex poisoned");
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
    /// `/show-thinking` toggle.
    ToggleThinking,
    ToggleCacheStats,
    Remote(Event),
    /// The harness sent a `Disconnect` message over the wire.
    RemoteDisconnect(Option<String>),
    ToggleDiffs,
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &WriterHandle,
    session_id: &mut String,
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    theme: tau_themes::Theme,
    renderer_tx: std::sync::mpsc::Sender<RendererCmd>,
    editor_context: std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>>,
    draft_handle: &DraftHandle,
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
                if let Ok(mut context) = editor_context.lock() {
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
                    let new_id = mint_session_id(&cwd);
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
                if text == "/show-diff" {
                    let _ = renderer_tx.send(RendererCmd::ToggleDiffs);
                    continue;
                }
                if text == "/show-thinking" {
                    let _ = renderer_tx.send(RendererCmd::ToggleThinking);
                    continue;
                }
                if text == "/show-cache-stats" {
                    let _ = renderer_tx.send(RendererCmd::ToggleCacheStats);
                    continue;
                }
                if let Some(model) = text.strip_prefix("/model ") {
                    let model = model.trim();
                    if !model.is_empty() {
                        let _ = send_event(
                            writer,
                            &Event::UiModelSelect(tau_proto::UiModelSelect {
                                model: model.into(),
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

                // Submission terminates the in-flight draft window —
                // the buffer just got cleared by the user pressing
                // Enter, so any pending draft is now stale. Invalidate
                // before sending the submission so a debounce thread that
                // already took an older snapshot can't emit it afterward.
                {
                    let (mtx, cv) = &**draft_handle;
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
                let (mtx, cv) = &**draft_handle;
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
            TermEvent::BackTab => {
                // Pi-style: cycle effort. Read the current
                // level from the shared atomic the renderer keeps in
                // sync with `HarnessEffortChanged`, advance,
                // send the request. The harness echoes back and the
                // renderer updates the status block.
                let current =
                    effort_from_u8(effort_state.load(std::sync::atomic::Ordering::Relaxed));
                let next = current.next();
                let _ = send_event(
                    writer,
                    &Event::UiSetEffort(tau_proto::UiSetEffort { level: next }),
                );
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
/// - context window known → `" ctx:{percent}%/{window}"` (e.g. `"
///   ctx:6%/200k"`)
/// - window unknown but token count reported → `" ctx:{tokens}/?"`
/// - nothing known yet → empty string (chip suppressed)
fn format_context_chip(
    input_tokens: Option<u64>,
    percent: Option<u8>,
    window: Option<u64>,
) -> String {
    match (window, percent, input_tokens) {
        (Some(w), Some(p), _) => format!(" ctx:{p}%/{}", format_token_count(w)),
        // Window not configured — fall back to raw token count so the
        // user can see usage exists and add `contextWindow` to fix it.
        (None, _, Some(t)) => format!(" ctx:{}/?", format_token_count(t)),
        _ => String::new(),
    }
}

fn format_cache_hit_chip(input_tokens: Option<u64>, cached_tokens: Option<u64>) -> String {
    match (cache_hit_percent(input_tokens, cached_tokens), input_tokens) {
        (Some(percent), Some(tokens)) => {
            format!(" hit:{percent}%/{}", format_token_count(tokens))
        }
        _ => String::new(),
    }
}

fn format_turn_metrics_chip(latency: Option<Duration>) -> String {
    let mut chip = String::new();
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

/// Returns the originator of any prompt-lifecycle event, or
/// [`tau_proto::PromptOriginator::User`] for events that don't carry
/// one (so unrelated events render as before).
fn originator_of(event: &Event) -> tau_proto::PromptOriginator {
    match event {
        Event::UiPromptSubmitted(p) => p.originator.clone(),
        Event::SessionPromptCreated(p) => p.originator.clone(),
        Event::AgentPromptSubmitted(s) => s.originator.clone(),
        Event::AgentResponseUpdated(u) => u.originator.clone(),
        Event::AgentResponseFinished(f) => f.originator.clone(),
        _ => tau_proto::PromptOriginator::User,
    }
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
    writer: &WriterHandle,
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
    send_event(
        writer,
        &Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: session_id.into(),
            command_id: command_id.into(),
            command: command.to_owned(),
            include_in_context,
        }),
    )
    .map_err(|_| ())
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

fn cbor_bool_field(value: &CborValue, key: &str) -> Option<bool> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let (CborValue::Text(k), CborValue::Bool(b)) = (k, v) {
                if k == key {
                    return Some(*b);
                }
            }
        }
    }
    None
}

fn cbor_array_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a [CborValue]> {
    if let CborValue::Map(entries) = value {
        for (k, v) in entries {
            if let (CborValue::Text(k), CborValue::Array(arr)) = (k, v) {
                if k == key {
                    return Some(arr.as_slice());
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
    Progress,
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
        "delegate" => match cbor_text_field(arguments, "task_name") {
            Some(name) if !name.is_empty() => format!("[{name}]"),
            _ => String::new(),
        },
        "skill" => match cbor_text_field(arguments, "action").as_deref() {
            Some("search") => {
                let query = cbor_text_field(arguments, "query").unwrap_or_default();
                let scope = if cbor_bool_field(arguments, "search_content").unwrap_or(false) {
                    " [content]"
                } else {
                    ""
                };
                format!("search: {query}{scope}")
            }
            // Default to load semantics for `action: "load"` and for
            // legacy / malformed calls without an action.
            _ => cbor_text_field(arguments, "name").unwrap_or_default(),
        },
        _ => String::new(),
    };
    let suffix = running_suffix_after(&args);
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffixes: vec![suffix],
    }
}

/// Builds the running display for a `delegate` call once the harness
/// has reported sub-agent state via `DelegateProgress`. Renders to:
/// `delegate [task_name] ctx: 38%/200k tools: 2/3 …`.
fn format_delegate_progress(
    args: String,
    progress: &tau_proto::DelegateProgress,
) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    if progress.ctx_percent.is_some() || progress.ctx_window.is_some() {
        suffixes.push(info_suffix(format_ctx_label(
            progress.ctx_percent,
            progress.ctx_window,
        )));
    }
    let tools_completed = progress
        .tools_total
        .saturating_sub(progress.tools_in_flight);
    suffixes.push(info_suffix(format!(
        "tools: {}/{}",
        tools_completed, progress.tools_total,
    )));
    suffixes.push(running_suffix_after("x")); // non-empty so a leading space is preserved
    ToolCallDisplay {
        tool_name: "delegate".to_owned(),
        args,
        suffixes,
    }
}

/// Builds the completion display for a finished `delegate` call.
/// Renders to:
/// `delegate [task_name] ctx: 38%/200k tools: 3 (5L, 220B) ok`
/// (or with `err: …` when `error_message` is set). The chip order
/// mirrors the in-progress line — `ctx:` then `tools:` — so the
/// transition from running to done shifts only the trailing segments
/// (`…` → `(NL, NB) ok`) instead of shuffling existing ones around.
fn format_delegate_completion(
    args: String,
    last_progress: Option<&tau_proto::DelegateProgress>,
    details: &CborValue,
    error_message: Option<&str>,
) -> ToolCallDisplay {
    let response_text = match details {
        CborValue::Text(text) => text.as_str(),
        _ => "",
    };
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    // Match the in-progress order (`ctx:` → `tools:`) so the line
    // doesn't visibly reorder when it transitions to done.
    if let Some(progress) = last_progress {
        if progress.ctx_percent.is_some() || progress.ctx_window.is_some() {
            suffixes.push(info_suffix(format_ctx_label(
                progress.ctx_percent,
                progress.ctx_window,
            )));
        }
        // Show a single number on completion since "in flight" is
        // always zero by the time the result lands; total is the
        // bit the user cares about.
        suffixes.push(info_suffix(format!("tools: {}", progress.tools_total)));
    }
    if !response_text.is_empty() {
        suffixes.push(output_stats_suffix(response_text));
    }
    suffixes.push(match error_message {
        Some(msg) if !msg.is_empty() => err_suffix(Some(msg)),
        _ => ok_suffix(),
    });
    ToolCallDisplay {
        tool_name: "delegate".to_owned(),
        args,
        suffixes,
    }
}

/// Renders the `ctx: ` chip for a `DelegateProgress` snapshot. Falls
/// back to whichever side of (`%`, `window`) is known.
fn format_ctx_label(percent: Option<u8>, window: Option<u64>) -> String {
    let percent_part = percent
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "?".to_owned());
    let window_part = window.map(format_window).unwrap_or_default();
    if window_part.is_empty() {
        format!("ctx: {percent_part}")
    } else {
        format!("ctx: {percent_part}/{window_part}")
    }
}

/// Compact rendering of a context-window size. `200000` -> `200k`,
/// `1_048_576` -> `1.0M`. Approximate; for surfacing alongside a `%`.
fn format_window(window: u64) -> String {
    if window >= 1_000_000 {
        let m = window as f64 / 1_000_000.0;
        format!("{m:.1}M")
    } else if window >= 1_000 {
        format!("{}k", window / 1_000)
    } else {
        window.to_string()
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
        status: ToolStatus::Progress,
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

/// Build a streaming block whose body uses `body_name` styling and
/// whose trailing `…` indicator uses [`names::PROGRESS_INDICATOR`], so
/// the indicator can be themed independently. The leading space before
/// the indicator is skipped when the body is empty or already ends in
/// whitespace, so the `…` doesn't double up whitespace or land one
/// column off the left margin on a fresh line.
fn streaming_block(
    theme: &tau_themes::Theme,
    body_name: &str,
    body_text: impl Into<String>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::{convert_color, resolve};
    use tau_cli_term::{Span, Style, StyledBlock, StyledText};
    use tau_themes::{StyleName, names};

    let body_text = body_text.into();
    let needs_space = body_text
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());

    let body_ts = theme.resolve_style(&StyleName::new(body_name));
    let body_span_style = Style {
        fg: body_ts.fg.map(convert_color),
        bg: None,
        bold: body_ts.bold,
        underline: body_ts.underline,
        italic: body_ts.italic,
    };
    let progress_style = resolve(theme, names::PROGRESS_INDICATOR);

    let mut spans = Vec::with_capacity(3);
    if !body_text.is_empty() {
        spans.push(Span::new(body_text, body_span_style));
    }
    if needs_space {
        spans.push(Span::new(" ", body_span_style));
    }
    spans.push(Span::new("…".to_owned(), progress_style));

    let mut block = StyledBlock::new(StyledText::from(spans));
    if let Some(bg) = body_ts.bg {
        block = block.bg(convert_color(bg));
    }
    block
}

fn output_stats_suffix(text: &str) -> ToolSuffixSegment {
    stats_suffix(None, text)
}

fn stats_suffix(prefix: Option<String>, text: &str) -> ToolSuffixSegment {
    let mut parts = Vec::new();
    if let Some(prefix) = prefix {
        parts.push(prefix);
    }
    parts.push(format!("{}L", text.lines().count()));
    parts.push(format!("{}B", text.len()));
    info_suffix(format!("({})", parts.join(", ")))
}

fn websearch_stats_suffix(text: &str) -> ToolSuffixSegment {
    let titles = text
        .lines()
        .filter(|line| line.starts_with("Title:"))
        .count();
    let urls = text.lines().filter(|line| line.starts_with("URL:")).count();
    let results = if titles < urls { urls } else { titles };
    if 0 < results {
        info_suffix(format!(
            "({results} results, {}L, {}B)",
            text.lines().count(),
            text.len()
        ))
    } else {
        output_stats_suffix(text)
    }
}

/// Render the `queries` array from a skill search/load-error result
/// as a single space-separated string. Non-text entries are skipped
/// rather than failing the render.
fn format_skill_query_list(queries: &[CborValue]) -> String {
    let mut out = String::new();
    for q in queries {
        if let CborValue::Text(s) = q {
            if !s.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s);
            }
        }
    }
    out
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
                let output = cbor_text_field(details, "output").unwrap_or_default();
                ToolCallDisplay {
                    tool_name: "find".into(),
                    args,
                    suffixes: vec![stats_suffix(None, &output), ok_suffix()],
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
                let match_count = cbor_int_field(details, "matches").unwrap_or(0);
                let mut suffixes = vec![stats_suffix(Some(format!("{match_count}M")), &output)];
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
        "websearch_exa" => {
            if let Some(msg) = error_message {
                format_tool_error("websearch_exa", String::new(), msg)
            } else {
                let text = match details {
                    CborValue::Text(text) => text.as_str(),
                    _ => "",
                };
                ToolCallDisplay {
                    tool_name: "websearch_exa".into(),
                    args: String::new(),
                    suffixes: vec![websearch_stats_suffix(text), ok_suffix()],
                }
            }
        }
        // `delegate` is rendered by `format_delegate_completion`,
        // which has access to the cached task name + last progress
        // snapshot. This match arm is unreachable for the running
        // delegate path; if a synthetic result somehow flows through
        // the generic fallback it will land in the catch-all below.
        "skill" => {
            // Distinguish search vs load by the result shape: search
            // results carry `queries` + `matches`; load successes
            // carry `name` + `content`; load failures carry `name`
            // plus a search-shaped `queries` + `matches` echo built
            // from the requested name's word-like tokens.
            let queries = cbor_array_field(details, "queries")
                .map(format_skill_query_list)
                .filter(|s| !s.is_empty());
            let load_name = cbor_text_field(details, "name");
            let match_count = cbor_array_field(details, "matches")
                .map(<[CborValue]>::len)
                .unwrap_or(0);
            let scope = if cbor_bool_field(details, "search_content").unwrap_or(false) {
                " [content]"
            } else {
                ""
            };
            match (queries, load_name.clone(), error_message) {
                // Failed load with auto-search hint. Show the
                // requested name as args and surface the suggestion
                // count next to the error so the user can tell at a
                // glance whether to expect useful follow-ups.
                (Some(_), Some(name), Some(msg)) => ToolCallDisplay {
                    tool_name: "skill".into(),
                    args: name,
                    suffixes: vec![
                        info_suffix(format!("({match_count} suggestions)")),
                        err_suffix(Some(msg)),
                    ],
                },
                // Plain skill search (success or error).
                (Some(query_str), _, error_message) => {
                    let args = format!("search: {query_str}{scope}");
                    if let Some(msg) = error_message {
                        format_tool_error("skill", args, msg)
                    } else {
                        ToolCallDisplay {
                            tool_name: "skill".into(),
                            args,
                            suffixes: vec![info_suffix(format!("({match_count}L)")), ok_suffix()],
                        }
                    }
                }
                // Plain skill load (success or non-suggestion error).
                (None, _, error_message) => {
                    let name = load_name.unwrap_or_default();
                    if let Some(msg) = error_message {
                        format_tool_error("skill", name, msg)
                    } else {
                        let content = cbor_text_field(details, "content").unwrap_or_default();
                        ToolCallDisplay {
                            tool_name: "skill".into(),
                            args: name,
                            suffixes: vec![output_stats_suffix(&content), ok_suffix()],
                        }
                    }
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
    let suffixes = vec![
        output_stats_suffix(&combined),
        match status {
            Some(0) => ok_suffix(),
            Some(code) => err_suffix(Some(&code.to_string())),
            None => info_suffix("ok?".to_owned()),
        },
    ];
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
    use tau_cli_term::StyledBlock;
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let mut themed = ThemedText::new();
    let output = themed.add_style(names::TOOL_OUTPUT);
    let name = themed.add_style(names::TOOL_NAME);
    let args = themed.add_style(names::TOOL_ARGS);

    let mut children = vec![SpanTree::span(
        name,
        vec![SpanTree::text(display.tool_name.clone())],
    )];
    if !display.args.is_empty() {
        children.push(SpanTree::span(
            args,
            vec![SpanTree::text(" "), SpanTree::text(display.args.clone())],
        ));
    }
    for suffix in &display.suffixes {
        let status_name = match suffix.status {
            ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
            ToolStatus::Error => names::TOOL_STATUS_ERROR,
            ToolStatus::Info => names::TOOL_STATUS_INFO,
            ToolStatus::Progress => names::PROGRESS_INDICATOR,
            ToolStatus::DiffAdded => names::DIFF_ADDED,
            ToolStatus::DiffRemoved => names::DIFF_REMOVED,
        };
        let status = themed.add_style(status_name);
        if !suffix.no_leading_space && !suffix.text.starts_with(':') {
            children.push(SpanTree::span(args, vec![SpanTree::text(" ")]));
        }
        children.push(SpanTree::span(
            status,
            vec![SpanTree::text(suffix.text.clone())],
        ));
    }
    themed.push_tree(SpanTree::span(output, children));

    StyledBlock::new(themed_text(theme, &themed))
}

/// Like [`render_tool_block`] but appends an expanded unified-diff
/// body when `expanded` is true and `diff` has hunks. The first line
/// is the themed tool header (with `+N/-M` chip); the body, if
/// rendered, comes after a `\n` so `layout_lines` wraps each diff line
/// independently.
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

    if info.level == tau_proto::HarnessInfoLevel::Normal {
        if let Some(path) = info
            .message
            .strip_prefix("session dir: ")
            .and_then(|path| path.strip_suffix('/'))
        {
            return system_path_block(theme, "session dir: ", Path::new(path), "/");
        }
    }

    let style_name = match info.level {
        tau_proto::HarnessInfoLevel::Normal => names::SYSTEM_INFO,
        tau_proto::HarnessInfoLevel::Important => names::SYSTEM_INFO_IMPORTANT,
    };
    themed_block(theme, style_name, &info.message)
}

fn ui_dir_block(theme: &tau_themes::Theme, path: &Path) -> tau_cli_term::StyledBlock {
    system_path_block(theme, "ui dir: ", path, "/")
}

fn session_status_block(
    theme: &tau_themes::Theme,
    path: &Path,
    suffix: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::SESSION_STATUS);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(lifecycle, "session dir: ");
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn system_path_block(
    theme: &tau_themes::Theme,
    prefix: &str,
    path: &Path,
    suffix: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(info, prefix);
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn system_loaded_block(
    theme: &tau_themes::Theme,
    path: &Path,
    content: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    let stats_style = text.add_style(names::TOOL_STATUS_INFO);
    text.push(info, "loaded: ");
    text.push(path_style, display_path(path));
    text.push(info, " ");
    text.push(stats_style, output_stats_suffix(content).text);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn system_status_block(
    theme: &tau_themes::Theme,
    prefix: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let status_style = text.add_style(names::SYSTEM_STATUS);
    text.push(info, prefix);
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn extension_status_block(
    theme: &tau_themes::Theme,
    extension_name: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::EXTENSION_STATUS);
    text.push(lifecycle, "extension ");
    text.push(lifecycle, extension_name);
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    completion_data: tau_cli_term::CompletionData,
    theme: tau_themes::Theme,
    prompt_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Live thinking blocks keyed by `session_prompt_id`. Lazy-created
    /// the first time the agent emits non-empty `thinking` for the
    /// prompt, so backends that don't return reasoning summaries
    /// produce no extra block.
    thinking_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Latest thinking text per live prompt, captured during streaming
    /// so `AgentResponseFinished` can render it into history.
    thinking_text: HashMap<String, String>,
    /// Block ID of the last user message (for moving on queue).
    last_user_block: Option<tau_cli_term::BlockId>,
    /// Queued user-message blocks (in above_sticky zone).
    /// When `SessionPromptCreated` fires for a dequeued prompt,
    /// the first entry is popped and moved back to history.
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    /// Live tool-call blocks keyed by call_id. Shown in
    /// above_active while running, moved to history on completion.
    tool_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Sticky args (e.g. `[task_name]`) for tool calls whose live
    /// block needs to be re-rendered on subsequent progress events
    /// — only `delegate` for now. The original `arguments` aren't
    /// carried on `DelegateProgress`, so we cache the display label
    /// when the block is first created.
    delegate_block_args: HashMap<String, String>,
    /// Most recent `DelegateProgress` snapshot per delegate call.
    /// On `ToolResult` we render the completion line with the final
    /// `ctx: …` / `tools: …` chips so the user sees the delegation
    /// cost alongside the response stats.
    delegate_last_progress: HashMap<String, tau_proto::DelegateProgress>,
    /// Tool call ids issued by sub-agents (side conversations). Their
    /// lifecycle events (`ToolResult`, `ToolError`, `ToolProgress`)
    /// share the bus with the main agent's, but the UI filters them
    /// out: sub-agent activity is rolled up into the parent's
    /// `delegate` block via `DelegateProgress` instead. Populated when
    /// the side conv's `AgentResponseFinished` is observed; entries
    /// removed on the matching result/error.
    sub_agent_call_ids: std::collections::HashSet<String>,
    /// Live user-shell blocks (from `!`/`!!`) keyed by command_id.
    /// Updated in place as progress chunks arrive, finalized on
    /// `ShellCommandFinished`.
    shell_blocks: HashMap<String, ShellBlockState>,
    /// Live extension blocks keyed by instance_id. Shown in
    /// above_active while starting, moved to history when ready.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, tau_cli_term::BlockId>,
    /// Extensions that are already up in this daemon. `/new` starts a
    /// fresh session, but these processes are intentionally kept.
    ready_extensions: HashSet<String>,
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
    /// Global show-thinking toggle. When false, agent reasoning
    /// summaries are not rendered (live or in history). Toggled by
    /// `/show-thinking`; persisted in `<state_dir>/cli.json`.
    show_thinking: bool,
    /// Persisted thinking blocks (one per finished assistant turn).
    /// When `/show-thinking` flips, every entry is re-rendered as
    /// either the full text or removed, so the toggle takes effect
    /// retroactively across the visible transcript.
    thinking_history: Vec<ThinkingBlockEntry>,
    /// Where to persist `show_diff` / `show_thinking` /
    /// `show_cache_stats` toggles.
    state_dirs: tau_config::settings::TauDirs,
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
    /// Whether to render provider prompt-cache hit stats in the
    /// model status bar.
    show_cache_stats: bool,
    /// End-to-end latency of the most recently completed prompt.
    last_turn_latency: Option<Duration>,
    /// Shared effort mirror for the input thread.
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Context appended to files opened by the external prompt editor.
    editor_context: std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>>,
    /// Symbol shown before submitted prompts in the transcript.
    submitted_prompt_symbol: String,
}

/// One completed file-mutation tool block. Held so `/diff` can
/// re-render every diff in the chat history when the global
/// expand toggle flips.
struct DiffBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
    diff: tau_proto::DiffSummary,
}

/// One finished thinking block. Held so `/show-thinking` can swap
/// its content between the original reasoning text (visible) and
/// empty content (hidden) without losing the block's position in
/// the transcript.
struct ThinkingBlockEntry {
    block_id: tau_cli_term::BlockId,
    text: String,
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
    #[cfg(test)]
    fn new(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
    ) -> Self {
        // Tests pass a state_dir of None so toggles never touch the
        // user's real `~/.local/state/tau/cli.json`.
        Self::new_with_state(
            handle,
            completion_data,
            theme,
            tau_config::settings::CliState::default(),
            tau_config::settings::TauDirs {
                config_dir: None,
                state_dir: None,
            },
            ">".to_string(),
        )
    }

    fn new_with_state(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
        state: tau_config::settings::CliState,
        state_dirs: tau_config::settings::TauDirs,
        submitted_prompt_symbol: String,
    ) -> Self {
        Self {
            handle,
            completion_data,
            theme,
            prompt_blocks: HashMap::new(),
            thinking_blocks: HashMap::new(),
            thinking_text: HashMap::new(),
            last_user_block: None,
            queued_user_blocks: VecDeque::new(),
            tool_blocks: HashMap::new(),
            delegate_block_args: HashMap::new(),
            delegate_last_progress: HashMap::new(),
            sub_agent_call_ids: std::collections::HashSet::new(),
            shell_blocks: HashMap::new(),
            extension_blocks: HashMap::new(),
            ready_extensions: HashSet::new(),
            model_status_block: None,
            diff_blocks: Vec::new(),
            prompt_started_at: HashMap::new(),
            diffs_expanded: state.show_diff,
            show_thinking: state.show_thinking,
            show_cache_stats: state.show_cache_stats,
            thinking_history: Vec::new(),
            state_dirs,
            current_model: tau_proto::ModelId::from(""),
            current_effort: tau_proto::Effort::Off,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_cached_tokens: None,
            current_context_window: None,
            last_turn_latency: None,
            effort_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(effort_to_u8(
                tau_proto::Effort::Off,
            ))),
            editor_context: std::sync::Arc::new(std::sync::Mutex::new(
                tau_cli_term::EditorContext::default(),
            )),
            submitted_prompt_symbol,
        }
    }

    fn save_cli_state(&self) {
        tau_config::settings::CliState {
            show_diff: self.diffs_expanded,
            show_thinking: self.show_thinking,
            show_cache_stats: self.show_cache_stats,
        }
        .save(&self.state_dirs);
    }

    fn editor_context(&self) -> std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>> {
        self.editor_context.clone()
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
        // Past diff blocks may have already scrolled out of the
        // visible window. Force a full repaint so the toggle takes
        // effect retroactively across scrollback.
        self.handle.invalidate_screen();
        self.save_cli_state();
    }

    /// Flip the global show-thinking flag and re-render every prior
    /// thinking block in the transcript so the toggle takes effect
    /// retroactively (full text when on, empty content when off).
    /// Live in-flight thinking blocks are also flipped. New turns
    /// continue to be gated by the same flag.
    ///
    /// Empty content is used instead of `remove_block` so the
    /// block's position in the transcript is preserved; toggling
    /// back on restores the original reasoning text in place.
    fn toggle_thinking_visible(&mut self) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        self.show_thinking = !self.show_thinking;
        for entry in &self.thinking_history {
            let display = if self.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.handle.set_block(
                entry.block_id,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        for (spid, &bid) in &self.thinking_blocks {
            let display = if self.show_thinking {
                self.thinking_text.get(spid).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            self.handle.set_block(
                bid,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        // Past thinking blocks may already be in terminal scrollback.
        // Force a full repaint so the toggle takes effect there too.
        self.handle.invalidate_screen();
        self.save_cli_state();
    }

    /// Flip provider prompt-cache hit stats in the status bar.
    fn toggle_cache_stats_visible(&mut self) {
        self.show_cache_stats = !self.show_cache_stats;
        self.render_model_status();
        self.save_cli_state();
    }

    /// Clears all session-scoped UI state and re-renders an empty
    /// transcript. Persistent user preferences such as `/show-diff`
    /// and `/show-thinking` are intentionally preserved.
    fn clear_for_new_session(&mut self) {
        self.prompt_blocks.clear();
        self.thinking_blocks.clear();
        self.thinking_text.clear();
        self.last_user_block = None;
        self.queued_user_blocks.clear();
        self.tool_blocks.clear();
        self.delegate_block_args.clear();
        self.delegate_last_progress.clear();
        self.sub_agent_call_ids.clear();
        self.shell_blocks.clear();
        self.extension_blocks.clear();
        self.model_status_block = None;
        self.diff_blocks.clear();
        self.prompt_started_at.clear();
        self.thinking_history.clear();
        // Model selection and effort are harness-global, not
        // session-scoped. `/new` only causes a SessionStarted event;
        // the harness does not re-emit HarnessModelSelected for the
        // unchanged model. Keep the cached selection so the status bar
        // can be recreated after clearing the terminal output.
        self.current_context_percent = None;
        self.current_context_input_tokens = None;
        self.current_context_cached_tokens = None;
        self.last_turn_latency = None;
        self.handle.clear_output();
        self.render_session_preamble();
        if !self.current_model.is_empty() {
            self.render_model_status();
        }
    }

    fn render_session_preamble(&mut self) {
        use tau_cli_term::{StyledBlock, StyledText};
        use tau_themes::names;

        let logo = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_LOGO);
        let name = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_NAME);
        let version_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_VERSION);
        let build_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_BUILD);
        let pun_style = tau_cli_term::resolve::resolve(&self.theme, names::BANNER_PUN);
        let pun = random_startup_pun();
        let (version, build) = build_label_parts();
        let banner = StyledText::from(vec![
            tau_cli_term::Span::new("▀█▀▀ ", logo),
            tau_cli_term::Span::new("tau", name),
            tau_cli_term::Span::new(version.trim_start_matches("tau"), version_style),
            tau_cli_term::Span::new(" ", Default::default()),
            tau_cli_term::Span::new(build, build_style),
            tau_cli_term::Span::new("\n", Default::default()),
            tau_cli_term::Span::new(" █▄▖ ", logo),
            tau_cli_term::Span::new(pun, pun_style),
        ]);
        self.handle.print_output(StyledBlock::new(banner));
        let mut extensions: Vec<_> = self.ready_extensions.iter().collect();
        extensions.sort();
        for extension_name in extensions {
            self.handle
                .print_output(extension_status_block(&self.theme, extension_name, "kept"));
        }
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
                self.current_context_percent,
                self.current_context_window,
            );
            let cache = if self.show_cache_stats {
                format_cache_hit_chip(
                    self.current_context_input_tokens,
                    self.current_context_cached_tokens,
                )
            } else {
                String::new()
            };
            let turn_metrics = format_turn_metrics_chip(self.last_turn_latency);
            format!(
                "{} ({level}){context}{cache}{turn_metrics}",
                self.current_model
            )
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

    fn submitted_prompt_prefix(&self) -> String {
        format!("{} ", self.submitted_prompt_symbol)
    }

    fn handle_disconnect(&mut self, reason: Option<String>) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        let reason = reason.as_deref().unwrap_or("disconnected");
        self.handle
            .print_output(themed_block(&self.theme, names::SYSTEM_DISCONNECT, reason));
    }

    fn handle(&mut self, event: &Event) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        // Side-conversation `AgentResponseFinished` events get filtered
        // out by `originator_of(event).is_user()` below — but we still
        // need to learn which `call_id`s those side conversations
        // emit, so we can suppress the matching `ToolResult` /
        // `ToolError` / `ToolProgress` (which carry no originator) on
        // their way past. Otherwise sub-agent tool activity would
        // leak into the user's transcript.
        if let Event::AgentResponseFinished(finished) = event
            && !finished.originator.is_user()
        {
            for call in &finished.tool_calls {
                self.sub_agent_call_ids.insert(call.id.to_string());
            }
        }

        // Skip events that belong to a side conversation spawned by an
        // extension (e.g. the std-notifications idle-summarizer). They
        // travel on the same bus as the user's interactive turn but
        // must not paint into the user's chat window or perturb its
        // pending-block bookkeeping.
        if !originator_of(event).is_user() {
            return;
        }

        match event {
            Event::SessionStarted(started)
                if matches!(started.reason, tau_proto::SessionStartReason::New) =>
            {
                self.clear_for_new_session();
            }
            Event::UiPromptSubmitted(prompt) => {
                let block = themed_block(
                    &self.theme,
                    names::USER_PROMPT,
                    format!("{}{}", self.submitted_prompt_prefix(), prompt.text),
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
                        format!("{}{} (queued)", self.submitted_prompt_prefix(), queued.text),
                    );
                    let queued_id = self.handle.new_block(block);
                    self.handle.push_above_sticky(queued_id);
                    self.handle.redraw();
                    self.queued_user_blocks
                        .push_back((queued_id, queued.text.clone()));
                }
            }
            Event::SessionPromptSteered(steered) => {
                // The harness folded a queued prompt into the current
                // turn's next round (alongside tool results) instead of
                // waiting for `Idle`. Promote the "(queued)" rendering
                // to a regular user prompt so the transcript reads
                // naturally above the agent's continuing response.
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{text}", self.submitted_prompt_prefix()),
                    ));
                    self.handle.redraw();
                } else {
                    // No matching "(queued)" block — fall back to
                    // rendering the steered text directly so the user
                    // still sees their message land.
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{}", self.submitted_prompt_prefix(), steered.text),
                    ));
                    self.handle.redraw();
                }
            }
            Event::SessionPromptCreated(prompt) => {
                if prompt.originator.is_user()
                    && let Ok(mut context) = self.editor_context.lock()
                {
                    context.active_prompt = None;
                }
                self.prompt_started_at
                    .insert(prompt.session_prompt_id.to_string(), Instant::now());
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("{}{text}", self.submitted_prompt_prefix()),
                    ));
                }

                let block = streaming_block(&self.theme, names::AGENT_PENDING, "");
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
                let spid = update.session_prompt_id.as_str();

                if update.originator.is_user()
                    && let Ok(mut context) = self.editor_context.lock()
                {
                    context.active_prompt = if update.text.is_empty() {
                        None
                    } else {
                        Some(update.text.clone())
                    };
                }

                // Thinking is its own block, lazy-created the first
                // time non-empty summary content arrives. Rendered
                // above the response block (in `above_active`). Always
                // accumulate the text so the toggle can flip on
                // retroactively, but only paint the live block when
                // `show_thinking` is on.
                if let Some(thinking) = update.thinking.as_deref()
                    && !thinking.is_empty()
                {
                    self.thinking_text
                        .insert(spid.to_owned(), thinking.to_owned());
                    if self.show_thinking {
                        let block = streaming_block(&self.theme, names::AGENT_THINKING, thinking);
                        if let Some(&tbid) = self.thinking_blocks.get(spid) {
                            self.handle.set_block(tbid, block);
                        } else {
                            // Insert the thinking block ABOVE the
                            // pending response block in `above_active`.
                            // The response block was pushed first
                            // (in SessionPromptCreated), so a plain
                            // push would land below it. Briefly
                            // remove the response, push thinking,
                            // re-push response — net effect: thinking
                            // is at the response's old position and
                            // the response moves down by one.
                            let tbid = self.handle.new_block(block);
                            if let Some(&response_bid) = self.prompt_blocks.get(spid) {
                                self.handle.remove_above_active(response_bid);
                                self.handle.push_above_active(tbid);
                                self.handle.push_above_active(response_bid);
                            } else {
                                self.handle.push_above_active(tbid);
                            }
                            self.thinking_blocks.insert(spid.to_owned(), tbid);
                        }
                        self.handle.redraw();
                    }
                }

                if let Some(&bid) = self.prompt_blocks.get(spid) {
                    let block =
                        streaming_block(&self.theme, names::AGENT_RESPONSE, update.text.clone());
                    self.handle.set_block(bid, block);
                    self.handle.redraw();
                }
            }
            Event::AgentResponseFinished(finished) => {
                let spid = finished.session_prompt_id.as_str();
                self.last_turn_latency = self
                    .prompt_started_at
                    .remove(spid)
                    .map(|started_at| started_at.elapsed());

                // Finalize the thinking block above the response.
                // Prefer the finished event's payload if it carries
                // one; fall back to whatever streaming captured.
                let thinking = finished
                    .thinking
                    .clone()
                    .or_else(|| self.thinking_text.remove(spid));
                if let Some(tbid) = self.thinking_blocks.remove(spid) {
                    self.handle.remove_block(tbid);
                }
                if self.show_thinking
                    && let Some(thinking) = thinking.filter(|t| !t.is_empty())
                {
                    let bid = self.handle.print_output(themed_block(
                        &self.theme,
                        names::AGENT_THINKING,
                        thinking.clone(),
                    ));
                    self.thinking_history.push(ThinkingBlockEntry {
                        block_id: bid,
                        text: thinking,
                    });
                }
                self.thinking_text.remove(spid);

                if let Some(bid) = self.prompt_blocks.remove(spid) {
                    self.handle.remove_block(bid);

                    let text = finished.text.as_deref().unwrap_or("");
                    if !text.is_empty() {
                        if finished.originator.is_user()
                            && let Ok(mut context) = self.editor_context.lock()
                        {
                            context.last_agent_response = Some(text.to_owned());
                            context.active_prompt = None;
                        }
                        self.handle.print_output(themed_block(
                            &self.theme,
                            names::AGENT_RESPONSE,
                            text,
                        ));
                    }
                }

                // Only the main agent's tool calls land in the UI as
                // their own blocks. Sub-agent (side conversation) tool
                // activity is summarized live under the parent's
                // `delegate` block via `DelegateProgress` instead, so
                // the user sees one line per delegation rather than
                // a flood of nested invocations.
                if finished.originator.is_user() {
                    for call in &finished.tool_calls {
                        let display = format_tool_call(call.name.as_str(), &call.arguments);
                        let block = render_tool_block(&self.theme, &display);
                        let id = self.handle.new_block(block);
                        self.handle.push_above_active(id);
                        self.tool_blocks.insert(call.id.to_string(), id);
                        // Cache the rendered args (`[task_name]`) for
                        // later `DelegateProgress` updates: those
                        // events don't carry the original tool
                        // arguments, only task_name + counters.
                        if call.name.as_str() == "delegate" && !display.args.is_empty() {
                            self.delegate_block_args
                                .insert(call.id.to_string(), display.args.clone());
                        }
                    }
                    if !finished.tool_calls.is_empty() {
                        self.handle.redraw();
                    }
                }
                self.render_model_status();
            }
            Event::ToolProgress(progress) => {
                if self.sub_agent_call_ids.contains(progress.call_id.as_str()) {
                    return;
                }
                if !self.tool_blocks.contains_key(progress.call_id.as_str()) {
                    let text = tau_harness::format_tool_progress(progress);
                    self.handle
                        .print_output(themed_block(&self.theme, names::SHELL_OUTPUT, text));
                }
            }
            Event::ToolDelegateProgress(progress) => {
                let call_id = progress.call_id.as_str();
                // Snapshot the latest counters and ctx info regardless
                // of whether the block is still live; the `ToolResult`
                // handler reuses them on the completion line.
                self.delegate_last_progress
                    .insert(call_id.to_owned(), progress.clone());
                let Some(&bid) = self.tool_blocks.get(call_id) else {
                    // Block already torn down (delegate finished or
                    // never rendered) — nothing to update.
                    return;
                };
                let args = self
                    .delegate_block_args
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        if progress.task_name.is_empty() {
                            String::new()
                        } else {
                            format!("[{}]", progress.task_name)
                        }
                    });
                let display = format_delegate_progress(args, progress);
                let block = render_tool_block(&self.theme, &display);
                self.handle.set_block(bid, block);
            }
            Event::ToolResult(result) => {
                let call_id = result.call_id.as_str();
                // Sub-agent tool activity stays out of the user's
                // transcript — its progress is rolled up under the
                // parent's `delegate` block by `DelegateProgress`.
                if self.sub_agent_call_ids.remove(call_id) {
                    self.delegate_block_args.remove(call_id);
                    return;
                }
                if let Some(bid) = self.tool_blocks.remove(call_id) {
                    self.handle.remove_block(bid);
                }
                let args = self.delegate_block_args.remove(call_id);
                let last_progress = self.delegate_last_progress.remove(call_id);
                let display = if result.tool_name.as_str() == "delegate" {
                    format_delegate_completion(
                        args.unwrap_or_default(),
                        last_progress.as_ref(),
                        &result.result,
                        None,
                    )
                } else {
                    format_tool_completion(&result.tool_name, &result.result, None)
                };
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
                let call_id = error.call_id.as_str();
                if self.sub_agent_call_ids.remove(call_id) {
                    self.delegate_block_args.remove(call_id);
                    self.delegate_last_progress.remove(call_id);
                    return;
                }
                if let Some(bid) = self.tool_blocks.remove(call_id) {
                    self.handle.remove_block(bid);
                }
                let args = self.delegate_block_args.remove(call_id);
                let last_progress = self.delegate_last_progress.remove(call_id);
                let cbor = error.details.as_ref();
                let display = if error.tool_name.as_str() == "delegate" {
                    format_delegate_completion(
                        args.unwrap_or_default(),
                        last_progress.as_ref(),
                        cbor.unwrap_or(&CborValue::Null),
                        Some(&error.message),
                    )
                } else {
                    format_tool_completion(
                        &error.tool_name,
                        cbor.unwrap_or(&CborValue::Null),
                        Some(&error.message),
                    )
                };
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
                let block =
                    extension_status_block(&self.theme, &starting.extension_name, "starting");
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.extension_blocks.insert(starting.instance_id, id);
            }
            Event::ExtensionReady(ready) => {
                if let Some(bid) = self.extension_blocks.remove(&ready.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.ready_extensions
                    .insert(ready.extension_name.to_string());
                self.handle.print_output(extension_status_block(
                    &self.theme,
                    &ready.extension_name,
                    "ready",
                ));
            }
            Event::ExtensionExited(exited) => {
                if let Some(bid) = self.extension_blocks.remove(&exited.instance_id) {
                    self.handle.remove_block(bid);
                }
                self.ready_extensions.remove(exited.extension_name.as_str());
                self.handle.print_output(extension_status_block(
                    &self.theme,
                    &exited.extension_name,
                    "exited",
                ));
            }
            Event::ExtAgentsMdAvailable(agents) => {
                self.handle.print_output(system_loaded_block(
                    &self.theme,
                    &agents.file_path,
                    &agents.content,
                ));
            }
            Event::ExtensionContextReady(_) => {
                self.handle.print_output(system_status_block(
                    &self.theme,
                    "session context ",
                    "ready",
                ));
            }
            Event::HarnessInfo(info) => {
                self.handle
                    .print_output(render_harness_info(&self.theme, info));
            }
            Event::HarnessSessionDir(session_dir) => {
                self.handle.print_output(session_status_block(
                    &self.theme,
                    &session_dir.path,
                    "/",
                    session_dir.status.as_str(),
                ));
            }
            Event::HarnessUiDir(ui_dir) => {
                self.handle
                    .print_output(ui_dir_block(&self.theme, &ui_dir.path));
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
        let cli::Cli {
            version,
            run,
            command,
        } = cli::Cli::parse();
        if version {
            println!("{}", version_label());
            return Ok(());
        }

        let command = command.unwrap_or(cli::Command::Run(run));

        match command {
            cli::Command::Run(cli::RunArgs {
                resume,
                config: _config,
                attach,
            }) => {
                let (session_id, session_status) = if attach {
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
                    (daemon_session_id, SessionLaunchStatus::Resumed)
                } else {
                    resolve_run_session_id(resume.as_deref())?
                };
                run_chat(&session_id, attach, session_status)
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

            cli::Command::Dev { command } => match command {
                cli::DevCommand::DumpInitialPrompt { out, message } => {
                    tau_harness::dump_initial_prompt(&out, &message)?;
                    println!("wrote {}", out.display());
                    Ok(())
                }
            },

            cli::Command::Ext { name } => {
                ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info");
                let runner: fn() -> Result<(), Box<dyn std::error::Error>> = match name.as_str() {
                    "agent" => tau_agent::run_stdio,
                    "ext-shell" => tau_ext_shell::run_stdio,
                    "ext-test-dummy" => tau_ext_test_dummy::run_stdio,
                    "ext-core-delegate" => tau_ext_core_delegate::run_stdio,
                    "ext-std-notifications" => tau_ext_std_notifications::run_stdio,
                    "ext-websearch-exa" => tau_ext_websearch_exa::run_stdio,
                    "harness" => tau_harness::run_component,
                    _ => {
                        return Err(CliError::Participant(format!(
                            "unknown extension: {name}\navailable: agent, ext-shell, ext-test-dummy, ext-core-delegate, ext-std-notifications, ext-websearch-exa, harness"
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
mod tests;
