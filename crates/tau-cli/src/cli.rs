use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tau_harness::{default_policy_store_path, default_session_id, default_session_store_path};

#[derive(Parser)]
#[command(name = "tau", about = "Unix-native LLM agent harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run an interactive agent session.
    ///
    /// By default, `tau run` spawns a new harness daemon and attaches
    /// to it for the duration of the session. Pass `--attach` (or
    /// `-a`) to connect to an already-running daemon for the current
    /// project instead — useful for a second UI, or for reconnecting
    /// after `/detach`.
    Run {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to extension configuration file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Attach to an existing harness daemon for this project
        /// instead of spawning a new one. Errors if no daemon is
        /// running.
        #[arg(short = 'a', long)]
        attach: bool,
    },

    /// List all sessions
    SessionList {
        /// Path to session store
        #[arg(long, default_value_os_t = default_session_store_path())]
        session_store: PathBuf,
    },

    /// Show a single session's history
    SessionShow {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to session store
        #[arg(long, default_value_os_t = default_session_store_path())]
        session_store: PathBuf,
    },

    /// Show persisted policy approvals
    PolicyShow {
        /// Path to policy store
        #[arg(long, default_value_os_t = default_policy_store_path())]
        policy_store: PathBuf,
    },

    /// Copy sample config files to ~/.config/tau/
    Init {
        /// Overwrite existing config files
        #[arg(long)]
        force: bool,
    },

    /// Manage LLM providers (add, login, list-models)
    Provider {
        /// Subcommand and arguments (e.g. add, login [name], list-models
        /// [name])
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Run an internal component as a standalone process (used by the
    /// harness to spawn extensions from the unified binary).
    #[command(hide = true)]
    Component {
        /// Component name (agent, ext-fs, harness)
        name: String,
    },
}
