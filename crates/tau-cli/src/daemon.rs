//! Harness daemon lifecycle: discovery, spawning, and the
//! parent↔child readiness handshake.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use tau_cli_picker::{PickerError, PickerItem, pick};
use tau_harness::{SessionLaunchStatus, runtime_dir};

use crate::{CliError, mint_short_id};

const RESUME_PICKER_LIMIT: usize = 10;

/// How this CLI invocation is related to its harness daemon.
///
/// - `Owned`: we spawned the daemon; Drop kills it unless the UI detached
///   (calls [`DaemonHandle::leak`]), in which case we forget the `Child` so the
///   daemon outlives us.
/// - `Attached`: we joined a daemon someone else owns. Drop never touches it.
pub(crate) enum DaemonHandle {
    /// `child` is `Some` until [`leak`] pulls it out.
    Owned {
        child: Option<std::process::Child>,
        daemon_dir: PathBuf,
    },
    Attached {
        daemon_dir: PathBuf,
    },
}

impl DaemonHandle {
    pub(crate) fn socket_path(&self) -> PathBuf {
        runtime_dir::socket_path(self.daemon_dir())
    }

    fn daemon_dir(&self) -> &Path {
        match self {
            Self::Owned { daemon_dir, .. } | Self::Attached { daemon_dir } => daemon_dir,
        }
    }

    /// Consume the handle without killing the child.
    ///
    /// Used by `/detach`: we want the daemon to outlive this CLI,
    /// whether we spawned it or attached to it. For `Owned` this
    /// `mem::forget`s the `Child` — on Linux its parent becomes init
    /// on our exit, which is exactly what we want for a long-lived
    /// daemon.
    pub(crate) fn leak(mut self) {
        if let Self::Owned { child, .. } = &mut self
            && let Some(child) = child.take()
        {
            std::mem::forget(child);
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Self::Owned {
            child: Some(child), ..
        } = self
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Attached, or Owned-after-leak: do nothing. The daemon keeps
        // running so other UIs can still use it, or this same UI can
        // `tau -a` back in later.
    }
}

/// Resolves the session id for one `tau` invocation.
///
/// - `None` → mint `<basename(cwd)>-<rand6>`.
/// - `Some("")` (bare `-r`) → interactively pick among recent sessions whose
///   `meta.json.cwd` matches cwd; if none, mint fresh.
/// - `Some(id)` → resume that explicit id; error if it does not exist.
pub(crate) fn resolve_run_session_id(
    resume: Option<&str>,
) -> Result<(String, SessionLaunchStatus), CliError> {
    let cwd = std::env::current_dir()?;
    match resume {
        None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        Some("") => match pick_resume_session(&cwd)? {
            Some(id) => Ok((id, SessionLaunchStatus::Resumed)),
            None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        },
        Some(id) => {
            if session_exists(id)? {
                Ok((id.to_owned(), SessionLaunchStatus::Resumed))
            } else {
                Err(CliError::SessionNotFound(id.to_owned()))
            }
        }
    }
}

fn session_exists(id: &str) -> Result<bool, CliError> {
    let state_dir = tau_session_inspect::default_state_dir();
    let metas = tau_harness::list_session_metas(&state_dir)?;
    Ok(metas
        .into_iter()
        .any(|(session_id, _)| session_id.as_str() == id))
}

pub(crate) fn mint_session_id(cwd: &Path) -> String {
    let basename = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    mint_short_id(basename)
}

fn pick_resume_session(cwd: &Path) -> Result<Option<String>, CliError> {
    let state_dir = tau_session_inspect::default_state_dir();
    let mut metas = tau_harness::list_session_metas(&state_dir)?;
    metas.retain(|(_, meta)| meta.cwd.as_deref() == Some(cwd));
    metas.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.last_touched));
    metas.truncate(RESUME_PICKER_LIMIT);
    if metas.is_empty() {
        return Ok(None);
    }
    if metas.len() == 1 || !io::IsTerminal::is_terminal(&io::stdin()) {
        return Ok(metas.first().map(|(sid, _)| sid.as_str().to_owned()));
    }

    let rows = metas
        .into_iter()
        .map(|(sid, meta)| {
            let locked =
                tau_harness::session_is_locked(&state_dir, sid.as_str()).unwrap_or_else(|error| {
                    tracing::warn!(
                        target: "tau_cli::startup",
                        session_id = sid.as_str(),
                        %error,
                        "could not determine session lock state — assuming unlocked"
                    );
                    false
                });
            let preview = meta
                .latest_user_prompt_preview
                .map(|p| format!(" — {p}"))
                .unwrap_or_default();
            let id = sid.as_str().to_owned();
            let item = format!("{}{}", sid.as_str(), preview);
            (id, item, locked)
        })
        .collect::<Vec<_>>();
    if rows.iter().all(|(_, _, locked)| *locked) {
        return Ok(None);
    }
    let default = rows
        .iter()
        .position(|(_, _, locked)| !*locked)
        .unwrap_or_default();
    if rows.iter().filter(|(_, _, locked)| !*locked).count() == 1 {
        return Ok(Some(rows[default].0.clone()));
    }
    let items = rows
        .iter()
        .map(|(_, item, locked)| {
            if *locked {
                PickerItem::disabled(item)
            } else {
                PickerItem::enabled(item)
            }
        })
        .collect::<Vec<_>>();
    let selection = match pick("Resume session", &items) {
        Ok(selection) => selection,
        Err(PickerError::Cancelled) => return Ok(None),
        Err(e) => return Err(CliError::Participant(e.to_string())),
    };
    Ok(Some(rows[selection].0.clone()))
}

