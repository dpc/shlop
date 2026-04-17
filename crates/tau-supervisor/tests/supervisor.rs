use std::path::PathBuf;
use std::time::Duration;

use tau_core::ToolRegistry;
use tau_proto::{
    CborValue, ClientKind, Event, LifecycleDisconnect, LifecycleHello, LifecycleReady,
    PROTOCOL_VERSION, ToolInvoke, ToolRegister,
};
use tau_supervisor::{ExtensionCommand, SupervisedChild};

fn test_child_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tau-supervisor-test-child"))
}

#[test]
fn supervised_child_exchanges_protocol_events_over_stdio() {
    let command = ExtensionCommand {
        name: "test-child".to_owned(),
        program: test_child_path(),
        args: Vec::new(),
    };
    let mut child = SupervisedChild::spawn(command.clone()).expect("child should spawn");

    assert_eq!(child.command(), &command);
    assert_eq!(
        child.command().starting_event(),
        Event::ExtensionStarting(tau_proto::ExtensionStarting {
            extension_name: "test-child".to_owned(),
            argv: vec![test_child_path().display().to_string()],
        })
    );

    let hello = child
        .recv_timeout(Duration::from_secs(1))
        .expect("hello should decode")
        .expect("hello should arrive");
    assert_eq!(
        hello,
        Event::LifecycleHello(LifecycleHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test-child".to_owned(),
            client_kind: ClientKind::Tool,
        })
    );

    child
        .send(&Event::LifecycleHello(LifecycleHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "parent".to_owned(),
            client_kind: ClientKind::Core,
        }))
        .expect("hello should be sent");

    let ready = child
        .recv_timeout(Duration::from_secs(1))
        .expect("ready should decode")
        .expect("ready should arrive");
    assert_eq!(
        ready,
        Event::LifecycleReady(LifecycleReady {
            message: Some("ready".to_owned()),
        })
    );
    assert_eq!(
        child.ready_event(Some("conn-child".to_owned())),
        Event::ExtensionReady(tau_proto::ExtensionReady {
            extension_name: "test-child".to_owned(),
            connection_id: Some("conn-child".to_owned()),
        })
    );

    let register = child
        .recv_timeout(Duration::from_secs(1))
        .expect("register should decode")
        .expect("register should arrive");
    assert_eq!(
        register,
        Event::ToolRegister(ToolRegister {
            tool: tau_proto::ToolSpec {
                name: "demo.echo".to_owned(),
                description: Some("Echo test payloads".to_owned()),
            },
        })
    );

    child
        .send(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".to_owned(),
            tool_name: "demo.echo".to_owned(),
            arguments: CborValue::Text("hello".to_owned()),
        }))
        .expect("tool invoke should be sent");
    let result = child
        .recv_timeout(Duration::from_secs(1))
        .expect("tool result should decode")
        .expect("tool result should arrive");
    assert_eq!(
        result,
        Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-1".to_owned(),
            tool_name: "demo.echo".to_owned(),
            result: CborValue::Text("hello".to_owned()),
        })
    );

    child
        .send(&Event::LifecycleDisconnect(LifecycleDisconnect {
            reason: Some("done".to_owned()),
        }))
        .expect("disconnect should be sent");
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(
        child.exited_event(&exit),
        Event::ExtensionExited(tau_proto::ExtensionExited {
            extension_name: "test-child".to_owned(),
            exit_code: Some(0),
            signal: None,
        })
    );
}

#[test]
fn disconnect_cleanup_removes_registered_tools_after_child_exit() {
    let command = ExtensionCommand {
        name: "test-child".to_owned(),
        program: test_child_path(),
        args: Vec::new(),
    };
    let mut child = SupervisedChild::spawn(command).expect("child should spawn");
    let connection_id = "conn-child";
    let mut registry = ToolRegistry::new();

    let _hello = child
        .recv_timeout(Duration::from_secs(1))
        .expect("hello should decode")
        .expect("hello should arrive");
    child
        .send(&Event::LifecycleHello(LifecycleHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "parent".to_owned(),
            client_kind: ClientKind::Core,
        }))
        .expect("hello should be sent");
    let _ready = child
        .recv_timeout(Duration::from_secs(1))
        .expect("ready should decode")
        .expect("ready should arrive");
    let register = child
        .recv_timeout(Duration::from_secs(1))
        .expect("register should decode")
        .expect("register should arrive");

    let Event::ToolRegister(register) = register else {
        panic!("expected tool register event");
    };
    registry.register(connection_id, register.tool);

    child
        .send(&Event::LifecycleDisconnect(LifecycleDisconnect {
            reason: Some("shutdown".to_owned()),
        }))
        .expect("disconnect should be sent");
    let exit = child
        .wait_for_exit(Duration::from_secs(2))
        .expect("child should exit");
    let cleanup = child.cleanup_disconnect(&mut registry, connection_id, &exit);

    assert_eq!(cleanup.removed_tools, vec!["demo.echo".to_owned()]);
    assert!(registry.providers_for("demo.echo").is_empty());
    assert_eq!(
        cleanup.lifecycle_event,
        Event::ExtensionExited(tau_proto::ExtensionExited {
            extension_name: "test-child".to_owned(),
            exit_code: Some(0),
            signal: None,
        })
    );
}

#[test]
fn restarted_child_can_reregister_after_disconnect_cleanup() {
    let command = ExtensionCommand {
        name: "test-child".to_owned(),
        program: test_child_path(),
        args: Vec::new(),
    };
    let mut registry = ToolRegistry::new();

    for connection_id in ["conn-child-1", "conn-child-2"] {
        let mut child = SupervisedChild::spawn(command.clone()).expect("child should spawn");
        let _hello = child
            .recv_timeout(Duration::from_secs(1))
            .expect("hello should decode")
            .expect("hello should arrive");
        child
            .send(&Event::LifecycleHello(LifecycleHello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "parent".to_owned(),
                client_kind: ClientKind::Core,
            }))
            .expect("hello should be sent");
        let _ready = child
            .recv_timeout(Duration::from_secs(1))
            .expect("ready should decode")
            .expect("ready should arrive");
        let register = child
            .recv_timeout(Duration::from_secs(1))
            .expect("register should decode")
            .expect("register should arrive");
        let Event::ToolRegister(register) = register else {
            panic!("expected tool register event");
        };
        registry.register(connection_id, register.tool);
        assert_eq!(registry.providers_for("demo.echo").len(), 1);

        child
            .send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: Some("restart".to_owned()),
            }))
            .expect("disconnect should be sent");
        let exit = child
            .wait_for_exit(Duration::from_secs(2))
            .expect("child should exit");
        let cleanup = child.cleanup_disconnect(&mut registry, connection_id, &exit);
        assert_eq!(cleanup.removed_tools, vec!["demo.echo".to_owned()]);
        assert!(registry.providers_for("demo.echo").is_empty());
    }
}
