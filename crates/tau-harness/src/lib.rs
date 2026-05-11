//! Harness daemon: manages extensions, routing, session state, and
//! serves socket clients.
//!
//! Each connection has a reader thread and a writer thread.  All
//! reader threads feed one shared `mpsc::channel`.  The harness event
//! loop blocks on `rx.recv()` and dispatches instantly.  The bus
//! delivers outgoing events by sending to per-connection writer
//! channels (non-blocking).  Writer threads drain their channel and
//! write to the stream; on channel close they run the shutdown
//! sequence for that connection.

pub mod runtime_dir;

mod conversation;
mod daemon;
mod debug_log;
mod dirs;
mod discovery;
mod error;
mod event;
mod event_log;
mod extension;
mod format;
mod harness;
mod model;
mod prompt;
mod session_cleanup;
mod settings;
mod turn;
mod version;

pub fn dump_initial_prompt(
    out_path: &std::path::Path,
    user_message: &str,
) -> Result<(), HarnessError> {
    harness::Harness::dump_initial_prompt(out_path, user_message)
}

pub use tau_core::{SessionEntry, SessionMeta, SessionTree, list_session_metas, session_is_locked};

pub use crate::daemon::{
    EmbeddedOptions, InteractionOutcome, ServeOptions, SessionLaunchStatus, run_component,
    run_daemon, run_daemon_with_config, run_embedded_message, run_embedded_message_with_options,
    run_embedded_message_with_trace, run_harness_daemon, send_daemon_message,
    send_daemon_message_with_trace,
};
#[cfg(any(test, feature = "echo-agent"))]
pub use crate::daemon::{run_daemon_with_echo, run_embedded_message_with_echo};
pub use crate::error::HarnessError;
pub use crate::format::{format_extension_event, format_tool_progress};
pub use crate::settings::{builtin_extensions, default_config};
