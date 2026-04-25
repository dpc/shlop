//! Reusable end-to-end test utilities for `tau` crates.

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tau_config::settings::TauDirs;
use tau_core::{PolicyStore, SessionStore};
use tau_harness::{
    EmbeddedOptions, HarnessError, ServeOptions, open_policy_store, open_session_store, run_daemon,
    run_embedded_message_with_options, send_daemon_message,
};
use tempfile::TempDir;

/// Temporary runtime paths for end-to-end tests.
#[derive(Debug)]
pub struct TestRuntime {
    _tempdir: TempDir,
    pub socket_path: PathBuf,
    /// Per-state directory containing session subdirs and `policy.cbor`.
    pub state_dir: PathBuf,
    /// Isolated `$XDG_CONFIG_HOME`/`$XDG_STATE_HOME` layout so tests don't
    /// leak into (or read from) the developer's real `~/.config/tau` and
    /// `~/.local/state/tau`.
    pub dirs: TauDirs,
}

impl TestRuntime {
    /// Creates isolated temporary paths for one test runtime.
    ///
    /// Seeds the isolated config dir with a minimal `models.json5` so the
    /// harness picks a non-empty `selected_model` and dispatches prompts
    /// immediately. The agent can't resolve this fake provider, so it replies
    /// with a short "cannot resolve model config" message — which is exactly
    /// what tests asserting "response is non-empty" want.
    pub fn new() -> Result<Self, std::io::Error> {
        let tempdir = TempDir::new()?;
        let config_dir = tempdir.path().join("config");
        let state_dir = tempdir.path().join("state");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        std::fs::write(
            config_dir.join("models.json5"),
            r#"{ providers: { "test": { auth: "none", models: [{ id: "echo" }] } } }"#,
        )?;
        Ok(Self {
            socket_path: tempdir.path().join("daemon.sock"),
            state_dir: state_dir.clone(),
            dirs: TauDirs {
                config_dir: Some(config_dir),
                state_dir: Some(state_dir),
            },
            _tempdir: tempdir,
        })
    }

    /// Runs one embedded interaction and returns the agent response.
    pub fn run_embedded(&self, session_id: &str, message: &str) -> Result<String, HarnessError> {
        Ok(run_embedded_message_with_options(
            &self.state_dir,
            session_id,
            message,
            EmbeddedOptions::builder().dirs(self.dirs.clone()).build(),
        )?
        .response)
    }

    /// Starts a foreground daemon in a background thread, eager-initing
    /// the given session id (typically what test code will then send a
    /// message to).
    pub fn spawn_daemon(&self, eager_session_id: &str, max_clients: Option<usize>) -> DaemonHandle {
        let socket_path = self.socket_path.clone();
        let state_dir = self.state_dir.clone();
        let dirs = self.dirs.clone();
        let eager_session_id = eager_session_id.to_owned();
        let join_handle = thread::spawn(move || {
            let mut options = ServeOptions::builder().dirs(dirs).build();
            options.max_clients = max_clients;
            run_daemon(socket_path, state_dir, &eager_session_id, options)
        });
        DaemonHandle { join_handle }
    }

    /// Waits until the daemon socket exists.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), WaitError> {
        wait_for_path(&self.socket_path, timeout)
    }

    /// Sends one message to a running daemon.
    pub fn send_daemon_message(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<String, HarnessError> {
        send_daemon_message(&self.socket_path, session_id, message)
    }

    /// Opens the session store for assertions.
    pub fn open_session_store(&self) -> Result<SessionStore, HarnessError> {
        open_session_store(&self.state_dir)
    }

    /// Opens the policy store for assertions.
    pub fn open_policy_store(&self) -> Result<PolicyStore, HarnessError> {
        open_policy_store(self.state_dir.join("policy.cbor"))
    }
}

/// A running daemon thread handle.
#[derive(Debug)]
pub struct DaemonHandle {
    join_handle: JoinHandle<Result<(), HarnessError>>,
}

impl DaemonHandle {
    /// Waits for the daemon thread to finish.
    pub fn join(self) -> Result<(), HarnessError> {
        self.join_handle
            .join()
            .map_err(|_| HarnessError::ThreadJoin("daemon".to_owned()))?
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
        assert!(!embedded.is_empty(), "response should not be empty");

        let daemon = runtime.spawn_daemon("session-2", Some(1));
        runtime
            .wait_until_ready(Duration::from_secs(2))
            .expect("daemon socket should appear");
        let attached = runtime
            .send_daemon_message("session-2", "hello")
            .expect("daemon message should succeed");
        assert!(!attached.is_empty(), "response should not be empty");
        daemon.join().expect("daemon should exit cleanly");
    }
}
