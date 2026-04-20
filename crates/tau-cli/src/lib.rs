//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tau_harness::runtime_dir;
use tau_proto::{
    ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleDisconnect,
    LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION, UiPromptSubmitted,
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
                EventSelector::Prefix("ui.".to_owned()),
                EventSelector::Prefix("session.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
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
        let mut renderer = EventRenderer::new(renderer_handle);
        while let Ok(event) = renderer_rx.recv() {
            renderer.handle(&event);
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
                    .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                        session_id: session_id.to_owned(),
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

/// Event renderer. Maps session_prompt_id → block_id for in-place
/// updates. No flags, no suppression — just ID-based lookups.
struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    prompt_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Block ID of the last user message (for restyling on queue).
    last_user_block: Option<tau_cli_term::BlockId>,
}

impl EventRenderer {
    fn new(handle: tau_cli_term::TermHandle) -> Self {
        Self {
            handle,
            prompt_blocks: HashMap::new(),
            last_user_block: None,
        }
    }

    fn handle(&mut self, event: &Event) {
        use tau_cli_term::{Color, Span, Style, StyledBlock, StyledText};

        match event {
            Event::UiPromptSubmitted(prompt) => {
                // Render immediately — if queued, SessionPromptQueued
                // will re-style it.
                let id = self.handle.print_output(
                    StyledBlock::new(StyledText::from(Span::new(
                        format!("> {}", prompt.text),
                        Style::default().fg(Color::White),
                    )))
                    .bg(Color::Rgb {
                        r: 40,
                        g: 40,
                        b: 55,
                    }),
                );
                // Track by session_id so SessionPromptQueued can
                // restyle it.
                self.last_user_block = Some(id);
            }
            Event::SessionPromptQueued(queued) => {
                // Restyle the user message block to show it's queued.
                if let Some(id) = self.last_user_block.take() {
                    let block = StyledBlock::new(StyledText::from(Span::new(
                        format!("> {} (queued)", queued.text),
                        Style::default().fg(Color::DarkGrey),
                    )))
                    .bg(Color::Rgb {
                        r: 35,
                        g: 35,
                        b: 40,
                    });
                    self.handle.set_block(id, block);
                    self.handle.redraw();
                }
            }
            Event::SessionPromptCreated(prompt) => {
                let block = StyledBlock::new(StyledText::from(Span::new(
                    "...",
                    Style::default().fg(Color::DarkGrey),
                )))
                .bg(Color::Rgb {
                    r: 25,
                    g: 35,
                    b: 45,
                });
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.prompt_blocks
                    .insert(prompt.session_prompt_id.clone(), id);
            }
            Event::AgentResponseUpdated(update) => {
                if let Some(&bid) = self.prompt_blocks.get(&update.session_prompt_id) {
                    let block = StyledBlock::new(StyledText::from(Span::new(
                        &update.text,
                        Style::default().fg(Color::White),
                    )))
                    .bg(Color::Rgb {
                        r: 25,
                        g: 35,
                        b: 45,
                    });
                    self.handle.set_block(bid, block);
                    self.handle.redraw();
                }
            }
            Event::AgentResponseFinished(finished) => {
                if let Some(bid) = self.prompt_blocks.remove(&finished.session_prompt_id) {
                    // Remove the live block (from all zones + storage),
                    // then print the final text to history.
                    self.handle.remove_block(bid);

                    let text = finished.text.as_deref().unwrap_or("");
                    self.handle.print_output(
                        StyledBlock::new(StyledText::from(Span::new(
                            text,
                            Style::default().fg(Color::White),
                        )))
                        .bg(Color::Rgb {
                            r: 25,
                            g: 35,
                            b: 45,
                        }),
                    );
                }
            }
            Event::ToolProgress(progress) => {
                let text = tau_harness::format_tool_progress(progress);
                self.handle.print_output(
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
                self.handle.print_output(
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
                self.handle.print_output(
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tau_cli_term::{Span, StyledBlock, StyledText, TermHandle};
    use tau_cli_term_raw::Term;
    use tau_proto::{
        AgentResponseFinished, AgentResponseUpdated, Event, SessionPromptCreated,
        SessionPromptQueued, UiPromptSubmitted,
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
            self.parser.lock().expect("vt").screen().rows(0, w).collect()
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
        let (term, handle, _input) =
            Term::new_virtual(w as usize, h as usize, "> ", Box::new(vt.clone()));
        (term, handle, vt)
    }

    /// Force a sync redraw. Two syncs guarantee that any async
    /// redraws triggered by print_output etc. have completed and
    /// our final render sees the latest state.
    fn sync(handle: &TermHandle) {
        handle.redraw_sync();
        handle.redraw_sync();
    }

    #[test]
    fn single_prompt_response_cycle() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(handle.clone());

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
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "..."));

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
        let mut renderer = EventRenderer::new(handle.clone());

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
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "response one"));

        // Second dispatched.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
        }));
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
        let mut renderer = EventRenderer::new(handle.clone());

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
            let spid = format!("sp-{i}");
            if i > 0 {
                renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                    session_prompt_id: spid.clone(),
                    session_id: "s1".into(),
                    system_prompt: String::new(),
                    messages: Vec::new(),
                    tools: Vec::new(),
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
            !vt.screen_contains(80, "..."),
            "no '...' should remain, got: {:?}",
            vt.screen_text(80)
        );
    }

    #[test]
    fn streaming_block_does_not_duplicate_on_finish() {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(handle.clone());

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
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-0".into(),
            text: "hello!".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some("hello!".into()),
            tool_calls: Vec::new(),
        }));
        sync(&handle);

        // Count how many rows contain "hello!".
        let count = vt.screen_text(80)
            .iter()
            .filter(|r| r.contains("hello!"))
            .count();
        assert_eq!(
            count, 1,
            "response should appear exactly once, got {count}: {:?}",
            vt.screen_text(80)
        );
    }
}

