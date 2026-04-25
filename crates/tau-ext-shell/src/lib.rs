//! Filesystem and shell tool extension.
//!
//! Provides `read`, `write`, `edit`, `grep`, `find`, `ls`, and
//! `shell` tools.
//!
//! The `echo` tool is available for testing via `include_echo: true`.

use std::error::Error;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, mpsc};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

// ---------------------------------------------------------------------------
// Simple counting semaphore
// ---------------------------------------------------------------------------

struct Semaphore {
    state: Mutex<usize>,
    cond: Condvar,
}

struct SemaphoreGuard<'a>(&'a Semaphore);

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(permits),
            cond: Condvar::new(),
        }
    }

    fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count == 0 {
            count = self.cond.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        SemaphoreGuard(self)
    }
}

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        let mut count = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
        self.0.cond.notify_one();
    }
}

use tau_proto::{
    Ack, CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, LogEventId, PROTOCOL_VERSION, SessionStarted, ToolError,
    ToolProgress, ToolRegister, ToolResult, ToolSideEffects, ToolSpec,
};

pub const ECHO_TOOL_NAME: &str = "echo";
pub const READ_TOOL_NAME: &str = "read";
pub const WRITE_TOOL_NAME: &str = "write";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const SHELL_TOOL_NAME: &str = "shell";
pub const GREP_TOOL_NAME: &str = "grep";
pub const FIND_TOOL_NAME: &str = "find";
pub const LS_TOOL_NAME: &str = "ls";

struct DiscoveredAgentsFile {
    file_path: PathBuf,
    content: String,
}

/// Runs the extension on stdin/stdout (production, no echo).
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run_impl(std::io::stdin(), std::io::stdout(), false)
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// When `include_echo` is true, registers the `echo` tool (for testing).
pub fn run<R, W>(reader: R, writer: W, include_echo: bool) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_impl(reader, writer, include_echo)
}

fn run_impl<R, W>(reader: R, writer: W, include_echo: bool) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-shell".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::TOOL_INVOKE),
            EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
            EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    if include_echo {
        writer.write_event(&Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: ECHO_TOOL_NAME.into(),
                description: Some("Echo the provided payload unchanged".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        }))?;
    }
    for tool in [
        ToolSpec {
            name: READ_TOOL_NAME.into(),
            description: Some(
                "Read the contents of a file. Supports optional line-based slicing via `start_line` and `line_count`. Returns the file path and text content."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "1-based first line to read. If omitted, starts at line 1."
                    },
                    "line_count": {
                        "type": "integer",
                        "description": "Maximum number of lines to return. If omitted, reads through end of file."
                    }
                },
                "required": ["path"]
            })),
            side_effects: ToolSideEffects::Pure,
        },
        ToolSpec {
            name: WRITE_TOOL_NAME.into(),
            description: Some(
                "Write content to a file, creating it if it does not exist. \
                 Returns the path and bytes written."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "content": {
                        "type": "string",
                        "description": "File contents, written verbatim. Embed real newlines directly — do NOT use backslash-n escape sequences."
                    }
                },
                "required": ["path", "content"]
            })),
            side_effects: ToolSideEffects::Mutating,
        },
        ToolSpec {
            name: EDIT_TOOL_NAME.into(),
            description: Some(
                "Edit a file using exact text replacement. Each edit's oldText must match \
                 a unique, non-overlapping region of the original file. All edits are matched \
                 against the original content, not incrementally."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements matched against the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text to find, matched verbatim. Must be unique in the file. Embed real newlines directly — do NOT use backslash-n escape sequences."
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "Replacement text, written verbatim. Embed real newlines directly — do NOT use backslash-n escape sequences."
                                }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            })),
            side_effects: ToolSideEffects::Mutating,
        },
        ToolSpec {
            name: GREP_TOOL_NAME.into(),
            description: Some(
                "Search file contents for a pattern using ripgrep. Returns matching lines with \
                 file paths and line numbers. Respects .gitignore. Output is truncated at \
                 `limit` matches or 50KB. Long lines are truncated to 500 chars."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern. Treated as a literal string by default. Set `regex: true` to interpret as a regex."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.rs'"
                    },
                    "ignoreCase": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    },
                    "regex": {
                        "type": "boolean",
                        "description": "Interpret `pattern` as a regex instead of a literal string (default: false)"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of lines to show before and after each match (default: 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default: 100)"
                    }
                },
                "required": ["pattern"]
            })),
            side_effects: ToolSideEffects::Pure,
        },
        ToolSpec {
            name: FIND_TOOL_NAME.into(),
            description: Some(
                "Search for files by glob pattern. Returns only file paths (directories are \
                 never included, even with '**/*') relative to the search directory. Respects \
                 .gitignore. Output is truncated at `limit` results or 50KB. Use the ls tool \
                 if you want to see directory entries."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern matched against file paths relative to `path`. `**` matches any number of intermediate directories, including zero — so `**/*.rs` finds both top-level `a.rs` and nested `src/a.rs`. Directories are not returned, even with `**/*`."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search (default: current directory)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 1000)"
                    }
                },
                "required": ["pattern"]
            })),
            side_effects: ToolSideEffects::Pure,
        },
        ToolSpec {
            name: LS_TOOL_NAME.into(),
            description: Some(
                "List directory contents. Returns entries sorted alphabetically, with '/' suffix \
                 for directories. Includes dotfiles. Output is truncated at `limit` entries or 50KB."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list (default: current directory)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of entries to return (default: 500)"
                    }
                }
            })),
            side_effects: ToolSideEffects::Pure,
        },
        ToolSpec {
            name: SHELL_TOOL_NAME.into(),
            description: Some(
                "Execute a shell command via `sh -c`. Returns stdout, stderr, \
                 and exit status. Prefer the dedicated `read`/`write`/`edit`/\
                 `grep`/`find`/`ls` tools when they fit — `sh` is for things \
                 those tools can't do."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 120"
                    }
                },
                "required": ["command"]
            })),
            side_effects: ToolSideEffects::Mutating,
        },
    ] {
        writer.write_event(&Event::ToolRegister(ToolRegister { tool }))?;
    }

    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("filesystem and shell tools ready".to_owned()),
    }))?;
    writer.flush()?;

    // Response channel: worker threads send events here, writer thread
    // drains them onto the wire.
    let (tx, rx) = mpsc::channel::<Event>();
    let sem = Arc::new(Semaphore::new(16));

    // Writer thread: drains response events and writes them to the wire.
    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for event in rx {
            writer
                .write_event(&event)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    // Reader loop: dispatch each tool invocation to a worker thread.
    //
    // ToolInvoke is sent point-to-point (not via the harness event log)
    // so it carries no `LogEventId` and needs no ack — the
    // ToolResult/ToolError correlated by call_id is the implicit reply.
    //
    // Other subscribed events (SessionStarted) come wrapped as
    // `Event::LogEvent` and require an `Ack` after processing.
    loop {
        let Some(event) = reader.read_event()? else {
            break;
        };
        let (log_id, inner) = event.peel_log();
        match inner {
            Event::ToolInvoke(invoke) => {
                let tx = tx.clone();
                let sem = Arc::clone(&sem);
                std::thread::spawn(move || {
                    let _permit = sem.acquire();
                    dispatch_tool_invoke(invoke, include_echo, &tx);
                });
            }
            Event::SessionStarted(started) => {
                dispatch_session_started(started, &tx);
            }
            Event::UiShellCommand(cmd) => {
                // User-initiated `!`/`!!` — run on a worker thread
                // and stream chunks out via the same tx writer.
                let tx = tx.clone();
                let sem = Arc::clone(&sem);
                std::thread::spawn(move || {
                    let _permit = sem.acquire();
                    dispatch_user_shell_command(cmd, &tx);
                });
            }
            Event::LifecycleDisconnect(_) => break,
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &tx);
        }
    }

    // Drop the sender so the writer thread exits.
    drop(tx);
    writer_handle
        .join()
        .map_err(|_| "writer thread panicked")?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

