//! Tool registry: dispatches a `ToolInvoke` to the right handler.

use tau_proto::{CborValue, Event, ToolError, ToolProgress, ToolResult};

use crate::config::ShellConfig;

pub(crate) mod edit;
pub(crate) mod find;
pub(crate) mod grep;
pub(crate) mod ls;
pub(crate) mod read;
pub(crate) mod shell;
pub(crate) mod write;

#[cfg(any(test, feature = "echo-agent"))]
pub const ECHO_TOOL_NAME: &str = "echo";
pub const READ_TOOL_NAME: &str = "read";
pub const WRITE_TOOL_NAME: &str = "write";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const SHELL_TOOL_NAME: &str = "shell";
pub const GREP_TOOL_NAME: &str = "grep";
pub const FIND_TOOL_NAME: &str = "find";
pub const LS_TOOL_NAME: &str = "ls";

/// Execute a tool and return the response event(s).
pub(crate) fn execute_tool(
    invoke: tau_proto::ToolInvoke,
    shell_config: &ShellConfig,
) -> Vec<Event> {
    let error_details = standard_tool_error_details(&invoke.tool_name, &invoke.arguments);

    #[cfg(any(test, feature = "echo-agent"))]
    if invoke.tool_name == ECHO_TOOL_NAME {
        return vec![Event::ToolResult(ToolResult {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            result: invoke.arguments,
            originator: tau_proto::PromptOriginator::User,
        })];
    }

    if invoke.tool_name == READ_TOOL_NAME {
        return wrap_pure(invoke, error_details, read::read_file);
    }
    if invoke.tool_name == WRITE_TOOL_NAME {
        return wrap_pure(invoke, error_details, write::write_file);
    }
    if invoke.tool_name == EDIT_TOOL_NAME {
        return wrap_pure(invoke, error_details, edit::edit_file);
    }
    if invoke.tool_name == GREP_TOOL_NAME {
        return wrap_pure(invoke, error_details, grep::run_grep);
    }
    if invoke.tool_name == FIND_TOOL_NAME {
        return wrap_pure(invoke, error_details, find::run_find);
    }
    if invoke.tool_name == LS_TOOL_NAME {
        return wrap_pure(invoke, error_details, ls::run_ls);
    }

    if invoke.tool_name == SHELL_TOOL_NAME {
        let mut events = vec![Event::ToolProgress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("running shell command".to_owned()),
            progress: None,
        })];
        match shell::run_command(&invoke.arguments, shell_config) {
            Ok(result) => events.push(Event::ToolResult(ToolResult {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                result,
                originator: tau_proto::PromptOriginator::User,
            })),
            Err((message, details)) => events.push(Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message,
                details,
                originator: tau_proto::PromptOriginator::User,
            })),
        }
        return events;
    }

    vec![Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        message: "unknown tool".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
    })]
}

/// Common Ok/Err → Result/Error wrapping for tools whose handler is a
/// pure `(arguments) -> Result<CborValue, String>`.
fn wrap_pure(
    invoke: tau_proto::ToolInvoke,
    error_details: Option<CborValue>,
    handler: fn(&CborValue) -> Result<CborValue, String>,
) -> Vec<Event> {
    match handler(&invoke.arguments) {
        Ok(result) => vec![Event::ToolResult(ToolResult {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            result,
            originator: tau_proto::PromptOriginator::User,
        })],
        Err(error) => vec![Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            message: error,
            details: error_details,
            originator: tau_proto::PromptOriginator::User,
        })],
    }
}

fn standard_tool_error_details(tool_name: &str, arguments: &CborValue) -> Option<CborValue> {
    match tool_name {
        READ_TOOL_NAME | WRITE_TOOL_NAME | EDIT_TOOL_NAME | GREP_TOOL_NAME | FIND_TOOL_NAME
        | LS_TOOL_NAME => Some(arguments.clone()),
        _ => None,
    }
}