pub(crate) struct DaemonOutput {
    pub(crate) stdout: Stdio,
    pub(crate) stderr: Stdio,
    pub(crate) log_path: PathBuf,
    pub(crate) start_offset: u64,
}

pub(crate) fn resolve_daemon(
    attach: bool,
    session_id: &str,
    session_status: SessionLaunchStatus,
    daemon_output: Option<DaemonOutput>,
) -> Result<DaemonHandle, CliError> {
    tracing::debug!(target: "tau_cli::startup", attach, session_id, "resolving harness daemon");
    let project_root = std::env::current_dir()?;
    if attach {
        tracing::debug!(target: "tau_cli::startup", project_root = %project_root.display(), "looking for existing harness daemon");
        let daemon_dir =
            runtime_dir::find_harness_for_dir(&project_root).ok_or(CliError::NoRunningDaemon)?;
        tracing::debug!(target: "tau_cli::startup", daemon_dir = %daemon_dir.display(), "attached harness daemon resolved");
        return Ok(DaemonHandle::Attached { daemon_dir });
    }
    start_daemon(
        session_id,
        session_status,
        daemon_output.expect("daemon output for spawned harness"),
    )
}

fn read_daemon_output_since(path: &Path, start_offset: u64) -> io::Result<String> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut output = String::new();
    file.read_to_string(&mut output)?;
    Ok(output)
}

