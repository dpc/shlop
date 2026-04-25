use std::time::Duration;

use tau_proto::{
    ClientKind, Event, EventSelector, LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION,
};
use tau_socket::SocketPeer;
use tau_test_support::TestRuntime;

#[test]
#[ignore = "needs echo agent wired into run_daemon"]
fn socket_transport_supports_later_attached_end_to_end_clients() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let daemon = runtime.spawn_daemon("session-1", Some(2));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");

    let first = runtime
        .send_daemon_message("session-1", "hello")
        .expect("first client message should succeed");
    let second = runtime
        .send_daemon_message("session-1", "read Cargo.toml")
        .expect("second client message should succeed");

    assert!(!first.is_empty(), "response should not be empty");
    assert!(!second.is_empty(), "read response should not be empty");
    daemon.join().expect("daemon should exit cleanly");

    let store = runtime
        .open_session_store()
        .expect("session store should reopen");
    let session = store.session("session-1").expect("session should exist");
    assert_eq!(session.current_branch().len(), 8);
}

#[test]
fn forbidden_socket_subscription_disconnects_client_without_killing_daemon() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let daemon = runtime.spawn_daemon("session-1", Some(2));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");

    let mut denied_client =
        SocketPeer::connect(&runtime.socket_path).expect("denied client should connect");
    denied_client
        .send(&Event::LifecycleHello(LifecycleHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "denied-client".into(),
            client_kind: ClientKind::Ui,
        }))
        .expect("hello should send");
    denied_client
        .send(&Event::LifecycleSubscribe(LifecycleSubscribe {
            selectors: vec![EventSelector::Prefix("lifecycle.".to_owned())],
        }))
        .expect("forbidden subscribe should send");

    let denial = denied_client
        .recv_timeout(Duration::from_secs(2))
        .expect("daemon should reply to denied client")
        .expect("disconnect should arrive");
    let Event::LifecycleDisconnect(disconnect) = denial else {
        panic!("expected lifecycle disconnect");
    };
    let reason = disconnect
        .reason
        .expect("disconnect reason should be present");
    assert!(reason.contains("subscription denied"));

    let response = runtime
        .send_daemon_message("session-1", "hello")
        .expect("daemon should still serve valid clients");
    assert!(!response.is_empty(), "response should not be empty");
    daemon.join().expect("daemon should exit cleanly");

    let approvals = runtime
        .open_policy_store()
        .expect("policy store should reopen");
    assert_eq!(approvals.approvals().len(), 1);
}
