//! Child-process isolation for shell-style commands.
//!
//! Used for every external command this crate spawns so the agent's
//! environment can't leak (or be leaked into) through shared
//! filesystem state, a tty, or the parent's environment variables.

use std::process::Command;

/// Allowlist of environment variables forwarded to spawned shell
/// commands. Anything outside this set (SSH agent sockets, cloud
/// credentials, shell history config, dev-shell injections) is
/// stripped so commands run in a predictable environment instead of
/// inheriting whatever the harness happened to be launched with.
///
/// Tau's own version metadata is preserved so the agent can verify
/// what harness build it is running under when asked.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TAU_VERSION",
    "TAU_BUILD",
];

/// Sanitize a `Command` so the child runs with a minimal environment
/// and is fully detached from the harness's controlling terminal:
///
/// - Replaces the inherited environment with [`ENV_ALLOWLIST`] plus `TERM=dumb`
///   / `NO_COLOR=1` / `CLICOLOR=0` so well-behaved tools suppress ANSI escapes
///   and TTY-only fancy output.
/// - Closes stdin so interactive prompts (`sudo`, `ssh`, `read`) fail fast
///   instead of hanging on input that will never arrive.
/// - On Unix, runs `setsid()` in the child so it becomes the leader of a new
///   session with no controlling terminal — even an explicit `open("/dev/tty")`
///   will fail rather than reach the harness's tty.
pub(crate) fn apply_command_isolation(cmd: &mut Command) {
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0");

    cmd.stdin(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and only mutates the
        // calling (child) process's session/pgid — no allocator, no
        // locks, no shared state with the parent.
        //
        // Failure inside `pre_exec` aborts the spawn, so be strict
        // about what we treat as a failure: `EPERM` means the child
        // is already a session leader, which is exactly the state we
        // were trying to reach — silently accept it.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EPERM) {
                        return Ok(());
                    }
                    return Err(err);
                }
                Ok(())
            });
        }
    }
}