/// Spawns a new harness daemon and waits for its socket to be ready.
///
/// Synchronization is via an inherited pipe rather than a polling
/// loop: a `pipe2` is created with the read end retained by the parent
/// and the write end inherited by the child. The harness writes one
/// byte and closes its end once the socket is bound and the runtime
/// markers are in place (see [`runtime_dir::signal_ready_to_parent`]);
/// the parent blocks on `read_exact` until that byte arrives. EOF
/// without a byte means the child exited early — we reap it and
/// surface its captured output.
fn start_daemon(
    session_id: &str,
    session_status: SessionLaunchStatus,
    output: DaemonOutput,
) -> Result<DaemonHandle, CliError> {
    use std::os::fd::FromRawFd;

    let tau_binary = std::env::current_exe()?;
    tracing::debug!(target: "tau_cli::startup", tau_binary = %tau_binary.display(), session_id, "spawning harness daemon");

    let ReadyPipe { read_fd, write_fd } = ReadyPipe::create()?;

    let spawn_result = build_daemon_command(
        &tau_binary,
        session_id,
        session_status,
        output.stdout,
        output.stderr,
        write_fd,
    )
    .spawn();

    // Parent never writes to the pipe; close our copy of the write end
    // so the only remaining handle is the child's, and the read end
    // will see EOF as soon as the child exits.
    ReadyPipe::close_fd(write_fd);

    let mut child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            ReadyPipe::close_fd(read_fd);
            return Err(e.into());
        }
    };

    tracing::debug!(target: "tau_cli::startup", pid = child.id(), "harness daemon spawned");
    let daemon_dir = runtime_dir::root_runtime_dir().join(child.id().to_string());
    let started_at = Instant::now();

    // SAFETY: read_fd was created above and not closed; ownership is
    // transferred to this File which closes it on drop.
    #[allow(unsafe_code)]
    let mut read_pipe = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut byte = [0u8; 1];
    match read_pipe.read_exact(&mut byte) {
        Ok(()) => {
            tracing::debug!(target: "tau_cli::startup", pid = child.id(), daemon_dir = %daemon_dir.display(), elapsed_ms = started_at.elapsed().as_millis(), "harness daemon signaled ready");
            Ok(DaemonHandle::Owned {
                child: Some(child),
                daemon_dir,
            })
        }
        Err(_eof_or_err) => {
            // Read end closed without a byte. Either the child exited
            // before signaling ready, or its pre_exec failed. Reap it
            // either way so we can surface the captured stderr.
            let status = child.wait()?;
            tracing::debug!(target: "tau_cli::startup", pid = child.id(), %status, elapsed_ms = started_at.elapsed().as_millis(), "harness daemon exited before signaling ready");
            let captured = read_daemon_output_since(&output.log_path, output.start_offset)?;
            let mut message = format!("exit status: {status}");
            if !captured.trim().is_empty() {
                message.push_str("\n\nHarness output:\n");
                message.push_str(captured.trim_end());
            }
            Err(CliError::DaemonExited(message))
        }
    }
}

/// Build the `tau ext harness` command with the readiness-pipe write fd
/// arranged to survive `execve`. Splitting this out keeps the unsafe
/// `pre_exec` block isolated from [`start_daemon`]'s control flow.
fn build_daemon_command(
    tau_binary: &Path,
    session_id: &str,
    session_status: SessionLaunchStatus,
    stdout: Stdio,
    stderr: Stdio,
    write_fd: libc::c_int,
) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(tau_binary);
    cmd.arg("ext")
        .arg("harness")
        .env("TAU_SESSION_ID", session_id)
        .env("TAU_SESSION_STATUS", session_status.as_str())
        // TAU_VERSION/TAU_BUILD/TAU_LAST_MODIFIED used to be forwarded
        // here; the harness child now reads its own `built` snapshot
        // (see `tau_harness::version::export_to_env`) and publishes
        // them to its own environment instead.
        .env(tau_harness::runtime_dir::READY_FD_ENV, write_fd.to_string())
        // Default-enable info logging in the child process so `tau`
        // captures harness logs without requiring an env var. Users
        // can still override/filter with `TAU_LOG`.
        .env(
            "TAU_LOG",
            std::env::var("TAU_LOG").unwrap_or_else(|_| "tau_harness=info,tau_cli=info".to_owned()),
        )
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);

    // Safety: `pre_exec` runs in the forked child between `fork` and
    // `execve`. We only call `fcntl` (signal-safe) and only on the fd
    // we created ourselves. Clearing `FD_CLOEXEC` is required so the
    // descriptor survives `execve` into the harness binary.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(write_fd, libc::F_GETFD);
            if flags == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(write_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// A `pipe2(O_CLOEXEC)` pair created for the parent↔child readiness
/// handshake. The read end is retained by the parent, the write end is
/// passed to the child via the [`runtime_dir::READY_FD_ENV`] env var.
struct ReadyPipe {
    read_fd: libc::c_int,
    write_fd: libc::c_int,
}

impl ReadyPipe {
    fn create() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // Safety: we pass a valid 2-element array to `pipe2`; on success
        // it writes two new fds owned by this process.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    fn close_fd(fd: libc::c_int) {
        // Safety: each fd is closed at most once across this module.
        #[allow(unsafe_code)]
        unsafe {
            libc::close(fd);
        }
    }
}
