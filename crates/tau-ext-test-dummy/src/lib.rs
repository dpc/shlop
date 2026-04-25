use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use tau_proto::{
    ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello, LifecycleReady,
    LifecycleSubscribe, PROTOCOL_VERSION, ToolError, ToolRegister, ToolResult, ToolSideEffects,
    ToolSpec,
};

pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
    T: Rng + ?Sized,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-test-dummy".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::TOOL_INVOKE),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    writer.write_event(&Event::ToolRegister(ToolRegister {
        tool: ToolSpec {
            name: RESTART_TEST_DUMMY_TOOL_NAME.into(),
            description: Some(
                "Test-only tool that randomly restarts the dummy extension or returns an error"
                    .to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            })),
            side_effects: ToolSideEffects::Mutating,
        },
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("test dummy tools ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            break;
        };
        let (_, inner) = event.peel_log();
        match inner {
            Event::ToolInvoke(invoke) if invoke.tool_name == RESTART_TEST_DUMMY_TOOL_NAME => {
                if rng.gen_bool(0.5) {
                    writer.flush()?;
                    return Ok(());
                }
                writer.write_event(&Event::ToolError(ToolError {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    message: "restarting failed".to_owned(),
                    details: None,
                }))?;
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) => {
                writer.write_event(&Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: tau_proto::CborValue::Map(Vec::new()),
                }))?;
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => break,
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tau_proto::{Event, EventReader, ToolInvoke};

    use super::*;

    fn invoke_restart() -> Event {
        Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: RESTART_TEST_DUMMY_TOOL_NAME.into(),
            arguments: tau_proto::CborValue::Map(Vec::new()),
        })
    }

    #[test]
    fn restart_tool_can_return_error() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer.write_event(&invoke_restart()).expect("write invoke");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        let mut rng = StdRng::seed_from_u64(1);
        run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        let hello = reader
            .read_event()
            .expect("read")
            .expect("hello should exist");
        assert!(matches!(hello, Event::LifecycleHello(_)));
        let subscribe = reader
            .read_event()
            .expect("read")
            .expect("subscribe should exist");
        assert!(matches!(subscribe, Event::LifecycleSubscribe(_)));
        let register = reader
            .read_event()
            .expect("read")
            .expect("register should exist");
        assert!(matches!(register, Event::ToolRegister(_)));
        let ready = reader
            .read_event()
            .expect("read")
            .expect("ready should exist");
        assert!(matches!(ready, Event::LifecycleReady(_)));
        let error = reader
            .read_event()
            .expect("read")
            .expect("error should exist");
        let Event::ToolError(error) = error else {
            panic!("expected tool error");
        };
        assert_eq!(error.message, "restarting failed");
        assert!(reader.read_event().expect("read eof").is_none());
    }

    #[test]
    fn restart_tool_can_exit_without_reply() {
        let mut input = Vec::new();
        let mut writer = EventWriter::new(&mut input);
        writer.write_event(&invoke_restart()).expect("write invoke");
        writer.flush().expect("flush");

        let mut output = Vec::new();
        let mut rng = StdRng::seed_from_u64(2);
        run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

        let mut reader = EventReader::new(Cursor::new(output));
        let mut events = Vec::new();
        while let Some(event) = reader.read_event().expect("read") {
            events.push(event);
        }
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], Event::LifecycleHello(_)));
        assert!(matches!(events[1], Event::LifecycleSubscribe(_)));
        assert!(matches!(events[2], Event::ToolRegister(_)));
        assert!(matches!(events[3], Event::LifecycleReady(_)));
    }
}
