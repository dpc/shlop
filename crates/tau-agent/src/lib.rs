//! First-party deterministic agent process.
//!
//! The current behavior is intentionally simple and command-like:
//!
//! - `read <path>` -> `fs.read`
//! - `shell <command>` -> `shell.exec`
//! - anything else -> `demo.echo`

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_proto::{
    CborValue, ChatMessage, ClientKind, Event, EventName, EventReader, EventSelector, EventWriter,
    LifecycleHello, LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolRequest,
    ToolResult,
};

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));
    let mut next_call_number = 1_u64;
    let mut pending_sessions = HashMap::new();

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::MessageUser),
            EventSelector::Exact(EventName::ToolResult),
            EventSelector::Exact(EventName::ToolError),
            EventSelector::Exact(EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("agent ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::MessageUser(message) => {
                let call_id = format!("call-{next_call_number}");
                next_call_number += 1;
                pending_sessions.insert(call_id.clone(), message.session_id.clone());
                let request = request_for_user_message(call_id, message.text);
                writer.write_event(&Event::ToolRequest(request))?;
                writer.flush()?;
            }
            Event::ToolResult(ToolResult {
                call_id,
                tool_name,
                result,
            }) => {
                let session_id = pending_sessions.remove(&call_id).unwrap_or_default();
                writer.write_event(&Event::MessageAgent(ChatMessage {
                    session_id,
                    text: format_tool_result(&tool_name, &result),
                }))?;
                writer.flush()?;
            }
            Event::ToolError(ToolError {
                call_id,
                tool_name,
                message,
                details,
            }) => {
                let session_id = pending_sessions.remove(&call_id).unwrap_or_default();
                writer.write_event(&Event::MessageAgent(ChatMessage {
                    session_id,
                    text: format_tool_error(&tool_name, &message, details.as_ref()),
                }))?;
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

fn request_for_user_message(call_id: String, text: String) -> ToolRequest {
    if let Some(path) = text.strip_prefix("read ") {
        return ToolRequest {
            call_id,
            tool_name: "fs.read".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.trim().to_owned()),
            )]),
        };
    }
    if let Some(command) = text.strip_prefix("shell ") {
        return ToolRequest {
            call_id,
            tool_name: "shell.exec".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text(command.trim().to_owned()),
            )]),
        };
    }
    ToolRequest {
        call_id,
        tool_name: "demo.echo".to_owned(),
        arguments: CborValue::Text(text),
    }
}

fn format_tool_result(tool_name: &str, result: &CborValue) -> String {
    match tool_name {
        "fs.read" => {
            let path = map_text(result, "path").unwrap_or_else(|| "<unknown>".to_owned());
            let content = map_text(result, "content").unwrap_or_else(|| format!("{result:?}"));
            format!("fs.read {path}:\n{content}")
        }
        "shell.exec" => {
            let status = map_integer(result, "status")
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            let stdout = map_text(result, "stdout").unwrap_or_default();
            let stderr = map_text(result, "stderr").unwrap_or_default();
            let mut text = format!("shell.exec status {status}");
            if !stdout.is_empty() {
                text.push_str(&format!("\nstdout:\n{stdout}"));
            }
            if !stderr.is_empty() {
                text.push_str(&format!("\nstderr:\n{stderr}"));
            }
            text
        }
        _ => format!("{tool_name} returned: {result:?}"),
    }
}

fn format_tool_error(tool_name: &str, message: &str, details: Option<&CborValue>) -> String {
    if tool_name == "shell.exec" {
        if let Some(details) = details {
            let stderr = map_text(details, "stderr").unwrap_or_default();
            if stderr.is_empty() {
                return format!("{tool_name} failed: {message}");
            }
            return format!("{tool_name} failed: {message}\nstderr:\n{stderr}");
        }
    }
    format!("{tool_name} failed: {message}")
}

fn map_text(value: &CborValue, key: &str) -> Option<String> {
    match value {
        CborValue::Map(entries) => {
            entries
                .iter()
                .find_map(|(entry_key, entry_value)| match (entry_key, entry_value) {
                    (CborValue::Text(entry_key), CborValue::Text(text)) if entry_key == key => {
                        Some(text.clone())
                    }
                    _ => None,
                })
        }
        _ => None,
    }
}

fn map_integer(value: &CborValue, key: &str) -> Option<i128> {
    match value {
        CborValue::Map(entries) => {
            entries
                .iter()
                .find_map(|(entry_key, entry_value)| match (entry_key, entry_value) {
                    (CborValue::Text(entry_key), CborValue::Integer(number))
                        if entry_key == key =>
                    {
                        i128::try_from(*number).ok()
                    }
                    _ => None,
                })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_routes_to_fs_read_when_requested() {
        let request = request_for_user_message("call-1".to_owned(), "read Cargo.toml".to_owned());
        assert_eq!(request.tool_name, "fs.read");
        assert_eq!(
            request.arguments,
            CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("Cargo.toml".to_owned()),
            )])
        );
    }

    #[test]
    fn user_message_routes_to_shell_exec_when_requested() {
        let request = request_for_user_message("call-1".to_owned(), "shell printf hi".to_owned());
        assert_eq!(request.tool_name, "shell.exec");
    }
}
