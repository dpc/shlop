use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tau_harness::{default_session_id, default_state_dir};

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
        /// Resume an existing session.
        ///
        /// Bare `-r` resumes the most recent session whose `meta.json.cwd`
        /// matches the current working directory. `-r <id>` resumes that
        /// specific session id. Without `-r`, a fresh session id is minted
        /// (`<basename(cwd)>-<rand6>`).
        #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,

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
        /// Path to session state directory
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },

    /// Show a single session's history
    SessionShow {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to session state directory
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },

    /// Show persisted policy approvals
    PolicyShow {
        /// Path to session state directory (policy.cbor lives inside)
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
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
        /// Component name (agent, ext-shell, harness)
        name: String,
    },
}
