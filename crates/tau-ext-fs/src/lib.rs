//! Filesystem and shell tool extension.
//!
//! Provides `read`, `write`, and `bash` tools.
//!
//! The `echo` tool is available for testing via `include_echo: true`.

use std::error::Error;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

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
    CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolProgress, ToolRegister,
    ToolResult, ToolSpec,
};

pub const ECHO_TOOL_NAME: &str = "echo";
pub const READ_TOOL_NAME: &str = "read";
pub const WRITE_TOOL_NAME: &str = "write";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const SHELL_TOOL_NAME: &str = "shell";

/// Runs the extension on stdin/stdout (production, no echo).
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout(), false)
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// When `include_echo` is true, registers the `echo` tool (for testing).
pub fn run<R, W>(reader: R, writer: W, include_echo: bool) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-fs".to_owned(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::ToolInvoke),
            EventSelector::Exact(tau_proto::EventName::LifecycleDisconnect),
        ],
    }))?;
    if include_echo {
        writer.write_event(&Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: ECHO_TOOL_NAME.into(),
                description: Some("Echo the provided payload unchanged".to_owned()),
                parameters: None,
            },
        }))?;
    }
    for tool in [
        ToolSpec {
            name: READ_TOOL_NAME.into(),
            description: Some(
                "Read the contents of a file. Returns the file path and text content. \
                 Use this instead of shell commands like cat or head."
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file to read"
                    }
                },
                "required": ["path"]
            })),
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
                        "description": "Absolute or relative path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The text content to write to the file"
                    }
                },
                "required": ["path", "content"]
            })),
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
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements matched against the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text to find. Must be unique in the file."
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "Replacement text."
                                }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            })),
        },
        ToolSpec {
            name: SHELL_TOOL_NAME.into(),
            description: Some(
                "Execute a shell command via `sh -c`. Returns stdout, stderr, and \
                 exit status. Use this for running builds, tests, git commands, and \
                 other shell operations."
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
    loop {
        let Some(event) = reader.read_event()? else {
            break;
        };
        match event {
            Event::ToolInvoke(invoke) => {
                let tx = tx.clone();
                let sem = Arc::clone(&sem);
                let include_echo = include_echo;
                std::thread::spawn(move || {
                    let _permit = sem.acquire();
                    dispatch_tool_invoke(invoke, include_echo, &tx);
                });
            }
            Event::LifecycleDisconnect(_) => break,
            _ => {}
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
// read
// ---------------------------------------------------------------------------

fn read_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let path_buf = PathBuf::from(&path);
    let content = fs::read_to_string(&path_buf)
        .map_err(|error| format!("failed to read {}: {error}", path_buf.display()))?;
    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text(content),
        ),
    ]))
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
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create directories for {}: {error}",
                    path_buf.display()
                )
            })?;
        }
    }

    let bytes_written = content.len();
    fs::write(&path_buf, &content)
        .map_err(|error| format!("failed to write {}: {error}", path_buf.display()))?;

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("bytes_written".to_owned()),
            CborValue::Integer((bytes_written as i64).into()),
        ),
    ]))
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

fn edit_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let path_buf = PathBuf::from(&path);

    let original = fs::read_to_string(&path_buf)
        .map_err(|e| format!("failed to read {}: {e}", path_buf.display()))?;

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
            return Err(format!(
                "oldText not found in {}: {:?}",
                path_buf.display(),
                truncate(old_text, 80)
            ));
        };
        let end = start + old_text.len();

        // Check uniqueness: there should be no second match.
        if original[start + 1..].contains(old_text) {
            return Err(format!(
                "oldText matches multiple locations in {}: {:?}",
                path_buf.display(),
                truncate(old_text, 80)
            ));
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
            return Err("edits overlap — merge nearby changes into one edit".to_owned());
        }
    }

    // Apply replacements from end to start.
    let mut result = original.clone();
    for (start, end, new_text) in &replacements {
        result.replace_range(*start..*end, new_text);
    }

    fs::write(&path_buf, &result)
        .map_err(|e| format!("failed to write {}: {e}", path_buf.display()))?;

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("edits_applied".to_owned()),
            CborValue::Integer((edits.len() as i64).into()),
        ),
    ]))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
    let result = command_details_value(
        command.clone(),
        cwd.clone(),
        status_code,
        wait.stdout,
        wait.stderr,
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
            Some(WaitResult { status, stdout, stderr })
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
            CborValue::Text(stdout),
        ),
        (
            CborValue::Text("stderr".to_owned()),
            CborValue::Text(stderr),
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

    fn spawn_extension() -> (
        EventReader<BufReader<UnixStream>>,
        EventWriter<BufWriter<UnixStream>>,
    ) {
        let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair should open");
        let reader_stream = runtime_stream
            .try_clone()
            .expect("runtime reader clone should succeed");
        thread::spawn(move || {
            run(reader_stream, runtime_stream, true).expect("extension should run");
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
            EventName::LifecycleHello,
            EventName::LifecycleSubscribe,
            EventName::ToolRegister,
            EventName::ToolRegister,
            EventName::ToolRegister,
            EventName::ToolRegister,
            EventName::ToolRegister,
            EventName::LifecycleReady,
        ] {
            let event = reader
                .read_event()
                .expect("read")
                .expect("startup event should arrive");
            assert_eq!(event.name(), expected);
        }
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
        assert!(error.message.contains("failed to read"));

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
}
