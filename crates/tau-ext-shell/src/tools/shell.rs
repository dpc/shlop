//! `shell` tool and user-initiated `!`/`!!` command dispatch.

use std::sync::mpsc;

use tau_proto::{CborValue, Event, Frame, ToolDisplay, ToolDisplayStatus};

use crate::argument::{argument_text, optional_argument_int, optional_argument_text};
use crate::config::ShellConfig;
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::truncate::truncate_tail;

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Execute a `shell` tool call.
///
/// **Non-zero exit semantics.** A non-zero exit status is surfaced as
/// `ToolError`, not `ToolResult` — even for benign cases like `grep`
/// finding no matches. The full stdout/stderr is preserved in
/// `details`, so the agent still sees what happened. The reason: every
/// model we target treats `ToolError` as a signal worth attending to,
/// and conflating "command ran" with "command failed" pushes
/// shell-script-style exit-code logic into the agent's prompt. Callers
/// that want exit codes returned as data should use a dedicated tool
/// (`grep`, `find`, `ls`) instead of `shell`.
pub(crate) fn run_command(
    arguments: &CborValue,
    shell_config: &ShellConfig,
) -> Result<ToolOutput, ToolFailure> {
    let command = argument_text(arguments, "command").map_err(ToolFailure::from)?;
    let cwd = optional_argument_text(arguments, "cwd");
    let timeout_secs = optional_argument_int(arguments, "timeout")
        .map(|v| v.max(1) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let display_args = command.clone();

    let mut child = shell_config
        .spawn_isolated(&command, cwd.as_deref())
        .map_err(|error| {
            ToolFailure::from(format!("failed to start shell command: {error}"))
                .with_args(display_args.clone())
                .with_details(command_details_value(
                    command.clone(),
                    cwd.clone(),
                    None,
                    String::new(),
                    String::new(),
                ))
        })?;

    let wait = match wait_with_timeout(&mut child, timeout) {
        Some(wait) => wait,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                ToolFailure::from(format!("command timed out after {timeout_secs}s"))
                    .with_args(display_args)
                    .with_details(command_details_value(
                        command.clone(),
                        cwd.clone(),
                        None,
                        String::new(),
                        String::new(),
                    )),
            );
        }
    };

    let status_code = wait.status_code;
    let success = wait.success;

    // Truncate stdout/stderr from the tail (keep last lines — errors and
    // final results are at the end).
    let stdout_trunc = truncate_tail(&wait.stdout);
    let stderr_trunc = truncate_tail(&wait.stderr);

    let combined = if stdout_trunc.content.is_empty() {
        stderr_trunc.content.clone()
    } else if stderr_trunc.content.is_empty() {
        stdout_trunc.content.clone()
    } else {
        format!("{}\n{}", stdout_trunc.content, stderr_trunc.content)
    };

    let result = command_details_value(
        command.clone(),
        cwd.clone(),
        status_code,
        stdout_trunc.content,
        stderr_trunc.content,
    );

    if success {
        let mut display = ok_display(display_args);
        display.stats = text_stats(&combined);
        Ok(ToolOutput { result, display })
    } else {
        let exit_label = status_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let mut display = ToolDisplay {
            args: display_args,
            status: ToolDisplayStatus::Error,
            status_text: format!("err: {exit_label}"),
            ..Default::default()
        };
        display.stats = text_stats(&combined);
        Err(ToolFailure {
            message: format!("command exited with status {exit_label}"),
            details: Some(Box::new(result)),
            display: Box::new(display),
        })
    }
}