/// Execute a single tool invocation and send the response event(s).
fn dispatch_tool_invoke(
    invoke: tau_proto::ToolInvoke,
    include_echo: bool,
    tx: &mpsc::Sender<Event>,
) {
    let events = execute_tool(invoke, include_echo);
    for event in events {
        let _ = tx.send(event);
    }
}

fn dispatch_session_started(started: SessionStarted, tx: &mpsc::Sender<Event>) {
    for event in build_session_started_events(started) {
        let _ = tx.send(event);
    }
}

/// Run a user-initiated `!`/`!!` shell command, streaming stdout and
/// stderr back as `ShellCommandProgress` chunks while they arrive and
/// emitting `ShellCommandFinished` with the full (truncated-tail)
/// output when the child exits.
fn dispatch_user_shell_command(cmd: tau_proto::UiShellCommand, tx: &mpsc::Sender<Event>) {
    use std::io::Read;

    let mut child_cmd = Command::new("sh");
    child_cmd
        .arg("-c")
        .arg(&cmd.command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match child_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send(Event::ShellCommandFinished(
                tau_proto::ShellCommandFinished {
                    command_id: cmd.command_id,
                    session_id: cmd.session_id,
                    command: cmd.command,
                    include_in_context: cmd.include_in_context,
                    output: format!("failed to start shell command: {err}"),
                    exit_code: None,
                    cancelled: false,
                },
            ));
            return;
        }
    };

    // Read each pipe on a dedicated thread. Each read chunk is both
    // emitted as a `ShellCommandProgress` event (for live UI
    // rendering) and accumulated into a buffer that we later truncate
    // and send in `ShellCommandFinished` (for session-history
    // injection when `include_in_context`).

    fn pump<R: Read + Send + 'static>(
        mut pipe: R,
        stream: tau_proto::ShellStream,
        command_id: tau_proto::ShellCommandId,
        tx: mpsc::Sender<Event>,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut captured = String::new();
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        captured.push_str(&chunk);
                        let _ = tx.send(Event::ShellCommandProgress(
                            tau_proto::ShellCommandProgress {
                                command_id: command_id.clone(),
                                stream,
                                chunk,
                            },
                        ));
                    }
                }
            }
            captured
        })
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = stdout_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stdout,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });
    let stderr_handle = stderr_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stderr,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });

    let wait = child.wait();
    let stdout = stdout_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    let (exit_code, status_note) = match wait {
        Ok(status) => (status.code(), None),
        Err(err) => (None, Some(format!("wait failed: {err}"))),
    };

    // Interleave stdout + stderr in the final output the way the
    // `shell` tool does: stderr follows stdout under a separator.
    let mut merged = stdout;
    if !stderr.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(&stderr);
    }
    if let Some(note) = status_note {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&note);
    }
    let truncated = truncate_tail(&merged);

    let _ = tx.send(Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            output: truncated.content,
            exit_code,
            cancelled: false,
        },
    ));
}

fn ack_log_event(id: LogEventId, tx: &mpsc::Sender<Event>) {
    let _ = tx.send(Event::Ack(Ack { up_to: id }));
}

fn build_session_started_events(started: SessionStarted) -> Vec<Event> {
    let mut events = Vec::new();

    let mut skill_dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        skill_dirs.push(cwd.join(".agents").join("skills"));
    }
    if let Some(home) = dirs::home_dir() {
        skill_dirs.push(home.join(".agents").join("skills"));
    }

    let result = tau_skills::load_skills_from_dirs(&skill_dirs);
    for skill in result.skills {
        let file_path = skill.file_path.canonicalize().unwrap_or(skill.file_path);
        events.push(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: skill.name.into(),
            description: skill.description,
            file_path,
            add_to_prompt: skill.add_to_prompt,
        }));
    }

    for agents_file in discover_session_agents_files() {
        events.push(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: agents_file.file_path,
                content: agents_file.content,
            },
        ));
    }

    events.push(Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            session_id: started.session_id,
        },
    ));
    events
}

/// Execute a tool and return the response event(s).
fn execute_tool(invoke: tau_proto::ToolInvoke, include_echo: bool) -> Vec<Event> {
    if include_echo && invoke.tool_name == ECHO_TOOL_NAME {
        return vec![Event::ToolResult(ToolResult {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            result: invoke.arguments,
        })];
    }

    if invoke.tool_name == READ_TOOL_NAME {
        return match read_file(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == WRITE_TOOL_NAME {
        return match write_file(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == EDIT_TOOL_NAME {
        return match edit_file(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == GREP_TOOL_NAME {
        return match run_grep(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == FIND_TOOL_NAME {
        return match run_find(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == LS_TOOL_NAME {
        return match run_ls(&invoke.arguments) {
            Ok(result) => vec![Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })],
            Err(error) => vec![Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message: error,
                details: None,
            })],
        };
    }

    if invoke.tool_name == SHELL_TOOL_NAME {
        let mut events = vec![Event::ToolProgress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("running shell command".to_owned()),
            progress: None,
        })];
        match run_command(&invoke.arguments) {
            Ok(result) => events.push(Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
            })),
            Err((message, details)) => events.push(Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message,
                details,
            })),
        }
        return events;
    }

    vec![Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        message: "unknown tool".to_owned(),
        details: None,
    })]
}

// ---------------------------------------------------------------------------
// Output truncation
// ---------------------------------------------------------------------------

/// Maximum lines before truncation kicks in.
const MAX_OUTPUT_LINES: usize = 2000;
/// Maximum bytes before truncation kicks in.
const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Result of a truncation operation.
struct Truncated {
    content: String,
    was_truncated: bool,
    total_lines: usize,
    total_bytes: usize,
}

