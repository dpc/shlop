//! Filesystem-oriented tool extension.
//!
//! This crate keeps the original deterministic `demo.echo` tool used by the
//! first vertical slice and adds a real `fs.read` tool for coding workflows.

use std::error::Error;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use tau_proto::{
    CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolRegister, ToolResult,
    ToolSpec,
};

/// The original deterministic tool used by the first vertical slice.
pub const DEMO_ECHO_TOOL_NAME: &str = "demo.echo";
/// A real filesystem tool for reading one file.
pub const FS_READ_TOOL_NAME: &str = "fs.read";

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
    ] {
        writer.write_event(&Event::ToolRegister(ToolRegister { tool }))?;
    }
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("filesystem tools ready".to_owned()),
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
                match read_file_result(&invoke.arguments) {
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

fn read_file_result(arguments: &CborValue) -> Result<CborValue, String> {
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

fn argument_text(arguments: &CborValue, key: &str) -> Result<String, String> {
    match arguments {
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(entry_key, entry_value)| match (entry_key, entry_value) {
                (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                    Some(value.clone())
                }
                _ => None,
            })
            .ok_or_else(|| format!("missing string argument: {key}")),
        _ => Err("tool arguments must be a CBOR map".to_owned()),
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

    #[test]
    fn extension_registers_fs_read_and_reads_file_content() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let file_path = tempdir.path().join("README.txt");
        fs::write(&file_path, "hello from file").expect("fixture file should be written");

        let (mut reader, mut writer) = spawn_extension();
        assert!(matches!(
            reader
                .read_event()
                .expect("read")
                .expect("hello should arrive"),
            Event::LifecycleHello(_)
        ));
        assert!(matches!(
            reader
                .read_event()
                .expect("read")
                .expect("subscribe should arrive"),
            Event::LifecycleSubscribe(_)
        ));
        let first_register = reader
            .read_event()
            .expect("read")
            .expect("first register should arrive");
        let second_register = reader
            .read_event()
            .expect("read")
            .expect("second register should arrive");
        let registered_names = [first_register, second_register]
            .into_iter()
            .filter_map(|event| match event {
                Event::ToolRegister(register) => Some(register.tool.name),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            registered_names
                .iter()
                .any(|name| name == DEMO_ECHO_TOOL_NAME)
        );
        assert!(
            registered_names
                .iter()
                .any(|name| name == FS_READ_TOOL_NAME)
        );
        assert!(matches!(
            reader
                .read_event()
                .expect("read")
                .expect("ready should arrive"),
            Event::LifecycleReady(_)
        ));

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: FS_READ_TOOL_NAME.to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                )]),
            }))
            .expect("invoke should send");
        writer.flush().expect("writer should flush");

        let result = reader
            .read_event()
            .expect("read")
            .expect("result should arrive");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_name, FS_READ_TOOL_NAME);
        let content = argument_text(&result.result, "content").expect("content should be present");
        assert_eq!(content, "hello from file");

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect should send");
        writer.flush().expect("writer should flush");
    }

    #[test]
    fn extension_reports_read_errors_cleanly() {
        let (mut reader, mut writer) = spawn_extension();
        for expected_name in [
            EventName::LifecycleHello,
            EventName::LifecycleSubscribe,
            EventName::ToolRegister,
            EventName::ToolRegister,
            EventName::LifecycleReady,
        ] {
            assert_eq!(
                reader
                    .read_event()
                    .expect("read")
                    .expect("startup event should arrive")
                    .name(),
                expected_name
            );
        }

        writer
            .write_event(&Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: FS_READ_TOOL_NAME.to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("/definitely/missing/file.txt".to_owned()),
                )]),
            }))
            .expect("invoke should send");
        writer.flush().expect("writer should flush");

        let error = reader
            .read_event()
            .expect("read")
            .expect("error should arrive");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.tool_name, FS_READ_TOOL_NAME);
        assert!(error.message.contains("failed to read"));

        writer
            .write_event(&Event::LifecycleDisconnect(
                tau_proto::LifecycleDisconnect { reason: None },
            ))
            .expect("disconnect should send");
        writer.flush().expect("writer should flush");
    }
}
