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
    /// Interactive chat session
    Chat {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to extension configuration file
        #[arg(long)]
        config: Option<PathBuf>,
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

    /// Run an internal component as a standalone process (used by the
    /// harness to spawn extensions from the unified binary).
    #[command(hide = true)]
    Component {
        /// Component name (agent, ext-fs, ext-shell, harness)
        name: String,
    },
}
