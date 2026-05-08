//! Core delegate tool extension.
//!
//! Registers `delegate`. A tool invocation is translated into an
//! `ExtAgentQuery` side conversation. When the harness returns the
//! side agent's final text, that text is returned as the tool result.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_proto::{
    Ack, CborValue, ClientKind, Event, EventSelector, ExtAgentQuery, Frame, FrameReader,
    FrameWriter, Hello, LogEventId, Message, PROTOCOL_VERSION, Ready, Subscribe, ToolError,
    ToolInvoke, ToolRegister, ToolResult, ToolSideEffects, ToolSpec,
};

pub const LOG_TARGET: &str = "core-delegate";
pub const TOOL_NAME: &str = "delegate";

const DELEGATE_PREFIX: &str = "You are a delegated sub-agent. Complete the task below fully using your tools — don't gold-plate, but don't leave it half-done.\n\nReturn only the final information useful to the parent agent: the answer, plus any absolute file paths and short code snippets that are load-bearing. Do not include reasoning, tool history, or status chatter. Do not write report/summary .md files; the parent reads your final message, not files you create.\n\nTask:\n";

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-core-delegate".into(),
        client_kind: ClientKind::Tool,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::TOOL_INVOKE),
            EventSelector::Exact(tau_proto::EventName::EXTENSION_AGENT_QUERY_RESULT),
        ],
    })))?;
    writer.write_frame(&Frame::Event(Event::ToolRegister(ToolRegister {
        tool: tool_spec(),
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("core-delegate ready".to_owned()),
    })))?;
    writer.flush()?;

    let mut pending: HashMap<String, (tau_proto::ToolCallId, tau_proto::ToolName)> = HashMap::new();
    let mut next_query_id: u64 = 0;

    loop {
        let Some(frame) = reader.read_frame()? else {
            break;
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::ToolInvoke(invoke)) => {
                handle_tool_invoke(invoke, &mut pending, &mut next_query_id, &mut writer)?
            }
            Frame::Event(Event::ExtAgentQueryResult(result)) => {
                if let Some((call_id, tool_name)) = pending.remove(&result.query_id) {
                    if let Some(error) = result.error {
                        writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                            call_id,
                            tool_name,
                            message: error,
                            details: None,
                        })))?;
                    } else {
                        writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                            call_id,
                            tool_name,
                            result: CborValue::Text(result.text),
                        })))?;
                    }
                    writer.flush()?;
                }
            }
            Frame::Message(Message::Disconnect(_)) => break,
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &mut writer)?;
        }
    }
    Ok(())
}

fn handle_tool_invoke<W: Write>(
    invoke: ToolInvoke,
    pending: &mut HashMap<String, (tau_proto::ToolCallId, tau_proto::ToolName)>,
    next_query_id: &mut u64,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    if invoke.tool_name.as_str() != TOOL_NAME {
        writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            message: "unknown tool".to_owned(),
            details: None,
        })))?;
        writer.flush()?;
        return Ok(());
    }

    let parsed = match parse_args(&invoke.arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                message,
                details: Some(invoke.arguments),
            })))?;
            writer.flush()?;
            return Ok(());
        }
    };

    let query_id = format!("delegate-{next_query_id}");
    *next_query_id += 1;
    let call_id = invoke.call_id.clone();
    pending.insert(query_id.clone(), (invoke.call_id, invoke.tool_name));
    writer.write_frame(&Frame::Event(Event::ExtAgentQuery(ExtAgentQuery {
        query_id,
        instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
        // Hand the parent call_id and the agent-supplied task name to
        // the harness so it can route sub-agent progress
        // (`DelegateProgress`) under this tool block and the CLI can
        // render `delegate [name] …`.
        tool_call_id: Some(call_id),
        task_name: Some(parsed.task_name),
    })))?;
    writer.flush()?;
    Ok(())
}

fn ack_log_event<W: Write>(
    id: LogEventId,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
    writer.flush()?;
    Ok(())
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_NAME.into(),
        description: Some(
            "Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple `read_only: true` delegations dispatched in the same turn run concurrently. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent's conversation is branched from the most recent completed user turn here, so it inherits prior completed turns as cached context — but it does NOT see the in-flight turn that triggered this delegation. Brief it for what is unique to the sub-task; you don't need to re-explain context already established earlier in this conversation."
                .to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task for the sub-agent. The sub-agent inherits this conversation's completed turns as context, so don't re-explain what's already been said — but DO state what is specific to the sub-task: the goal, any new constraints, and exactly what information you need back. Terse command-style prompts produce shallow work."
                },
                "read_only": {
                    "type": "boolean",
                    "description": "Set true ONLY when the sub-task is fully read-only (no file writes, no shell commands with side effects, no network mutation, no nested delegations that mutate). Read-only delegations dispatched in the same turn run concurrently with each other and with other read-only tool calls — set this whenever applicable to enable parallelism. Default: false (treated as `Mutating` — runs sequentially with other mutating work)."
                }
            },
            "required": ["task_name", "prompt"]
        })),
        // Conservative default at registration time. The harness
        // overrides this per-call when `read_only: true` is set in the
        // arguments, so two read-only delegations from the same agent
        // turn can dispatch concurrently.
        side_effects: ToolSideEffects::Mutating,
    }
}

struct DelegateArgs {
    task_name: String,
    prompt: String,
}

fn parse_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut prompt = None;
    let mut task_name = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "prompt" => match v {
                CborValue::Text(text) => prompt = Some(text.clone()),
                _ => return Err("`prompt` must be a string".to_owned()),
            },
            "task_name" => match v {
                CborValue::Text(text) => task_name = Some(text.clone()),
                _ => return Err("`task_name` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let prompt = prompt.ok_or_else(|| "missing string argument: prompt".to_owned())?;
    if prompt.trim().is_empty() {
        return Err("`prompt` must not be empty".to_owned());
    }
    let task_name = task_name.ok_or_else(|| "missing string argument: task_name".to_owned())?;
    if task_name.trim().is_empty() {
        return Err("`task_name` must not be empty".to_owned());
    }
    Ok(DelegateArgs { task_name, prompt })
}
