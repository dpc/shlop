//! Daemon runtime directory management.
//!
//! Each harness daemon gets its own directory under
//! `$XDG_RUNTIME_DIR/tau/{pid}/` containing:
//!
//! - `tau.sock` — Unix socket for client connections
//! - `tau.dir` — project root path (discovery marker)
//! - `tau.pid` — daemon process ID
//! - `tau.session_id` — bound session id (so `tau -a` can resume it)
//!
//! Finding `tau.dir` guarantees the socket is already bound (the marker
//! is written *after* binding the socket).

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const SOCK_FILENAME: &str = "tau.sock";
const DIR_FILENAME: &str = "tau.dir";
const PID_FILENAME: &str = "tau.pid";
const SESSION_ID_FILENAME: &str = "tau.session_id";

/// Returns the root runtime directory for all tau daemon instances.
#[must_use]
pub fn root_runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .map(|dir| dir.join("tau"))
        .unwrap_or_else(|| {
            let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
            PathBuf::from(format!("/tmp/tau-{user}"))
        })
}

/// Returns the socket path within a daemon directory.
#[must_use]
pub fn socket_path(daemon_dir: &Path) -> PathBuf {
    daemon_dir.join(SOCK_FILENAME)
}

/// Metadata for one daemon directory, created before entering the
/// daemon loop.
pub struct DaemonDir {
    path: PathBuf,
    project_root: PathBuf,
}

impl DaemonDir {
    /// Returns the path to this daemon directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the socket path.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        socket_path(&self.path)
    }

    /// Writes the project root marker. Must be called *after* the
    /// socket is bound.
    pub fn write_marker(&self) -> Result<(), std::io::Error> {
        std::fs::write(
            self.path.join(DIR_FILENAME),
            self.project_root.to_string_lossy().as_bytes(),
        )
    }

    /// Writes the PID file.
    pub fn write_pid(&self) -> Result<(), std::io::Error> {
        std::fs::write(self.path.join(PID_FILENAME), std::process::id().to_string())
    }

    /// Writes the bound session id so `tau -a` can join that
    /// specific session instead of minting a fresh one.
    pub fn write_session_id(&self, session_id: &str) -> Result<(), std::io::Error> {
        std::fs::write(self.path.join(SESSION_ID_FILENAME), session_id.as_bytes())
    }

    /// Removes the daemon directory.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Reads the session id a running daemon at `daemon_dir` is bound to.
#[must_use]
pub fn read_session_id(daemon_dir: &Path) -> Option<String> {
    std::fs::read_to_string(daemon_dir.join(SESSION_ID_FILENAME))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Creates a new daemon directory for the current process.
pub fn prepare_daemon_dir(project_root: &Path) -> Result<DaemonDir, std::io::Error> {
    let pid = std::process::id();
    let path = root_runtime_dir().join(pid.to_string());
    std::fs::create_dir_all(&path)?;
    Ok(DaemonDir {
        path,
        project_root: project_root.to_path_buf(),
    })
}

/// Finds a running harness daemon for the given project root.
#[must_use]
pub fn find_harness_for_dir(project_root: &Path) -> Option<PathBuf> {
    let runtime_dir = root_runtime_dir();
    if !runtime_dir.exists() {
        return None;
    }

    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let pid_dir = entry.path();

        let dir_file = pid_dir.join(DIR_FILENAME);
        let stored_root = match std::fs::read_to_string(&dir_file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if paths_equal(Path::new(stored_root.trim()), project_root) {
            if verify_harness_running(&pid_dir) {
                return Some(pid_dir);
            } else {
                let _ = std::fs::remove_dir_all(&pid_dir);
            }
        }
    }

    None
}

/// Verifies that a daemon is actually running by connecting to its
/// socket.
fn verify_harness_running(daemon_dir: &Path) -> bool {
    let sock = daemon_dir.join(SOCK_FILENAME);
    UnixStream::connect(sock).is_ok()
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a_canon), Ok(b_canon)) => a_canon == b_canon,
        _ => a == b,
    }
}
