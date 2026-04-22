use std::error::Error;
use std::io::{BufReader, BufWriter};

use tau_proto::{
    CborValue, ClientKind, Event, EventReader, EventWriter, LifecycleHello, LifecycleReady,
    PROTOCOL_VERSION, ToolRegister, ToolResult, ToolSpec,
};

fn main() -> Result<(), Box<dyn Error>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = EventReader::new(BufReader::new(stdin.lock()));
    let mut writer = EventWriter::new(BufWriter::new(stdout.lock()));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test-child".to_owned(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::LifecycleHello(_) => {
                writer.write_event(&Event::LifecycleReady(LifecycleReady {
                    message: Some("ready".to_owned()),
                }))?;
                writer.write_event(&Event::ToolRegister(ToolRegister {
                    tool: ToolSpec {
                        name: "echo".into(),
                        description: Some("Echo test payloads".to_owned()),
                        parameters: None,
                    },
                }))?;
                writer.flush()?;
            }
            Event::ToolInvoke(invoke) => {
                writer.write_event(&Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: match invoke.arguments {
                        CborValue::Null => CborValue::Text("null".to_owned()),
                        value => value,
                    },
                }))?;
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}