fn truncate_head_plain(input: &str) -> Truncated {
    let total_lines = input.lines().count();
    let total_bytes = input.len();

    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: input.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    let mut result = String::new();
    let mut bytes = 0;
    let mut kept_lines = 0;

    for (line_idx, line) in input.lines().enumerate() {
        if kept_lines >= MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        if line_idx > 0 {
            result.push('\n');
            bytes += 1;
        }
        result.push_str(line);
        bytes += line.len();
        kept_lines = line_idx + 1;
    }

    Truncated {
        content: result,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

/// Truncate from the head (keep first lines).  Used by `read`.
fn truncate_head(input: &str) -> Truncated {
    truncate_head_with_notice(input, "Use start_line and line_count to continue reading.")
}

fn truncate_head_with_notice(input: &str, continuation_hint: &str) -> Truncated {
    let mut truncated = truncate_head_plain(input);
    if !truncated.was_truncated {
        return truncated;
    }

    let kept_lines = truncated.content.lines().count();
    truncated.content.push_str(&format!(
        "\n\n[Showing lines 1-{kept_lines} of {} ({} bytes total). \
         {continuation_hint}]",
        truncated.total_lines, truncated.total_bytes
    ));
    truncated
}

/// Truncate from the tail (keep last lines).  Used by `shell`.
fn truncate_tail(input: &str) -> Truncated {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let total_bytes = input.len();

    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: input.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    // Walk backwards, accumulating lines until we hit a limit.
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;

    for &line in all_lines.iter().rev() {
        if kept.len() >= MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();

    let first_kept = total_lines - kept.len() + 1;
    let last_kept = total_lines;
    let mut result = format!(
        "[Showing lines {first_kept}-{last_kept} of {total_lines} ({total_bytes} bytes total)]\n\n"
    );
    result.push_str(&kept.join("\n"));

    Truncated {
        content: result,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

fn read_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let start_line = parse_read_start_line(arguments)?;
    let line_count = parse_read_line_count(arguments)?;
    let path_buf = PathBuf::from(&path);
    let raw = fs::read_to_string(&path_buf).map_err(|error| error.to_string())?;

    let total_lines = raw.lines().count();
    let sliced = slice_lines(&raw, start_line, line_count);
    let truncated = truncate_head_with_notice(
        &sliced.content,
        "Use start_line and line_count to continue reading.",
    );
    let mut entries = vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text(truncated.content),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer((sliced.start_line as i64).into()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer((sliced.line_count as i64).into()),
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ),
    ];
    if truncated.was_truncated {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((truncated.total_bytes as i64).into()),
        ));
    }
    Ok(CborValue::Map(entries))
}

struct ReadSlice {
    content: String,
    start_line: usize,
    line_count: usize,
}

fn parse_read_start_line(arguments: &CborValue) -> Result<usize, String> {
    match optional_argument_int(arguments, "start_line") {
        None => Ok(1),
        Some(value) if value < 1 => Err("start_line must be >= 1".to_owned()),
        Some(value) => Ok(value as usize),
    }
}

fn parse_read_line_count(arguments: &CborValue) -> Result<Option<usize>, String> {
    match optional_argument_int(arguments, "line_count") {
        None => Ok(None),
        Some(value) if value < 1 => Err("line_count must be >= 1".to_owned()),
        Some(value) => Ok(Some(value as usize)),
    }
}

fn slice_lines(input: &str, start_line: usize, line_count: Option<usize>) -> ReadSlice {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let start_idx = start_line.saturating_sub(1).min(total_lines);
    let end_idx = match line_count {
        Some(count) => start_idx.saturating_add(count).min(total_lines),
        None => total_lines,
    };
    ReadSlice {
        content: all_lines[start_idx..end_idx].join("\n"),
        start_line,
        line_count: end_idx.saturating_sub(start_idx),
    }
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

fn write_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let content = argument_text(arguments, "content")?;
    let path_buf = PathBuf::from(&path);

    if let Some(parent) = path_buf.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }

    // Best-effort read of the existing file so we can diff. If the
    // file doesn't exist (or can't be decoded as utf-8), treat the
    // baseline as empty — every line of `content` becomes an add.
    let original = fs::read_to_string(&path_buf).unwrap_or_default();

    let bytes_written = content.len();
    fs::write(&path_buf, &content).map_err(|error| error.to_string())?;

    let diff = compute_diff(&original, &content);

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("bytes_written".to_owned()),
            CborValue::Integer((bytes_written as i64).into()),
        ),
        (CborValue::Text("diff".to_owned()), encode_diff(&diff)),
    ]))
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

fn edit_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let path_buf = PathBuf::from(&path);

    let original = fs::read_to_string(&path_buf).map_err(|e| e.to_string())?;

    let edits = argument_array(arguments, "edits")?;
    if edits.is_empty() {
        return Err("edits array must not be empty".to_owned());
    }

    // Collect all (oldText, newText) pairs and validate against the original.
    let mut replacements: Vec<(usize, usize, &str)> = Vec::new();
    for edit in edits {
        let old_text = cbor_map_text(edit, "oldText")
            .ok_or_else(|| "each edit must have a string oldText".to_owned())?;
        let new_text = cbor_map_text(edit, "newText")
            .ok_or_else(|| "each edit must have a string newText".to_owned())?;

        let Some(start) = original.find(old_text) else {
            return Err("not found".to_owned());
        };
        let end = start + old_text.len();

        // Check uniqueness: there should be no second match.
        if original[start + 1..].contains(old_text) {
            return Err("ambiguous match".to_owned());
        }

        replacements.push((start, end, new_text));
    }

    // Sort by start position (descending) so we can apply from end to start
    // without invalidating earlier offsets.
    replacements.sort_by(|a, b| b.0.cmp(&a.0));

    // Check for overlapping ranges.
    for pair in replacements.windows(2) {
        // After descending sort: pair[0].start >= pair[1].start.
        // Overlap if pair[1].end > pair[0].start (pair[1] is earlier in file).
        if pair[1].1 > pair[0].0 {
            return Err("overlapping edits".to_owned());
        }
    }

    // Apply replacements from end to start.
    let mut result = original.clone();
    for (start, end, new_text) in &replacements {
        result.replace_range(*start..*end, new_text);
    }

    fs::write(&path_buf, &result).map_err(|e| e.to_string())?;

    let diff = compute_diff(&original, &result);

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("edits_applied".to_owned()),
            CborValue::Integer((edits.len() as i64).into()),
        ),
        (CborValue::Text("diff".to_owned()), encode_diff(&diff)),
    ]))
}

// ---------------------------------------------------------------------------
// diff helpers
// ---------------------------------------------------------------------------

/// Number of unchanged lines to keep around each hunk's edits.
const DIFF_CONTEXT_LINES: usize = 3;

/// Compute a [`tau_proto::DiffSummary`] from two file contents using
/// the `similar` crate. Hunks that are exactly one Remove paired with
/// one Add collapse into a single [`DiffLine::Modify`] with intra-line
/// word-level segments; other shapes flatten to plain Add/Remove/Equal
/// rows.
fn compute_diff(old: &str, new: &str) -> tau_proto::DiffSummary {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut summary = tau_proto::DiffSummary::default();

    for group in diff.grouped_ops(DIFF_CONTEXT_LINES) {
        if group.is_empty() {
            continue;
        }

        // Hunk header (1-based line numbers like unified-diff).
        let first = &group[0];
        let last = &group[group.len() - 1];
        let old_start = first.old_range().start as u32 + 1;
        let new_start = first.new_range().start as u32 + 1;
        let old_count = (last.old_range().end - first.old_range().start) as u32;
        let new_count = (last.new_range().end - first.new_range().start) as u32;

        let mut lines: Vec<tau_proto::DiffLine> = Vec::new();
        // Group adjacent {1×Remove, 1×Add} pairs into Modify lines so
        // single-line edits get intra-line word-level highlighting.
        let mut pending_remove: Option<String> = None;
        for op in &group {
            for change in diff.iter_changes(op) {
                let text = strip_eol(change.value()).to_owned();
                match change.tag() {
                    ChangeTag::Equal => {
                        if let Some(removed) = pending_remove.take() {
                            lines.push(tau_proto::DiffLine::Remove { text: removed });
                        }
                        lines.push(tau_proto::DiffLine::Equal { text });
                    }
                    ChangeTag::Delete => {
                        if let Some(removed) = pending_remove.take() {
                            lines.push(tau_proto::DiffLine::Remove { text: removed });
                        }
                        pending_remove = Some(text);
                        summary.removed += 1;
                    }
                    ChangeTag::Insert => {
                        summary.added += 1;
                        if let Some(removed) = pending_remove.take() {
                            // 1-Remove + 1-Add → Modify with intra-line segments.
                            lines.push(make_modify(&removed, &text));
                        } else {
                            lines.push(tau_proto::DiffLine::Add { text });
                        }
                    }
                }
            }
        }
        if let Some(removed) = pending_remove.take() {
            lines.push(tau_proto::DiffLine::Remove { text: removed });
        }

        summary.hunks.push(tau_proto::DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        });
    }

    summary
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