/// Run a user-initiated `!`/`!!` shell command, streaming stdout and
/// stderr back as `ShellCommandProgress` chunks while they arrive and
/// emitting `ShellCommandFinished` with the full (truncated-tail)
/// output when the child exits.
pub(crate) fn dispatch_user_shell_command(
    cmd: tau_proto::UiShellCommand,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<Frame>,
) {
    use std::io::Read;

    let mut child = match shell_config.spawn_isolated(&cmd.command, None) {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send(Frame::Event(Event::ShellCommandFinished(
                tau_proto::ShellCommandFinished {
                    command_id: cmd.command_id,
                    session_id: cmd.session_id,
                    command: cmd.command,
                    include_in_context: cmd.include_in_context,
                    output: format!("failed to start shell command: {err}"),
                    exit_code: None,
                    cancelled: false,
                },
            )));
            return;
        }
    };

    // Read each pipe on a dedicated thread. Each read chunk is both
    // emitted as a `ShellCommandProgress` event (for live UI
    // rendering) and accumulated into a buffer that we later truncate
    // and send in `ShellCommandFinished` (for session-history
    // injection when `include_in_context`).

    fn pump<R: Read + Send + 'static>(
        mut pipe: R,
        stream: tau_proto::ShellStream,
        command_id: tau_proto::ShellCommandId,
        tx: mpsc::Sender<Frame>,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut captured = String::new();
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        captured.push_str(&chunk);
                        let _ = tx.send(Frame::Event(Event::ShellCommandProgress(
                            tau_proto::ShellCommandProgress {
                                command_id: command_id.clone(),
                                stream,
                                chunk,
                            },
                        )));
                    }
                }
            }
            captured
        })
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = stdout_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stdout,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });
    let stderr_handle = stderr_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stderr,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });

    // Bound the user `!`/`!!` runtime. Without this a hung command
    // would block the worker thread (and its semaphore permit)
    // forever. The default is generous (1h) so legitimate long
    // builds aren't cut short.
    let timeout = std::time::Duration::from_secs(shell_config.user_command_timeout_secs);
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let waiter = std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = done_tx.send(status);
    });

    let (exit_code, status_note, cancelled) = match done_rx.recv_timeout(timeout) {
        Ok(Some(status)) => (status.code(), None, false),
        Ok(None) => (None, Some("wait failed".to_owned()), false),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Kill the whole process group. `setsid` in
            // `apply_command_isolation` made the child a session
            // leader, so its pgid equals its pid — sending a signal
            // to `-pid` reaches the leader and every descendant it
            // hasn't detached.
            #[cfg(unix)]
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            // Wait for the waiter to observe the death so the child
            // is reaped rather than left as a zombie.
            let status = done_rx.recv().ok().flatten();
            let _ = waiter.join();
            (
                status.and_then(|s| s.code()),
                Some(format!(
                    "command killed after {}s timeout",
                    shell_config.user_command_timeout_secs
                )),
                true,
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            (None, Some("waiter thread vanished".to_owned()), false)
        }
    };

    let stdout = stdout_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    // Interleave stdout + stderr in the final output the way the
    // `shell` tool does: stderr follows stdout under a separator.
    let mut merged = stdout;
    if !stderr.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(&stderr);
    }
    if let Some(note) = status_note {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&note);
    }
    let truncated = truncate_tail(&merged);

    let _ = tx.send(Frame::Event(Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            output: truncated.content,
            exit_code,
            cancelled,
        },
    )));
}

/// Wait for a child process with a timeout. Returns `None` if timed out.
///
/// Pipes are read on dedicated threads to avoid deadlocks. When the child
/// exits its pipes close, the reader threads complete, and we get our
/// signal — no polling.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<WaitResult> {
    // Take the pipes so we can move them into reader threads.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Read stdout/stderr on dedicated threads so a full pipe buffer
    // can't prevent the child from exiting.
    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe));

    // Collector thread: joins both readers (which complete when the child
    // exits and closes its pipes), then sends the output on a channel.
    let (tx, rx) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();
        let _ = tx.send((stdout, stderr));
    });

    match rx.recv_timeout(timeout) {
        Ok((stdout, stderr)) => {
            // Pipes closed → child *almost certainly* exited, but the
            // child can call `close(1)/close(2)` itself, so EOF on
            // stdio is not a hard guarantee of process reap. Treat a
            // `wait()` failure as "exited under unknown circumstances"
            // rather than panicking.
            let (status_code, success) = match child.wait() {
                Ok(status) => (status.code(), status.success()),
                Err(_) => (None, false),
            };
            Some(WaitResult {
                status_code,
                success,
                stdout,
                stderr,
            })
        }
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

struct WaitResult {
    status_code: Option<i32>,
    success: bool,
    stdout: String,
    stderr: String,
}

fn read_pipe(pipe: Option<impl std::io::Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = pipe.read_to_string(&mut buf);
    buf
}

pub(crate) fn command_details_value(
    command: String,
    cwd: Option<String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
) -> CborValue {
    let mut entries = vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command),
        ),
        (
            CborValue::Text("stdout".to_owned()),
            CborValue::Text(stdout.clone()),
        ),
        (
            CborValue::Text("stderr".to_owned()),
            CborValue::Text(stderr.clone()),
        ),
        (
            CborValue::Text("stdout_lines".to_owned()),
            CborValue::Integer((stdout.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stdout_bytes".to_owned()),
            CborValue::Integer((stdout.len() as i64).into()),
        ),
        (
            CborValue::Text("stderr_lines".to_owned()),
            CborValue::Integer((stderr.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stderr_bytes".to_owned()),
            CborValue::Integer((stderr.len() as i64).into()),
        ),
    ];
    if let Some(cwd) = cwd {
        entries.push((CborValue::Text("cwd".to_owned()), CborValue::Text(cwd)));
    }
    if let Some(status) = status {
        entries.push((
            CborValue::Text("status".to_owned()),
            CborValue::Integer(status.into()),
        ));
    }
    CborValue::Map(entries)
}
