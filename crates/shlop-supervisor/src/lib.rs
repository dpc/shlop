//! Supervised child-process management and stdio transport adapters.
//!
//! The initial implementation focuses on one supervised child process connected
//! over stdin/stdout using the shared CBOR event protocol.

use std::io::{self, BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use shlop_core::{ToolRegistry, ToolRouteError};
use shlop_proto::{
    ConnectionId, DecodeError, Event, EventReader, EventWriter, ExtensionExited, ExtensionName,
    ExtensionReady, ExtensionStarting, ToolName,
};

/// One configured supervised extension command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionCommand {
    pub name: ExtensionName,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl ExtensionCommand {
    /// Returns the argv used to launch the child process.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(1 + self.args.len());
        argv.push(self.program.display().to_string());
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Creates the lifecycle event emitted before the child starts.
    #[must_use]
    pub fn starting_event(&self) -> Event {
        Event::ExtensionStarting(ExtensionStarting {
            extension_name: self.name.clone(),
            argv: self.argv(),
        })
    }
}

/// One detected child-process exit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildExit {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

impl ChildExit {
    fn from_status(status: std::process::ExitStatus) -> Self {
        Self {
            exit_code: status.code(),
            signal: exit_signal(status),
        }
    }
}

/// Cleanup result returned after a supervised child disconnects.
#[derive(Clone, Debug, PartialEq)]
pub struct DisconnectCleanup {
    pub removed_tools: Vec<ToolName>,
    pub lifecycle_event: Event,
}

/// Errors produced by the supervised stdio transport.
#[derive(Debug)]
pub enum SupervisionError {
    Spawn(io::Error),
    MissingStdin,
    MissingStdout,
    Encode(shlop_proto::EncodeError),
    Flush(io::Error),
    Decode(DecodeError),
    Wait(io::Error),
    Timeout { duration: Duration },
    ToolRoute(ToolRouteError),
}

impl fmt::Display for SupervisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(source) => write!(f, "failed to spawn child process: {source}"),
            Self::MissingStdin => f.write_str("spawned child process did not expose stdin"),
            Self::MissingStdout => f.write_str("spawned child process did not expose stdout"),
            Self::Encode(source) => write!(f, "failed to encode event for child stdin: {source}"),
            Self::Flush(source) => write!(f, "failed to flush child stdin: {source}"),
            Self::Decode(source) => write!(f, "failed to decode event from child stdout: {source}"),
            Self::Wait(source) => write!(f, "failed to wait for child process: {source}"),
            Self::Timeout { duration } => {
                write!(f, "timed out waiting for child exit after {duration:?}")
            }
            Self::ToolRoute(source) => write!(
                f,
                "tool routing failed during supervision cleanup: {source}"
            ),
        }
    }
}

impl std::error::Error for SupervisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(source) => Some(source),
            Self::MissingStdin => None,
            Self::MissingStdout => None,
            Self::Encode(source) => Some(source),
            Self::Flush(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::Wait(source) => Some(source),
            Self::Timeout { .. } => None,
            Self::ToolRoute(source) => Some(source),
        }
    }
}

/// One supervised child process connected over stdin/stdout.
pub struct SupervisedChild {
    command: ExtensionCommand,
    child: Child,
    stdin: EventWriter<BufWriter<ChildStdin>>,
    stdout_events: Receiver<Result<Event, DecodeError>>,
}

impl SupervisedChild {
    /// Spawns one supervised child process with piped stdin/stdout.
    pub fn spawn(command: ExtensionCommand) -> Result<Self, SupervisionError> {
        let mut child = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(SupervisionError::Spawn)?;

        let stdin = child.stdin.take().ok_or(SupervisionError::MissingStdin)?;
        let stdout = child.stdout.take().ok_or(SupervisionError::MissingStdout)?;
        let stdout_events = spawn_stdout_reader(stdout);

        Ok(Self {
            command,
            child,
            stdin: EventWriter::new(BufWriter::new(stdin)),
            stdout_events,
        })
    }

    /// Returns the extension command used to launch this child.
    #[must_use]
    pub fn command(&self) -> &ExtensionCommand {
        &self.command
    }

    /// Creates the lifecycle event emitted when the child becomes connected.
    #[must_use]
    pub fn ready_event(&self, connection_id: Option<ConnectionId>) -> Event {
        Event::ExtensionReady(ExtensionReady {
            extension_name: self.command.name.clone(),
            connection_id,
        })
    }

    /// Sends one protocol event to the child over stdin.
    pub fn send(&mut self, event: &Event) -> Result<(), SupervisionError> {
        self.stdin
            .write_event(event)
            .map_err(SupervisionError::Encode)?;
        self.stdin.flush().map_err(SupervisionError::Flush)
    }

    /// Reads one protocol event from the child, or returns `Ok(None)` on
    /// timeout.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Event>, SupervisionError> {
        match self.stdout_events.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) if is_unexpected_eof(&error) => Ok(None),
            Ok(Err(error)) => Err(SupervisionError::Decode(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    /// Checks whether the child has already exited.
    pub fn try_wait(&mut self) -> Result<Option<ChildExit>, SupervisionError> {
        self.child
            .try_wait()
            .map_err(SupervisionError::Wait)
            .map(|status| status.map(ChildExit::from_status))
    }

    /// Waits until the child exits or the timeout elapses.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<ChildExit, SupervisionError> {
        let started_at = Instant::now();
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            if timeout <= started_at.elapsed() {
                return Err(SupervisionError::Timeout { duration: timeout });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Creates the lifecycle event emitted when the child exits.
    #[must_use]
    pub fn exited_event(&self, exit: &ChildExit) -> Event {
        Event::ExtensionExited(ExtensionExited {
            extension_name: self.command.name.clone(),
            exit_code: exit.exit_code,
            signal: exit.signal,
        })
    }

    /// Removes tools owned by the disconnected child and emits an exit event.
    pub fn cleanup_disconnect(
        &self,
        registry: &mut ToolRegistry,
        connection_id: &str,
        exit: &ChildExit,
    ) -> DisconnectCleanup {
        DisconnectCleanup {
            removed_tools: registry.unregister_connection(connection_id),
            lifecycle_event: self.exited_event(exit),
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            Err(_) => {}
        }
    }
}

fn spawn_stdout_reader(stdout: std::process::ChildStdout) -> Receiver<Result<Event, DecodeError>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(stdout));
        loop {
            match reader.read_event() {
                Ok(event) => {
                    if sender.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

fn is_unexpected_eof(error: &DecodeError) -> bool {
    match error {
        DecodeError::Io(source) => source.kind() == io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}