fn make_modify(old: &str, new: &str) -> tau_proto::DiffLine {
    use similar::{ChangeTag, TextDiff};
    let inline = TextDiff::from_words(old, new);
    let mut old_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    let mut new_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    for change in inline.iter_all_changes() {
        let text = change.value().to_owned();
        match change.tag() {
            ChangeTag::Equal => {
                old_segs.push(tau_proto::DiffSegment::Equal { text: text.clone() });
                new_segs.push(tau_proto::DiffSegment::Equal { text });
            }
            ChangeTag::Delete => {
                old_segs.push(tau_proto::DiffSegment::Remove { text });
            }
            ChangeTag::Insert => {
                new_segs.push(tau_proto::DiffSegment::Add { text });
            }
        }
    }
    tau_proto::DiffLine::Modify {
        old: old_segs,
        new: new_segs,
    }
}

/// Encode a [`tau_proto::DiffSummary`] as a [`CborValue`] sub-tree
/// embedded in a tool result. Goes through `serde_cbor` style via
/// ciborium so the wire shape matches the Rust struct's serde derives.
fn encode_diff(summary: &tau_proto::DiffSummary) -> CborValue {
    let mut buf = Vec::new();
    if ciborium::ser::into_writer(summary, &mut buf).is_err() {
        return CborValue::Null;
    }
    ciborium::de::from_reader(buf.as_slice()).unwrap_or(CborValue::Null)
}

// ---------------------------------------------------------------------------
// grep (ripgrep)
// ---------------------------------------------------------------------------

const DEFAULT_GREP_LIMIT: usize = 100;
const GREP_MAX_LINE_LENGTH: usize = 500;
const DEFAULT_FIND_LIMIT: usize = 1000;
const DEFAULT_LS_LIMIT: usize = 500;

fn run_grep(arguments: &CborValue) -> Result<CborValue, String> {
    let pattern = argument_text(arguments, "pattern")?;
    let path = optional_argument_text(arguments, "path");
    let glob = optional_argument_text(arguments, "glob");
    let ignore_case = optional_argument_bool(arguments, "ignoreCase").unwrap_or(false);
    // Literal matching is the default. Most callers are searching for
    // an exact string and regex metacharacters in that string (`[`,
    // `(`, `.`, `?`, `+`, `*`, `|`, `{`, `\`) would otherwise either
    // fail to parse or silently match something unintended. Regex
    // users opt in explicitly with `regex: true`.
    let regex = optional_argument_bool(arguments, "regex").unwrap_or(false);
    let context = optional_argument_int(arguments, "context").map(|v| v.max(0) as usize);
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_GREP_LIMIT);

    let search_path = path.as_deref().unwrap_or(".");

    // Build ripgrep arguments.
    let mut args: Vec<String> = vec![
        "--line-number".to_owned(),
        "--color=never".to_owned(),
        "--hidden".to_owned(),
    ];
    if ignore_case {
        args.push("--ignore-case".to_owned());
    }
    if !regex {
        args.push("--fixed-strings".to_owned());
    }
    if let Some(ref g) = glob {
        args.push("--glob".to_owned());
        args.push(g.clone());
    }
    if let Some(ctx) = context {
        args.push(format!("--context={ctx}"));
    }
    args.push("--".to_owned());
    args.push(pattern.clone());
    args.push(search_path.to_owned());

    let child = Command::new("rg")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start ripgrep: {e}"))?;

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for ripgrep: {e}"))?;

    // rg exit codes: 0=matches found, 1=no matches, 2=error
    let status = output.status.code();
    if status == Some(2) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ripgrep error: {}", stderr.trim()));
    }

    let raw_output = String::from_utf8_lossy(&output.stdout);
    if raw_output.trim().is_empty() {
        return Ok(grep_result_map(
            &pattern,
            search_path,
            glob.as_deref(),
            status,
            0,
            "no matches found".to_owned(),
        ));
    }

    // Process lines: count matches, truncate long lines, enforce match limit.
    let mut result_lines = Vec::new();
    let mut match_count: usize = 0;
    let mut lines_truncated = false;
    let mut match_limit_reached = false;

    for line in raw_output.lines() {
        // A match line looks like "path:123:content" (context lines use - instead of :)
        // Count only actual match lines (not context lines).
        let is_match = is_grep_match_line(line);
        if is_match {
            match_count += 1;
            if limit < match_count {
                match_limit_reached = true;
                break;
            }
        }

        // Per-line truncation.
        if line.len() > GREP_MAX_LINE_LENGTH {
            let truncated = truncate_line(line, GREP_MAX_LINE_LENGTH);
            result_lines.push(truncated);
            lines_truncated = true;
        } else {
            result_lines.push(line.to_owned());
        }
    }

    let mut output_text = result_lines.join("\n");

    // Apply byte-level truncation to the assembled output.
    let byte_truncated = truncate_head(&output_text);
    if byte_truncated.was_truncated {
        output_text = byte_truncated.content;
    }

    // Build notices.
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern.",
            limit * 2
        ));
    }
    if byte_truncated.was_truncated {
        notices.push("50KB output limit reached.".to_owned());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines."
        ));
    }

    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    Ok(grep_result_map(
        &pattern,
        search_path,
        glob.as_deref(),
        status,
        match_count,
        output_text,
    ))
}

/// Build the CBOR result map for `grep`. Echoes `pattern`/`path`/`glob`
/// alongside the match count and output so UI renderers have enough
/// context to label the call without re-reading the request arguments.
fn grep_result_map(
    pattern: &str,
    search_path: &str,
    glob: Option<&str>,
    status: Option<i32>,
    matches: usize,
    output_text: String,
) -> CborValue {
    let mut fields = vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern.to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(search_path.to_owned()),
        ),
        (
            CborValue::Text("status".to_owned()),
            status
                .map(|code| CborValue::Integer((code as i64).into()))
                .unwrap_or(CborValue::Null),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text.clone()),
        ),
        (
            CborValue::Text("output_lines".to_owned()),
            CborValue::Integer((output_text.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("output_bytes".to_owned()),
            CborValue::Integer((output_text.len() as i64).into()),
        ),
    ];
    if let Some(glob) = glob {
        fields.push((
            CborValue::Text("glob".to_owned()),
            CborValue::Text(glob.to_owned()),
        ));
    }
    CborValue::Map(fields)
}

fn run_find(arguments: &CborValue) -> Result<CborValue, String> {
    let pattern = argument_text(arguments, "pattern")?;
    let path = optional_argument_text(arguments, "path").unwrap_or_else(|| ".".to_owned());
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_FIND_LIMIT);
    let search_path = PathBuf::from(&path);

    let metadata = fs::metadata(&search_path)
        .map_err(|e| format!("failed to access {}: {e}", search_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {}", search_path.display()));
    }

    let glob = compile_find_glob(&pattern)?;
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(&search_path)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry.map_err(|e| format!("failed to walk {}: {e}", search_path.display()))?;
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let Ok(relative_path) = entry.path().strip_prefix(&search_path) else {
            continue;
        };
        if glob.is_match(relative_path) {
            matches.push(path_to_slash(relative_path));
        }
    }
    matches.sort_by_key(|entry| entry.to_lowercase());

    if matches.is_empty() {
        return Ok(CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(search_path.display().to_string()),
            ),
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text(pattern),
            ),
            (
                CborValue::Text("matches".to_owned()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("no files found matching pattern".to_owned()),
            ),
        ]));
    }

    let total_matches = matches.len();
    let displayed: Vec<String> = matches.into_iter().take(limit).collect();
    let limit_reached = total_matches > displayed.len();
    let mut output_text = displayed.join("\n");
    let truncated = truncate_head_plain(&output_text);
    if truncated.was_truncated {
        output_text = truncated.content;
    }

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern.",
            limit * 2
        ));
    }
    if truncated.was_truncated {
        notices.push("50KB/2000 line output limit reached.".to_owned());
    }
    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(search_path.display().to_string()),
        ),
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((total_matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ]))
}

