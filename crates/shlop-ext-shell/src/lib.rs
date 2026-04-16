//! Shell-oriented tool extension.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::Command;

use shlop_proto::{
    CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolProgress, ToolRegister,
    ToolResult, ToolSpec,
};

/// The shell execution tool exposed by this extension.
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
        client_name: "shlop-ext-shell".to_owned(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(shlop_proto::EventName::ToolInvoke),
            EventSelector::Exact(shlop_proto::EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::ToolRegister(ToolRegister {
        tool: ToolSpec {
            name: SHELL_EXEC_TOOL_NAME.to_owned(),
            description: Some("Run one shell command with captured output".to_owned()),
        },
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("shell tool ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        match reader.read_event()? {
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
    use std::io::{BufReader, BufWriter};
    use std::os::unix::net::UnixStream;
    use std::thread;

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

    #[test]
    fn shell_tool_reports_progress_and_success() {
        let (mut reader, mut writer) = spawn_extension();
        for _ in 0..4 {
            let _ = reader.read_event().expect("startup event should arrive");
        }

        writer
            .write_event(&Event::ToolInvoke(shlop_proto::ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: SHELL_EXEC_TOOL_NAME.to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("printf hello".to_owned()),
                )]),
            }))
            .expect("invoke should send");
        writer.flush().expect("writer should flush");

        let progress = reader.read_event().expect("progress should arrive");
        assert!(matches!(progress, Event::ToolProgress(_)));
        let result = reader.read_event().expect("result should arrive");
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
                shlop_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect should send");
        writer.flush().expect("writer should flush");
    }

    #[test]
    fn shell_tool_reports_failures_with_details() {
        let (mut reader, mut writer) = spawn_extension();
        for _ in 0..4 {
            let _ = reader.read_event().expect("startup event should arrive");
        }

        writer
            .write_event(&Event::ToolInvoke(shlop_proto::ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: SHELL_EXEC_TOOL_NAME.to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("exit 7".to_owned()),
                )]),
            }))
            .expect("invoke should send");
        writer.flush().expect("writer should flush");

        let _progress = reader.read_event().expect("progress should arrive");
        let error = reader.read_event().expect("error should arrive");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, SHELL_EXEC_TOOL_NAME);
        assert!(error.message.contains("command exited with status 7"));
        assert!(error.details.is_some());

        writer
            .write_event(&Event::LifecycleDisconnect(
                shlop_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect should send");
        writer.flush().expect("writer should flush");
    }
}
