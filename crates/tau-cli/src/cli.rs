use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{
    default_policy_store_path, default_session_id, default_session_store_path, default_socket_path,
};

#[derive(Parser)]
#[command(name = "tau", about = "Unix-native LLM agent harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Interactive chat session
    Chat {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to session store
        #[arg(long, default_value_os_t = default_session_store_path())]
        session_store: PathBuf,

        /// Path to extension configuration file
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Run a single embedded interaction (interactive if --message is omitted)
    Embedded {
        /// Message to send (omit for interactive mode)
        #[arg(long)]
        message: Option<String>,

        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to session store
        #[arg(long, default_value_os_t = default_session_store_path())]
        session_store: PathBuf,

        /// Path to extension configuration file
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Start the daemon and accept socket clients
    Serve {
        /// Unix socket path
        #[arg(long, default_value_os_t = default_socket_path())]
        socket: PathBuf,

        /// Path to session store
        #[arg(long, default_value_os_t = default_session_store_path())]
        session_store: PathBuf,

        /// Path to policy store
        #[arg(long, default_value_os_t = default_policy_store_path())]
        policy_store: PathBuf,

        /// Path to extension configuration file
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Send a single message to a running daemon
    Send {
        /// Message to send
        #[arg(long, default_value = "hello")]
        message: String,

        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Unix socket path of the daemon
        #[arg(long, default_value_os_t = default_socket_path())]
        socket: PathBuf,
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
}