fn compile_find_glob(pattern: &str) -> Result<GlobSet, String> {
    let glob = Glob::new(pattern).map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|e| format!("failed to compile glob pattern {pattern:?}: {e}"))
}

fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn discover_session_agents_files() -> Vec<DiscoveredAgentsFile> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".agents"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(ancestor_dirs(&cwd));
    }
    discover_agents_files_from_roots(roots)
}

#[cfg(test)]
fn discover_agents_files_from(cwd: &Path) -> Vec<DiscoveredAgentsFile> {
    discover_agents_files_from_roots(ancestor_dirs(cwd))
}

fn discover_agents_files_from_roots(
    roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<DiscoveredAgentsFile> {
    let mut seen = std::collections::HashSet::new();
    let mut discovered = Vec::new();
    for dir in roots {
        let candidate = dir.join("AGENTS.md");
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let Ok(content) = fs::read_to_string(&candidate) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }

        let file_path = candidate.canonicalize().unwrap_or(candidate);
        if !seen.insert(file_path.clone()) {
            continue;
        }
        discovered.push(DiscoveredAgentsFile { file_path, content });
    }

    discovered
}

fn ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        dirs.push(current.clone());
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    dirs.reverse();
    dirs
}

fn run_ls(arguments: &CborValue) -> Result<CborValue, String> {
    let path = optional_argument_text(arguments, "path").unwrap_or_else(|| ".".to_owned());
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_LS_LIMIT);
    let dir_path = PathBuf::from(&path);

    let metadata = fs::metadata(&dir_path)
        .map_err(|e| format!("failed to access {}: {e}", dir_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {}", dir_path.display()));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir_path)
        .map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?
    {
        let entry = entry.map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?;
        let name = entry.file_name();
        let mut display = name.to_string_lossy().into_owned();
        if entry
            .file_type()
            .map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?
            .is_dir()
        {
            display.push('/');
        }
        entries.push(display);
    }
    entries.sort_by_key(|entry| entry.to_lowercase());

    if entries.is_empty() {
        return Ok(CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(dir_path.display().to_string()),
            ),
            (
                CborValue::Text("entries".to_owned()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("(empty directory)".to_owned()),
            ),
        ]));
    }

    let total_entries = entries.len();
    let displayed: Vec<String> = entries.into_iter().take(limit).collect();
    let limit_reached = total_entries > displayed.len();
    let mut output_text = displayed.join("\n");
    let truncated = truncate_head_plain(&output_text);
    if truncated.was_truncated {
        output_text = truncated.content;
    }

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} entries limit reached. Use limit={} for more.",
            limit * 2
        ));
    }
    if truncated.was_truncated {
        notices.push("50KB/2000 line output limit reached.".to_owned());
    }
    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(dir_path.display().to_string()),
        ),
        (
            CborValue::Text("entries".to_owned()),
            CborValue::Integer((total_entries as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ]))
}

/// Classify a ripgrep output line as a match (true) or context (false).
///
/// rg emits match lines as `PATH:LINE:CONTENT` and context lines as
/// `PATH-LINE-CONTENT`. We scan for the leftmost `<sep><digits><sep>`
/// run where both separators are the same `:` or `-` — that brackets
/// the line number — and use the separator character to classify.
///
/// Requiring digits between two identical separators is what rules out
/// bare dashes inside path segments (e.g. `tool-exercise/notes.md:2:…`):
/// the first `-` has no digits after it, so we skip past it to find the
/// real `:2:` separator.
///
/// Edge case: a path like `file-12-34.txt` contains `-\d+-` already in
/// the path, so a match line for it would be misclassified as context.
/// Rare enough to accept; `rg --json` would be the bulletproof fix if
/// this ever matters.
fn is_grep_match_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let sep = bytes[i];
        if sep == b':' || sep == b'-' {
            let digits_start = i + 1;
            let mut j = digits_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start && j < bytes.len() && bytes[j] == sep {
                return sep == b':';
            }
        }
        i += 1;
    }
    false
}

/// Truncate a single line, appending a marker if truncated.
fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &line[..end])
}

fn optional_argument_bool(arguments: &CborValue, key: &str) -> Option<bool> {
    match arguments {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Bool(b)) if k == key => Some(*b),
            _ => None,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_SECS: u64 = 120;

fn run_command(arguments: &CborValue) -> Result<CborValue, (String, Option<CborValue>)> {
    let command = argument_text(arguments, "command").map_err(|message| (message, None))?;
    let cwd = optional_argument_text(arguments, "cwd");
    let timeout_secs = optional_argument_int(arguments, "timeout")
        .map(|v| v.max(1) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout = std::time::Duration::from_secs(timeout_secs);

    let mut child_cmd = Command::new("sh");
    child_cmd
        .arg("-c")
        .arg(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(cwd) = &cwd {
        child_cmd.current_dir(cwd);
    }

    let mut child = child_cmd.spawn().map_err(|error| {
        (
            format!("failed to start shell command: {error}"),
            Some(command_details_value(
                command.clone(),
                cwd.clone(),
                None,
                String::new(),
                String::new(),
            )),
        )
    })?;

    let wait = match wait_with_timeout(&mut child, timeout) {
        Some(wait) => wait,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err((
                format!("command timed out after {timeout_secs}s"),
                Some(command_details_value(
                    command.clone(),
                    cwd.clone(),
                    None,
                    String::new(),
                    String::new(),
                )),
            ));
        }
    };

    let status_code = wait.status.code();
    let success = wait.status.success();

    // Truncate stdout/stderr from the tail (keep last lines — errors and
    // final results are at the end).
    let stdout_trunc = truncate_tail(&wait.stdout);
    let stderr_trunc = truncate_tail(&wait.stderr);

    let result = command_details_value(
        command.clone(),
        cwd.clone(),
        status_code,
        stdout_trunc.content,
        stderr_trunc.content,
    );

    if success {
        Ok(result)
    } else {
        Err((
            format!(
                "command exited with status {}",
                status_code
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
            Some(result),
        ))
    }
}

/// Wait for a child process with a timeout. Returns `None` if timed out.
///
/// Pipes are read on dedicated threads to avoid deadlocks. When the child
/// exits its pipes close, the reader threads complete, and we get our
/// signal — no polling.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<WaitResult> {
    // Take the pipes so we can move them into reader threads.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Read stdout/stderr on dedicated threads so a full pipe buffer
    // can't prevent the child from exiting.
    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe));

    // Collector thread: joins both readers (which complete when the child
    // exits and closes its pipes), then sends the output on a channel.
    let (tx, rx) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();
        let _ = tx.send((stdout, stderr));
    });

    match rx.recv_timeout(timeout) {
        Ok((stdout, stderr)) => {
            // Pipes closed → child exited. Reap it.
            let status = child.wait().expect("child already exited");
            Some(WaitResult {
                status,
                stdout,
                stderr,
            })
        }
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

struct WaitResult {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn read_pipe(pipe: Option<impl std::io::Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = pipe.read_to_string(&mut buf);
    buf
}

fn command_details_value(
    command: String,
    cwd: Option<String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
) -> CborValue {
    let mut entries = vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command),
        ),
        (
            CborValue::Text("stdout".to_owned()),
            CborValue::Text(stdout.clone()),
        ),
        (
            CborValue::Text("stderr".to_owned()),
            CborValue::Text(stderr.clone()),
        ),
        (
            CborValue::Text("stdout_lines".to_owned()),
            CborValue::Integer((stdout.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stdout_bytes".to_owned()),
            CborValue::Integer((stdout.len() as i64).into()),
        ),
        (
            CborValue::Text("stderr_lines".to_owned()),
            CborValue::Integer((stderr.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stderr_bytes".to_owned()),
            CborValue::Integer((stderr.len() as i64).into()),
        ),
    ];
    if let Some(cwd) = cwd {
        entries.push((CborValue::Text("cwd".to_owned()), CborValue::Text(cwd)));
    }
    if let Some(status) = status {
        entries.push((
            CborValue::Text("status".to_owned()),
            CborValue::Integer(status.into()),
        ));
    }
    CborValue::Map(entries)
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn argument_text(arguments: &CborValue, key: &str) -> Result<String, String> {
    optional_argument_text(arguments, key).ok_or_else(|| format!("missing string argument: {key}"))
}

fn optional_argument_text(arguments: &CborValue, key: &str) -> Option<String> {
    cbor_map_text(arguments, key).map(str::to_owned)
}

fn optional_argument_int(arguments: &CborValue, key: &str) -> Option<i64> {
    match arguments {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Integer(n)) if k == key => {
                i128::from(*n).try_into().ok()
            }
            _ => None,
        }),
        _ => None,
    }
}

/// Extract a string value from a CBOR map by key.
fn cbor_map_text<'a>(map: &'a CborValue, key: &str) -> Option<&'a str> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Text(v)) if k == key => Some(v.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

