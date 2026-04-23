//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
    "Tau users don’t think outside the box; they think around the circle.",
    "Tau is the irrational choice for rational Unix hackers.",
    "Some projects revolve around hype. This one revolves around Tau.",
    "Small tools, loosely joined — that’s the Tau of Unix.",
    "Everything is a process, and every process deserves a full turn.",
    "In Tau, what goes around comes around over stdio.",
    "We’ve come full τurn.",
    "Tau keeps the loop tight and the pipes honest.",
    "Every extension gets its turn in Tau.",
    "Tau speaks fluent stdio with a circular accent.",
    "Agents, tools, sockets, loops: the Tau lineup is well-rounded.",
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
                    // Peel the LogEvent wrapper so downstream renderers
                    // see the inner payload directly. The UI is a
                    // best-effort consumer and does not ack.
                    let (_log_id, inner) = event.peel_log();
                    if event_tx.send(inner).is_err() {
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
        SlashCommand::new("/model", "Switch model (e.g. /model provider/model-id)"),
    ];
    let theme = tau_themes::Theme::builtin();
    let prompt_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER);
    let prompt = tau_cli_term::Span::new("> ", prompt_style);
    let (mut term, handle, completion_data) = HighTerm::new(prompt, commands, theme.clone())?;

    // Show logo if enabled.
    let settings = tau_config::settings::load_cli_settings().unwrap_or_default();
    if settings.show_logo {
        use tau_cli_term::{Span, StyledBlock, StyledText};
        let accent = tau_cli_term::resolve::resolve(&theme, tau_themes::names::BANNER_ACCENT);
        let pun = random_startup_pun();
        let banner = StyledText::from(vec![
            Span::new("▀█▀▀ ", accent),
            Span::plain(format!("tau {}", env!("CARGO_PKG_VERSION"))),
            Span::new("\n", Default::default()),
            Span::new(" █▄  ", accent),
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
    let _renderer = std::thread::spawn(move || {
        let mut renderer = EventRenderer::new(renderer_handle, renderer_completion_data, theme);
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

fn random_startup_pun() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize % STARTUP_PUNS.len())
        .unwrap_or(0);
    STARTUP_PUNS[idx]
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

                if writer
                    .write_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
                        session_id: session_id.into(),
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

/// Formats a tool call for display while it is running.
/// Which status-suffix style the completion block should use.
#[derive(Clone, Copy)]
enum ToolStatus {
    Success,
    Error,
    Info,
}

/// Decomposed tool-call label, painted as three themed spans:
/// `<tool_name> <args> <suffix>`. `suffix` is `None` while the tool is
/// still running (nothing to say yet about the outcome).
struct ToolCallDisplay {
    tool_name: String,
    args: String,
    suffix: Option<String>,
    status: ToolStatus,
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
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffix: None,
        status: ToolStatus::Success,
    }
}

/// Error-path display: `<tool_name> <args>` with a `": <msg>"`
/// status suffix in the error style. Shared by every tool.
fn format_tool_error(tool_name: &str, args: String, error_message: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        args,
        suffix: Some(format!(": {error_message}")),
        status: ToolStatus::Error,
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
                let line_count = content.lines().count();
                let byte_count = content.len();
                ToolCallDisplay {
                    tool_name: "read".into(),
                    args: path,
                    suffix: Some(format!("({line_count} lines, {byte_count} bytes)")),
                    status: ToolStatus::Success,
                }
            }
        }
        "write" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("write", path, msg)
            } else {
                let bytes = cbor_int_field(details, "bytes_written").unwrap_or(0);
                ToolCallDisplay {
                    tool_name: "write".into(),
                    args: path,
                    suffix: Some(format!("({bytes} bytes)")),
                    status: ToolStatus::Success,
                }
            }
        }
        "edit" => {
            let path = cbor_text_field(details, "path").unwrap_or_default();
            if let Some(msg) = error_message {
                format_tool_error("edit", path, msg)
            } else {
                let count = cbor_int_field(details, "edits_applied").unwrap_or(0);
                ToolCallDisplay {
                    tool_name: "edit".into(),
                    args: path,
                    suffix: Some(format!("({count} edits applied)")),
                    status: ToolStatus::Success,
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
                    suffix: Some(format!("({count} matches)")),
                    // Zero matches is neutral informational, not a failure.
                    status: if count == 0 {
                        ToolStatus::Info
                    } else {
                        ToolStatus::Success
                    },
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
                let count = cbor_int_field(details, "matches").unwrap_or(0);
                let suffix_word = if count == 1 { "match" } else { "matches" };
                ToolCallDisplay {
                    tool_name: "grep".into(),
                    args,
                    suffix: Some(format!("({count} {suffix_word})")),
                    status: if count == 0 {
                        ToolStatus::Info
                    } else {
                        ToolStatus::Success
                    },
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
                    suffix: Some(format!("({count} entries)")),
                    status: ToolStatus::Success,
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
                    suffix: Some("loaded".to_owned()),
                    status: ToolStatus::Success,
                }
            }
        }
        _ => ToolCallDisplay {
            tool_name: tool_name.to_owned(),
            args: String::new(),
            suffix: Some(match error_message {
                Some(msg) => format!(": {msg}"),
                None => "done".to_owned(),
            }),
            status: match error_message {
                Some(_) => ToolStatus::Error,
                None => ToolStatus::Info,
            },
        },
    }
}

