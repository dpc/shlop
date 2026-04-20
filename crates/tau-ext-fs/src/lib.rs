//! Filesystem and shell tool extension.
//!
//! Provides `fs.read`, `fs.write`, `shell.exec`, and the deterministic
//! `demo.echo` tool used by the first vertical slice.

use std::error::Error;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::Command;

use tau_proto::{
    CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolProgress, ToolRegister,
    ToolResult, ToolSpec,
};

pub const DEMO_ECHO_TOOL_NAME: &str = "demo.echo";
pub const FS_READ_TOOL_NAME: &str = "fs.read";
pub const FS_WRITE_TOOL_NAME: &str = "fs.write";
pub const SHELL_EXEC_TOOL_NAME: &str = "shell.exec";

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
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
    for tool in [
        ToolSpec {
            name: DEMO_ECHO_TOOL_NAME.to_owned(),
            description: Some("Echo the provided payload unchanged".to_owned()),
        },
        ToolSpec {
            name: FS_READ_TOOL_NAME.to_owned(),
            description: Some(
                "Read the contents of a file. Returns the file path and text content. \
                 Use this instead of shell commands like cat or head."
                    .to_owned(),
            ),
        },
        ToolSpec {
            name: FS_WRITE_TOOL_NAME.to_owned(),
            description: Some(
                "Write content to a file, creating it if it does not exist. \
                 Takes a path and content string. Returns the path and bytes written."
                    .to_owned(),
            ),
        },
        ToolSpec {
            name: SHELL_EXEC_TOOL_NAME.to_owned(),
            description: Some(
                "Execute a bash command in the current working directory. \
                 Returns stdout, stderr, and exit status. \
                 Use this for running builds, tests, git commands, and other shell operations."
                    .to_owned(),
            ),
        },
    ] {
        writer.write_event(&Event::ToolRegister(ToolRegister { tool }))?;
    }
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("filesystem and shell tools ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::ToolInvoke(invoke) if invoke.tool_name == DEMO_ECHO_TOOL_NAME => {
                writer.write_event(&Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: invoke.arguments,
                }))?;
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) if invoke.tool_name == FS_READ_TOOL_NAME => {
                match read_file(&invoke.arguments) {
                    Ok(result) => {
                        writer.write_event(&Event::ToolResult(ToolResult {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            result,
                        }))?;
                    }
                    Err(error) => {
                        writer.write_event(&Event::ToolError(ToolError {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            message: error,
                            details: None,
                        }))?;
                    }
                }
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) if invoke.tool_name == FS_WRITE_TOOL_NAME => {
                match write_file(&invoke.arguments) {
                    Ok(result) => {
                        writer.write_event(&Event::ToolResult(ToolResult {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            result,
                        }))?;
                    }
                    Err(error) => {
                        writer.write_event(&Event::ToolError(ToolError {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            message: error,
                            details: None,
                        }))?;
                    }
                }
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) if invoke.tool_name == SHELL_EXEC_TOOL_NAME => {
                writer.write_event(&Event::ToolProgress(ToolProgress {
                    call_id: invoke.call_id.clone(),
                    tool_name: invoke.tool_name.clone(),
                    message: Some("running shell command".to_owned()),
                    progress: None,
                }))?;
                match run_command(&invoke.arguments) {
                    Ok(result) => {
                        writer.write_event(&Event::ToolResult(ToolResult {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            result,
                        }))?;
                    }
                    Err((message, details)) => {
                        writer.write_event(&Event::ToolError(ToolError {
                            call_id: invoke.call_id,
                            tool_name: invoke.tool_name,
                            message,
                            details,
                        }))?;
                    }
                }
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) => {
                writer.write_event(&Event::ToolError(ToolError {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    message: "unknown tool".to_owned(),
                    details: None,
                }))?;
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// fs.read
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
// fs.write
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
// shell.exec
// ---------------------------------------------------------------------------

fn run_command(arguments: &CborValue) -> Result<CborValue, (String, Option<CborValue>)> {
    let command = argument_text(arguments, "command").map_err(|message| (message, None))?;
    let cwd = optional_argument_text(arguments, "cwd");

    let mut child = Command::new("sh");
    child.arg("-lc").arg(&command);
    if let Some(cwd) = &cwd {
        child.current_dir(cwd);
    }

    let output = child.output().map_err(|error| {
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

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let status_code = output.status.code();
    let result = command_details_value(command.clone(), cwd.clone(), status_code, stdout, stderr);

    if output.status.success() {
        Ok(result)
    } else {
        Err((
            format!(
                "command exited with status {}",
                status_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
            Some(result),
        ))
    }
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
    match arguments {
        CborValue::Map(entries) => {
            entries
                .iter()
                .find_map(|(entry_key, entry_value)| match (entry_key, entry_value) {
                    (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                        Some(value.clone())
                    }
                    _ => None,
                })
        }
        _ => None,
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
            run(reader_stream, runtime_stream).expect("extension should run");
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
                call_id: "call-1".to_owned(),
                tool_name: FS_READ_TOOL_NAME.to_owned(),
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
        assert_eq!(result.tool_name, FS_READ_TOOL_NAME);
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
                call_id: "call-1".to_owned(),
                tool_name: FS_READ_TOOL_NAME.to_owned(),
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
                call_id: "call-1".to_owned(),
                tool_name: FS_WRITE_TOOL_NAME.to_owned(),
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
        assert_eq!(result.tool_name, FS_WRITE_TOOL_NAME);
        assert_eq!(fs::read_to_string(&file_path).expect("read back"), "written content");

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
                call_id: "call-1".to_owned(),
                tool_name: FS_WRITE_TOOL_NAME.to_owned(),
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
        assert_eq!(fs::read_to_string(&file_path).expect("read back"), "deep content");

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
                call_id: "call-1".to_owned(),
                tool_name: SHELL_EXEC_TOOL_NAME.to_owned(),
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
        assert_eq!(result.tool_name, SHELL_EXEC_TOOL_NAME);
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
                call_id: "call-1".to_owned(),
                tool_name: SHELL_EXEC_TOOL_NAME.to_owned(),
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
        assert_eq!(error.tool_name, SHELL_EXEC_TOOL_NAME);
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
