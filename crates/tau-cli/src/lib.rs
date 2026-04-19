//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tau_harness::runtime_dir;
use tau_proto::{
    ChatMessage, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleDisconnect,
    LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION,
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

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

struct DaemonHandle {
    child: std::process::Child,
    daemon_dir: PathBuf,
}

impl DaemonHandle {
    fn socket_path(&self) -> PathBuf {
        runtime_dir::socket_path(&self.daemon_dir)
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns a new harness daemon and waits for its socket to be ready.
fn start_daemon() -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;

    let child = Command::new(&tau_binary)
        .arg("component")
        .arg("harness")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;

    let daemon_dir = runtime_dir::root_runtime_dir().join(child.id().to_string());
    let dir_marker = daemon_dir.join("tau.dir");
    let started_at = Instant::now();
    let mut handle = DaemonHandle { child, daemon_dir };

    loop {
        if dir_marker.exists() {
            return Ok(handle);
        }
        if let Some(status) = handle.child.try_wait()? {
            return Err(CliError::DaemonExited(format!("exit status: {status}")));
        }
        if DAEMON_START_TIMEOUT <= started_at.elapsed() {
            return Err(CliError::DaemonStartTimeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Chat as socket client
// ---------------------------------------------------------------------------

fn run_chat(session_id: &str) -> Result<(), CliError> {
    use tau_cli_term::{HighTerm, SlashCommand};

    let daemon = start_daemon()?;
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
            client_name: "tau-chat".to_owned(),
            client_kind: ClientKind::Ui,
        }))
        .map_err(CliError::Encode)?;
    writer
        .write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
            selectors: vec![
                EventSelector::Prefix("message.".to_owned()),
                EventSelector::Prefix("tool.".to_owned()),
                EventSelector::Prefix("extension.".to_owned()),
            ],
        }))
        .map_err(CliError::Encode)?;
    writer.flush()?;

    // Background socket reader — decodes events and sends them to
    // a channel. Runs independently of the main thread.
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let _socket_reader = std::thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(read_stream));
        loop {
            match reader.read_event() {
                Ok(Some(event)) => {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    });

    // Terminal setup.
    let commands = vec![SlashCommand::new("/quit", "Exit the chat session")];
    let (mut term, handle) = HighTerm::new("> ", commands)?;
    handle.redraw();

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    let _renderer = std::thread::spawn(move || {
        while let Ok(event) = renderer_rx.recv() {
            render_event(&renderer_handle, &event);
        }
    });

    // Terminal input loop — owns the writer, no locking needed.
    let result = terminal_input_loop(&mut term, &mut writer, session_id);

    // Send disconnect (best effort).
    let _ = writer.write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
        reason: Some("quit".to_owned()),
    }));
    let _ = writer.flush();

    // Drop the writer (closes the write half) which will cause the
    // socket reader to get EOF and exit. The renderer drains remaining
    // events and exits when the channel closes.
    drop(writer);
    drop(daemon);

    result
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &mut EventWriter<BufWriter<UnixStream>>,
    session_id: &str,
) -> Result<(), CliError> {
    use tau_cli_term::Event as TermEvent;

    loop {
        match term.get_next_event()? {
            TermEvent::Line(line) => {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                if text == "/quit" {
                    break;
                }

                if writer
                    .write_event(&Event::MessageUser(ChatMessage {
                        session_id: Some(session_id.to_owned()),
                        text: text.to_owned(),
                    }))
                    .is_err()
                {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
            TermEvent::Eof => break,
            TermEvent::Resize { .. } | TermEvent::BufferChanged => {}
        }
    }

    Ok(())
}

fn render_event(handle: &tau_cli_term::TermHandle, event: &Event) {
    use tau_cli_term::{Color, Span, Style, StyledBlock, StyledText};

    match event {
        Event::MessageUser(msg) => {
            handle.print_output(
                StyledBlock::new(StyledText::from(vec![
                    Span::new("> ", Style::default().fg(Color::White)),
                    Span::new(&msg.text, Style::default().fg(Color::White)),
                ]))
                .bg(Color::Rgb {
                    r: 40,
                    g: 40,
                    b: 55,
                }),
            );
        }
        Event::MessageAgent(msg) => {
            handle.print_output(
                StyledBlock::new(StyledText::from(vec![
                    Span::new("  ", Style::default().fg(Color::White)),
                    Span::new(&msg.text, Style::default().fg(Color::White)),
                    Span::new("  ", Style::default().fg(Color::White)),
                ]))
                .bg(Color::Rgb {
                    r: 25,
                    g: 35,
                    b: 45,
                }),
            );
        }
        Event::ToolProgress(progress) => {
            let text = tau_harness::format_tool_progress(progress);
            handle.print_output(
                StyledBlock::new(StyledText::from(Span::new(
                    format!("  {text}  "),
                    Style::default().fg(Color::DarkYellow),
                )))
                .bg(Color::Rgb {
                    r: 50,
                    g: 45,
                    b: 20,
                }),
            );
        }
        Event::ExtensionStarting(_)
        | Event::ExtensionReady(_)
        | Event::ExtensionExited(_)
        | Event::ExtensionRestarting(_) => {
            let text = tau_harness::format_extension_event(event);
            handle.print_output(
                StyledBlock::new(StyledText::from(Span::new(
                    format!("  {text}  "),
                    Style::default().fg(Color::DarkGrey),
                )))
                .bg(Color::Rgb {
                    r: 30,
                    g: 30,
                    b: 30,
                }),
            );
        }
        Event::LifecycleDisconnect(disconnect) => {
            let reason = disconnect.reason.as_deref().unwrap_or("disconnected");
            handle.print_output(
                StyledBlock::new(StyledText::from(Span::new(
                    format!("  {reason}  "),
                    Style::default().fg(Color::White),
                )))
                .bg(Color::DarkRed),
            );
        }
        _ => {}
    }
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

        let command = parsed.command.unwrap_or(cli::Command::Chat {
            session_id: tau_harness::default_session_id().to_owned(),
            config: None,
        });

        match command {
            cli::Command::Chat {
                session_id,
                config: _config,
            } => run_chat(&session_id),

            cli::Command::SessionList { session_store } => {
                for line in tau_harness::session_list_lines(session_store)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::SessionShow {
                session_id,
                session_store,
            } => {
                for line in tau_harness::session_lines(session_store, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::PolicyShow { policy_store } => {
                for line in tau_harness::policy_lines(policy_store)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Component { name } => {
                let runner: fn() -> Result<(), Box<dyn std::error::Error>> = match name.as_str() {
                    "agent" => tau_agent::run_stdio,
                    "ext-fs" => tau_ext_fs::run_stdio,
                    "ext-shell" => tau_ext_shell::run_stdio,
                    "harness" => tau_harness::run_component,
                    _ => {
                        return Err(CliError::Participant(format!(
                            "unknown component: {name}\navailable: agent, ext-fs, ext-shell, harness"
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
