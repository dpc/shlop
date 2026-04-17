//! Reusable end-to-end test utilities for `shlop` crates.

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shlop_cli::{
    CliError, ServeOptions, open_policy_store, open_session_store, run_daemon,
    run_embedded_message, send_daemon_message,
};
use shlop_core::{PolicyStore, SessionStore};
use tempfile::TempDir;

/// Temporary runtime paths for end-to-end tests.
#[derive(Debug)]
pub struct TestRuntime {
    _tempdir: TempDir,
    pub socket_path: PathBuf,
    pub session_store_path: PathBuf,
    pub policy_store_path: PathBuf,
}

impl TestRuntime {
    /// Creates isolated temporary paths for one test runtime.
    pub fn new() -> Result<Self, std::io::Error> {
        let tempdir = TempDir::new()?;
        Ok(Self {
            socket_path: tempdir.path().join("daemon.sock"),
            session_store_path: tempdir.path().join("sessions.cbor"),
            policy_store_path: tempdir.path().join("policy.cbor"),
            _tempdir: tempdir,
        })
    }

    /// Runs one embedded interaction and returns the agent response.
    pub fn run_embedded(&self, session_id: &str, message: &str) -> Result<String, CliError> {
        run_embedded_message(&self.session_store_path, session_id, message)
    }

    /// Starts a foreground daemon in a background thread.
    pub fn spawn_daemon(&self, max_clients: Option<usize>) -> DaemonHandle {
        let socket_path = self.socket_path.clone();
        let session_store_path = self.session_store_path.clone();
        let policy_store_path = self.policy_store_path.clone();
        let join_handle = thread::spawn(move || {
            run_daemon(
                socket_path,
                session_store_path,
                ServeOptions {
                    max_clients,
                    policy_store_path: Some(policy_store_path),
                },
            )
        });
        DaemonHandle { join_handle }
    }

    /// Waits until the daemon socket exists.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), WaitError> {
        wait_for_path(&self.socket_path, timeout)
    }

    /// Sends one message to a running daemon.
    pub fn send_daemon_message(&self, session_id: &str, message: &str) -> Result<String, CliError> {
        send_daemon_message(&self.socket_path, session_id, message)
    }

    /// Opens the session store for assertions.
    pub fn open_session_store(&self) -> Result<SessionStore, CliError> {
        open_session_store(&self.session_store_path)
    }

    /// Opens the policy store for assertions.
    pub fn open_policy_store(&self) -> Result<PolicyStore, CliError> {
        open_policy_store(&self.policy_store_path)
    }
}

/// A running daemon thread handle.
#[derive(Debug)]
pub struct DaemonHandle {
    join_handle: JoinHandle<Result<(), CliError>>,
}

impl DaemonHandle {
    /// Waits for the daemon thread to finish.
    pub fn join(self) -> Result<(), CliError> {
        self.join_handle
            .join()
            .map_err(|_| CliError::ThreadJoin("daemon".to_owned()))?
    }
}

/// Waits until one filesystem path exists.
pub fn wait_for_path(path: &Path, timeout: Duration) -> Result<(), WaitError> {
    let started_at = Instant::now();
    while !path.exists() {
        if timeout <= started_at.elapsed() {
            return Err(WaitError::Timeout {
                path: path.to_path_buf(),
                timeout,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Error returned when waiting for a test condition times out.
#[derive(Debug)]
pub enum WaitError {
    Timeout { path: PathBuf, timeout: Duration },
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { path, timeout } => write!(
                f,
                "timed out waiting for path {} after {timeout:?}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for WaitError {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn runtime_supports_embedded_and_daemon_scenarios() {
        let runtime = TestRuntime::new().expect("runtime should be created");

        let embedded = runtime
            .run_embedded("session-1", "hello")
            .expect("embedded run should succeed");
        assert!(embedded.contains("demo.echo returned"));

        let daemon = runtime.spawn_daemon(Some(1));
        runtime
            .wait_until_ready(Duration::from_secs(2))
            .expect("daemon socket should appear");
        let attached = runtime
            .send_daemon_message("session-2", "hello")
            .expect("daemon message should succeed");
        assert!(attached.contains("demo.echo returned"));
        daemon.join().expect("daemon should exit cleanly");
    }
}