fn format_shell_completion(details: &CborValue, error_message: Option<&str>) -> ToolCallDisplay {
    let cmd = cbor_text_field(details, "command").unwrap_or_default();
    if let Some(msg) = error_message {
        return format_tool_error("shell", cmd, msg);
    }
    let status = cbor_int_field(details, "status");
    let (suffix, outcome) = match status {
        Some(code) => (
            format!("[{code}]"),
            if code == 0 {
                ToolStatus::Success
            } else {
                ToolStatus::Error
            },
        ),
        None => ("[?]".to_owned(), ToolStatus::Info),
    };
    ToolCallDisplay {
        tool_name: "shell".into(),
        args: cmd,
        suffix: Some(suffix),
        status: outcome,
    }
}

/// Paints a [`ToolCallDisplay`] onto a three-span styled block:
/// tool name in the name style, args in the args style, trailing
/// status suffix (if any) in the success/error/info style.
fn render_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let name_style = resolve(theme, names::TOOL_NAME);
    let args_style = resolve(theme, names::TOOL_ARGS);
    let status_name = match display.status {
        ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
        ToolStatus::Error => names::TOOL_STATUS_ERROR,
        ToolStatus::Info => names::TOOL_STATUS_INFO,
    };
    let status_style = resolve(theme, status_name);

    let mut spans = vec![Span::new(display.tool_name.clone(), name_style)];
    if !display.args.is_empty() {
        spans.push(Span::new(" ", args_style));
        spans.push(Span::new(display.args.clone(), args_style));
    }
    if let Some(ref suffix) = display.suffix {
        // The error prefix `": "` hugs the args, everything else gets
        // a single space gap.
        if !suffix.starts_with(':') {
            spans.push(Span::new(" ", args_style));
        }
        spans.push(Span::new(suffix.clone(), status_style));
    }
    StyledBlock::new(StyledText::from(spans))
}

/// Event renderer. Maps session_prompt_id → block_id for in-place
/// updates. No flags, no suppression — just ID-based lookups.
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
    /// Live extension blocks keyed by instance_id. Shown in
    /// above_active while starting, moved to history when ready.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, tau_cli_term::BlockId>,
    /// Persistent status bar block showing the current model.
    model_status_block: Option<tau_cli_term::BlockId>,
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
            extension_blocks: HashMap::new(),
            model_status_block: None,
        }
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
                if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
                    self.handle.remove_block(queued_id);
                    self.handle.print_output(themed_block(
                        &self.theme,
                        names::USER_PROMPT,
                        format!("> {text}"),
                    ));
                }

                let block = themed_block(&self.theme, names::AGENT_PENDING, " …");
                let id = self.handle.new_block(block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.prompt_blocks
                    .insert(prompt.session_prompt_id.to_string(), id);
            }
            Event::AgentResponseUpdated(update) => {
                if let Some(&bid) = self.prompt_blocks.get(update.session_prompt_id.as_str()) {
                    let text = format!("{} …", update.text);
                    let block = themed_block(&self.theme, names::AGENT_RESPONSE, text);
                    self.handle.set_block(bid, block);
                    self.handle.redraw();
                }
            }
            Event::AgentResponseFinished(finished) => {
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
                self.handle
                    .print_output(render_tool_block(&self.theme, &display));
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
                let file_name = agents
                    .file_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("AGENTS.md");
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::SYSTEM_INFO,
                    format!("loaded {file_name}: {}", agents.file_path.display()),
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
                self.handle.print_output(themed_block(
                    &self.theme,
                    names::SYSTEM_INFO,
                    &info.message,
                ));
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
                let label = if selected.model.is_empty() {
                    "no model selected".to_string()
                } else {
                    selected.model.to_string()
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

        let command = parsed.command.unwrap_or(cli::Command::Chat {
            session_id: tau_harness::default_session_id().to_owned(),
            config: None,
        });

        match command {
            cli::Command::Chat {
                session_id,
                config: _config,
            } => {
                // Generate a unique session ID per run to avoid
                // accumulating stale history across daemon restarts.
                let session_id = if session_id == tau_harness::default_session_id() {
                    format!(
                        "chat-{}",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    )
                } else {
                    session_id
                };
                run_chat(&session_id)
            }

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

            cli::Command::Init { force } => run_init(force),

            cli::Command::Provider { args } => {
                tau_provider::run(&args).map_err(|e| CliError::Participant(e.to_string()))
            }

            cli::Command::Component { name } => {
                let runner: fn() -> Result<(), Box<dyn std::error::Error>> = match name.as_str() {
                    "agent" => tau_agent::run_stdio,
                    "ext-fs" => tau_ext_fs::run_stdio,
                    "harness" => tau_harness::run_component,
                    _ => {
                        return Err(CliError::Participant(format!(
                            "unknown component: {name}\navailable: agent, ext-fs, harness"
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

    use tau_cli_term::TermHandle;
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
        let (term, handle, _input) =
            Term::new_virtual(w as usize, h as usize, "> ", Box::new(vt.clone()));
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
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, " …"));

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

        // Second dispatched — "(queued)" should be removed.
        renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: None,
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
            !vt.screen_contains(80, " …"),
            "no ' …' should remain, got: {:?}",
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
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, " …"));

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
        }));
        sync(&handle);
        assert!(vt.screen_contains(80, "Hello"));
        assert!(!vt.screen_contains(80, "Hello …"));
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
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-1".into(),
            text: "Hello again!\n\nHow can I help you?".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("Hello again!\n\nHow can I help you?".into()),
            tool_calls: Vec::new(),
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
        }));
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: "sp-2".into(),
            text: "Hi there!\n\nWhat can I help you with?".into(),
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-2".into(),
            text: Some("Hi there!\n\nWhat can I help you with?".into()),
            tool_calls: Vec::new(),
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
            !vt.screen_contains(80, " …"),
            "no ' …' should remain, got: {:?}",
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
        }));

        // 3 emoji = 6 columns + "end" = 9 columns total.
        let response = "🎉🎊🎈end\nnext line here";
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-0".into(),
            text: Some(response.into()),
            tool_calls: Vec::new(),
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