/// Extract an array value from a CBOR map by key.
fn argument_array<'a>(arguments: &'a CborValue, key: &str) -> Result<&'a [CborValue], String> {
    match arguments {
        CborValue::Map(entries) => {
            for (k, v) in entries {
                if let (CborValue::Text(k), CborValue::Array(arr)) = (k, v) {
                    if k == key {
                        return Ok(arr);
                    }
                }
            }
            Err(format!("missing array argument: {key}"))
        }
        _ => Err(format!("missing array argument: {key}")),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;

    use tau_proto::{EventName, ToolInvoke};
    use tempfile::TempDir;

    use super::*;

    fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
        match value {
            CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Integer(n)) if k == key => Some((*n).into()),
                _ => None,
            }),
            _ => None,
        }
    }

    fn spawn_extension() -> (
        EventReader<BufReader<UnixStream>>,
        EventWriter<BufWriter<UnixStream>>,
    ) {
        let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair should open");
        let reader_stream = runtime_stream
            .try_clone()
            .expect("runtime reader clone should succeed");
        thread::spawn(move || {
            run_impl(reader_stream, runtime_stream, true).expect("extension should run");
        });
        (
            EventReader::new(BufReader::new(
                harness_stream
                    .try_clone()
                    .expect("harness reader clone should succeed"),
            )),
            EventWriter::new(BufWriter::new(harness_stream)),
        )
    }

    /// Consumes startup events (hello, subscribe, registers, ready).
    fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) {
        for expected in [
            EventName::LIFECYCLE_HELLO,
            EventName::LIFECYCLE_SUBSCRIBE,
            EventName::TOOL_REGISTER, // echo
            EventName::TOOL_REGISTER, // read
            EventName::TOOL_REGISTER, // write
            EventName::TOOL_REGISTER, // edit
            EventName::TOOL_REGISTER, // grep
            EventName::TOOL_REGISTER, // find
            EventName::TOOL_REGISTER, // ls
            EventName::TOOL_REGISTER, // shell
            EventName::LIFECYCLE_READY,
        ] {
            let event = reader
                .read_event()
                .expect("read")
                .expect("startup event should arrive");
            assert_eq!(event.name(), expected);
        }
    }

    #[test]
    fn discover_agents_files_walks_ancestor_chain_in_order() {
        let tempdir = TempDir::new().expect("tempdir");
        let root = tempdir.path();
        let nested = root.join("pkg/src");
        fs::create_dir_all(&nested).expect("mkdir");

        let root_agents = root.join("AGENTS.md");
        let pkg_agents = root.join("pkg").join("AGENTS.md");
        let empty_agents = root.join("pkg").join("src").join("AGENTS.md");

        fs::write(&root_agents, "# Root\n- rule one\n").expect("write root");
        fs::write(&pkg_agents, "# Package\n- rule two\n").expect("write pkg");
        fs::write(&empty_agents, "   \n").expect("write empty");

        let discovered = discover_agents_files_from(&nested);
        assert_eq!(discovered.len(), 2);
        assert_eq!(
            discovered[0].file_path,
            root_agents.canonicalize().expect("canonical root")
        );
        assert_eq!(
            discovered[1].file_path,
            pkg_agents.canonicalize().expect("canonical pkg")
        );
        assert!(discovered[0].content.contains("rule one"));
        assert!(discovered[1].content.contains("rule two"));
    }

    #[test]
    fn discover_agents_files_from_roots_keeps_home_before_repo_chain() {
        let tempdir = TempDir::new().expect("tempdir");
        let home = tempdir.path().join("home");
        let repo = tempdir.path().join("repo");
        fs::create_dir_all(home.join(".agents")).expect("home agents dir");
        fs::create_dir_all(repo.join("pkg")).expect("repo pkg dir");

        let home_agents = home.join(".agents").join("AGENTS.md");
        let repo_agents = repo.join("AGENTS.md");
        let pkg_agents = repo.join("pkg").join("AGENTS.md");
        fs::write(&home_agents, "# Home\n- personal rule\n").expect("write home");
        fs::write(&repo_agents, "# Repo\n- repo rule\n").expect("write repo");
        fs::write(&pkg_agents, "# Package\n- package rule\n").expect("write pkg");

        let discovered = discover_agents_files_from_roots(vec![
            home.join(".agents"),
            repo.clone(),
            repo.join("pkg"),
        ]);

        let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                home_agents.canonicalize().expect("canonical home"),
                repo_agents.canonicalize().expect("canonical repo"),
                pkg_agents.canonicalize().expect("canonical pkg"),
            ]
        );
    }

    #[test]
    fn session_started_emits_ready_after_startup() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: tau_proto::SessionStartReason::Initial,
            }))
            .expect("request");
        writer.flush().expect("flush");

        loop {
            let event = reader.read_event().expect("read").expect("context event");
            if event.name() == EventName::EXTENSION_CONTEXT_READY {
                let Event::ExtensionContextReady(ready) = event else {
                    unreachable!("matched on event name");
                };
                assert_eq!(ready.session_id, "s1");
                break;
            }
        }

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_reads_file() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("README.txt");
        fs::write(&file_path, "hello from file").expect("write fixture");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: READ_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                )]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let result = reader.read_event().expect("read").expect("result");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, READ_TOOL_NAME);
        assert_eq!(
            optional_argument_text(&result.result, "content"),
            Some("hello from file".to_owned())
        );

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_read_missing_file_reports_error() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: READ_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("/definitely/missing/file.txt".to_owned()),
                )]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert!(!error.message.contains("failed to read"));
        assert!(error.message.contains("No such file or directory"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_writes_file() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("output.txt");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: WRITE_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text(file_path.display().to_string()),
                    ),
                    (
                        CborValue::Text("content".to_owned()),
                        CborValue::Text("written content".to_owned()),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let result = reader.read_event().expect("read").expect("result");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, WRITE_TOOL_NAME);
        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "written content"
        );

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_write_missing_parent_reports_short_error() {
        let tempdir = TempDir::new().expect("tempdir");
        let missing_parent = tempdir.path().join("missing-parent");
        let file_path = missing_parent.join("child.txt");
        fs::write(&missing_parent, "not a dir").expect("write blocker");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: WRITE_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text(file_path.display().to_string()),
                    ),
                    (
                        CborValue::Text("content".to_owned()),
                        CborValue::Text("x".to_owned()),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, WRITE_TOOL_NAME);
        assert!(!error.message.contains("failed to create directories"));
        assert!(!error.message.contains(file_path.to_string_lossy().as_ref()));
        assert!(error.message.contains("Not a directory"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_write_directory_reports_short_error() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: WRITE_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text("/tmp".to_owned()),
                    ),
                    (
                        CborValue::Text("content".to_owned()),
                        CborValue::Text("x".to_owned()),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, WRITE_TOOL_NAME);
        assert!(!error.message.contains("failed to write"));
        assert!(error.message.contains("Is a directory"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_writes_file_creates_directories() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("a/b/c/deep.txt");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: WRITE_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text(file_path.display().to_string()),
                    ),
                    (
                        CborValue::Text("content".to_owned()),
                        CborValue::Text("deep content".to_owned()),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let result = reader.read_event().expect("read").expect("result");
        assert!(matches!(result, Event::ToolResult(_)));
        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "deep content"
        );

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn edit_read_failure_reports_short_reason() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: EDIT_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text("/definitely/missing/file.txt".to_owned()),
                    ),
                    (
                        CborValue::Text("edits".to_owned()),
                        CborValue::Array(vec![CborValue::Map(vec![
                            (
                                CborValue::Text("oldText".to_owned()),
                                CborValue::Text("a".to_owned()),
                            ),
                            (
                                CborValue::Text("newText".to_owned()),
                                CborValue::Text("b".to_owned()),
                            ),
                        ])]),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, EDIT_TOOL_NAME);
        assert!(!error.message.contains("failed to read"));
        assert!(error.message.contains("No such file or directory"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn edit_errors_use_short_reasons() {
        let tempdir = TempDir::new().expect("tempdir");
        let file_path = tempdir.path().join("edit.txt");
        fs::write(&file_path, "hello\nworld\n").expect("write");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: EDIT_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text(file_path.display().to_string()),
                    ),
                    (
                        CborValue::Text("edits".to_owned()),
                        CborValue::Array(vec![CborValue::Map(vec![
                            (
                                CborValue::Text("oldText".to_owned()),
                                CborValue::Text("missing".to_owned()),
                            ),
                            (
                                CborValue::Text("newText".to_owned()),
                                CborValue::Text("x".to_owned()),
                            ),
                        ])]),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, EDIT_TOOL_NAME);
        assert_eq!(error.message, "not found");

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_finds_files() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
        fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
        fs::write(
            tempdir.path().join("src/nested/mod.rs"),
            "pub fn two() {}\n",
        )
        .expect("write");
        fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: FIND_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("pattern".to_owned()),
                        CborValue::Text("**/*.rs".to_owned()),
                    ),
                    (
                        CborValue::Text("path".to_owned()),
                        CborValue::Text(tempdir.path().display().to_string()),
                    ),
                ]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let result = reader.read_event().expect("read").expect("result");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, FIND_TOOL_NAME);
        assert_eq!(cbor_int_field(&result.result, "matches"), Some(2));
        let output = cbor_map_text(&result.result, "output").expect("output");
        assert!(output.contains("src/lib.rs"));
        assert!(output.contains("src/nested/mod.rs"));
        assert!(!output.contains("README.md"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn extension_lists_directory_contents() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
        fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
        fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: LS_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(tempdir.path().display().to_string()),
                )]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let result = reader.read_event().expect("read").expect("result");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, LS_TOOL_NAME);
        assert_eq!(cbor_int_field(&result.result, "entries"), Some(3));
        let output = cbor_map_text(&result.result, "output").expect("output");
        assert!(output.contains(".env"));
        assert!(output.contains("README.md"));
        assert!(output.contains("src/"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn shell_tool_reports_progress_and_success() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: SHELL_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("printf hello".to_owned()),
                )]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let progress = reader.read_event().expect("read").expect("progress");
        assert!(matches!(progress, Event::ToolProgress(_)));

        let result = reader.read_event().expect("read").expect("result");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, SHELL_TOOL_NAME);
        assert_eq!(
            optional_argument_text(&result.result, "stdout"),
            Some("hello".to_owned())
        );

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    #[test]
    fn shell_tool_reports_failures_with_details() {
        let (mut reader, mut writer) = spawn_extension();
        drain_startup(&mut reader);

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: SHELL_TOOL_NAME.into(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("exit 7".to_owned()),
                )]),
            }))
            .expect("invoke");
        writer.flush().expect("flush");

        let _progress = reader.read_event().expect("read").expect("progress");

        let error = reader.read_event().expect("read").expect("error");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, SHELL_TOOL_NAME);
        assert!(error.message.contains("command exited with status 7"));
        assert!(error.details.is_some());

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect");
        writer.flush().expect("flush");
    }

    // -- Truncation ---------------------------------------------------------

    #[test]
    fn truncate_head_short_input_unchanged() {
        let input = "line 1\nline 2\nline 3";
        let result = truncate_head(input);
        assert!(!result.was_truncated);
        assert_eq!(result.content, input);
    }

    #[test]
    fn truncate_head_limits_by_lines() {
        let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        let input = lines.join("\n");
        let result = truncate_head(&input);
        assert!(result.was_truncated);
        assert!(result.content.contains("line 1\n"));
        assert!(result.content.contains("[Showing lines 1-"));
        assert!(
            result
                .content
                .contains("Use start_line and line_count to continue reading.")
        );
        // Should not contain lines beyond the limit.
        let content_before_notice = result.content.split("\n\n[").next().unwrap_or("");
        let kept_count = content_before_notice.lines().count();
        assert!(kept_count <= MAX_OUTPUT_LINES);
    }

    #[test]
    fn truncate_head_limits_by_bytes() {
        // Create input that's within line count but exceeds byte limit.
        let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let input = format!("first\n{big_line}\nthird");
        let result = truncate_head(&input);
        assert!(result.was_truncated);
        assert!(result.content.starts_with("first"));
        assert!(result.content.contains("[Showing lines 1-"));
    }

    #[test]
    fn grep_result_map_echoes_request_context_for_ui() {
        // The CLI renders grep completions using fields read back from
        // the tool result (pattern/path/glob/matches/output). Lock the
        // wire contract so a future shape change doesn't silently
        // regress the UI back to "grep: done".
        let with_glob = grep_result_map(
            "foo",
            "src",
            Some("*.rs"),
            Some(0),
            3,
            "src/a.rs:1:foo".to_owned(),
        );
        assert_eq!(cbor_map_text(&with_glob, "pattern"), Some("foo"));
        assert_eq!(cbor_map_text(&with_glob, "path"), Some("src"));
        assert_eq!(cbor_map_text(&with_glob, "glob"), Some("*.rs"));
        assert_eq!(cbor_int_field(&with_glob, "status"), Some(0));
        assert_eq!(cbor_int_field(&with_glob, "matches"), Some(3));
        assert_eq!(cbor_map_text(&with_glob, "output"), Some("src/a.rs:1:foo"));
        assert_eq!(cbor_int_field(&with_glob, "output_lines"), Some(1));
        assert_eq!(cbor_int_field(&with_glob, "output_bytes"), Some(14));

        // No-glob form omits the field entirely rather than emitting
        // an empty string.
        let no_glob = grep_result_map("foo", ".", None, Some(1), 0, "no matches found".to_owned());
        assert!(cbor_map_text(&no_glob, "glob").is_none());
        assert_eq!(cbor_int_field(&no_glob, "status"), Some(1));
        assert_eq!(cbor_int_field(&no_glob, "matches"), Some(0));
        assert_eq!(cbor_int_field(&no_glob, "output_lines"), Some(1));
        assert_eq!(cbor_int_field(&no_glob, "output_bytes"), Some(16));
    }

    #[test]
    fn command_details_value_records_stdout_and_stderr_stats() {
        let details = command_details_value(
            "echo hi".to_owned(),
            None,
            Some(0),
            "hi\nthere\n".to_owned(),
            "oops\n".to_owned(),
        );
        assert_eq!(cbor_int_field(&details, "stdout_lines"), Some(2));
        assert_eq!(cbor_int_field(&details, "stdout_bytes"), Some(9));
        assert_eq!(cbor_int_field(&details, "stderr_lines"), Some(1));
        assert_eq!(cbor_int_field(&details, "stderr_bytes"), Some(5));
    }

    #[test]
    fn is_grep_match_line_classifies_match_and_context_lines() {
        // Basic match / context forms.
        assert!(is_grep_match_line("foo/bar.rs:10:hello"));
        assert!(!is_grep_match_line("foo/bar.rs-10-hello"));

        // Paths containing `-` must not trip the classifier — the `-`
        // in "tool-exercise" has no digits after it, so the scan skips
        // past it to the real `:2:` separator.
        assert!(is_grep_match_line(
            "tmp/tool-exercise/beta/nested/notes.md:2:foo bar"
        ));
        assert!(!is_grep_match_line(
            "tmp/tool-exercise/beta/nested/notes.md-2-foo bar"
        ));
        assert!(is_grep_match_line(
            "tmp/tool-exercise/generated.txt:2:SECOND"
        ));

        // Group separator and blank lines are not matches.
        assert!(!is_grep_match_line("--"));
        assert!(!is_grep_match_line(""));
    }

    #[test]
    fn truncate_tail_short_input_unchanged() {
        let input = "line 1\nline 2\nline 3";
        let result = truncate_tail(input);
        assert!(!result.was_truncated);
        assert_eq!(result.content, input);
    }

    #[test]
    fn truncate_tail_keeps_last_lines() {
        let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        let input = lines.join("\n");
        let result = truncate_tail(&input);
        assert!(result.was_truncated);
        assert!(
            result
                .content
                .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
        );
        assert!(result.content.contains("[Showing lines"));
        // Should not contain the very first line.
        assert!(!result.content.contains("line 1\n"));
    }

    #[test]
    fn truncate_tail_limits_by_bytes() {
        let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let input = format!("first\nsecond\n{big_line}\nlast");
        let result = truncate_tail(&input);
        assert!(result.was_truncated);
        assert!(result.content.contains("last"));
        assert!(result.content.contains("[Showing lines"));
    }

    #[test]
    fn slice_lines_returns_requested_window() {
        let sliced = slice_lines("a\nb\nc\nd", 2, Some(2));
        assert_eq!(sliced.content, "b\nc");
        assert_eq!(sliced.start_line, 2);
        assert_eq!(sliced.line_count, 2);
    }

    #[test]
    fn slice_lines_clamps_past_end() {
        let sliced = slice_lines("a\nb\nc", 10, Some(5));
        assert_eq!(sliced.content, "");
        assert_eq!(sliced.start_line, 10);
        assert_eq!(sliced.line_count, 0);
    }

    #[test]
    fn read_file_honors_start_line_and_line_count() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("small.txt");
        std::fs::write(&path, "line 1\nline 2\nline 3\nline 4\nline 5\n").expect("write");

        let args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.display().to_string()),
            ),
            (
                CborValue::Text("start_line".to_owned()),
                CborValue::Integer(2.into()),
            ),
            (
                CborValue::Text("line_count".to_owned()),
                CborValue::Integer(3.into()),
            ),
        ]);
        let result = read_file(&args).expect("read");
        assert_eq!(
            cbor_map_text(&result, "content"),
            Some("line 2\nline 3\nline 4")
        );
        assert_eq!(cbor_int_field(&result, "start_line"), Some(2));
        assert_eq!(cbor_int_field(&result, "line_count"), Some(3));
        assert_eq!(cbor_int_field(&result, "total_lines"), Some(5));
    }

    #[test]
    fn read_file_rejects_invalid_line_arguments() {
        let args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("x".to_owned()),
            ),
            (
                CborValue::Text("start_line".to_owned()),
                CborValue::Integer(0.into()),
            ),
        ]);
        assert_eq!(
            read_file(&args).expect_err("start_line=0 should fail"),
            "start_line must be >= 1"
        );

        let args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("x".to_owned()),
            ),
            (
                CborValue::Text("line_count".to_owned()),
                CborValue::Integer(0.into()),
            ),
        ]);
        assert_eq!(
            read_file(&args).expect_err("line_count=0 should fail"),
            "line_count must be >= 1"
        );
    }

    #[test]
    fn read_file_truncates_large_output() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("big.txt");
        let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
        std::fs::write(&path, lines.join("\n")).expect("write");

        let args = CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        )]);
        let result = read_file(&args).expect("read");
        let content = cbor_map_text(&result, "content").expect("content field");
        assert!(content.contains("line 1\n"));
        assert!(content.contains("[Showing lines 1-"));
        assert!(content.contains("Use start_line and line_count to continue reading."));
        assert_eq!(cbor_int_field(&result, "start_line"), Some(1));
        assert_eq!(cbor_int_field(&result, "line_count"), Some(3000));
        assert_eq!(cbor_int_field(&result, "total_lines"), Some(3000));
    }

    #[test]
    fn run_find_double_star_matches_top_level_files() {
        // Regression: `**/*.rs` should match both nested AND
        // top-level Rust files. `globset`'s native `**` requires one
        // path separator; we work around that in `compile_find_glob`.
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
        fs::write(tempdir.path().join("top.rs"), "fn top() {}\n").expect("write top");
        fs::write(tempdir.path().join("src/lib.rs"), "fn nested() {}\n").expect("write nested");
        fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write readme");

        let args = CborValue::Map(vec![
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text("**/*.rs".to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(tempdir.path().display().to_string()),
            ),
        ]);
        let result = run_find(&args).expect("find");

        assert_eq!(cbor_int_field(&result, "matches"), Some(2));
        let output = cbor_map_text(&result, "output").expect("output");
        assert!(
            output.contains("top.rs"),
            "top-level match missing: {output}"
        );
        assert!(
            output.contains("src/lib.rs"),
            "nested match missing: {output}"
        );
        assert!(!output.contains("README.md"));
    }

    #[test]
    fn run_find_returns_matching_files() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
        fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
        fs::write(
            tempdir.path().join("src/nested/mod.rs"),
            "pub fn two() {}\n",
        )
        .expect("write");
        fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

        let args = CborValue::Map(vec![
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text("**/*.rs".to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(tempdir.path().display().to_string()),
            ),
        ]);
        let result = run_find(&args).expect("find");

        assert_eq!(cbor_int_field(&result, "matches"), Some(2));
        let output = cbor_map_text(&result, "output").expect("output");
        assert!(output.contains("src/lib.rs"));
        assert!(output.contains("src/nested/mod.rs"));
        assert!(!output.contains("README.md"));
    }

    #[test]
    fn run_ls_lists_directory_contents() {
        let tempdir = TempDir::new().expect("tempdir");
        fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
        fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
        fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

        let args = CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        )]);
        let result = run_ls(&args).expect("ls");

        assert_eq!(cbor_int_field(&result, "entries"), Some(3));
        let output = cbor_map_text(&result, "output").expect("output");
        assert!(output.contains(".env"));
        assert!(output.contains("README.md"));
        assert!(output.contains("src/"));
    }
}
