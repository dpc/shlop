//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.

pub mod cli;

mod chat;
mod daemon;
mod event_renderer;
mod tool_render;
mod ui_logging;

use std::sync::{Mutex, MutexGuard};
use std::{fmt, io};

use tau_harness::{SessionLaunchStatus, runtime_dir};

use crate::chat::run_chat;
use crate::daemon::resolve_run_session_id;

/// Single shared message for mutex-poison panics: every mutex in this
/// crate is held only for short, infallible critical sections, so poison
/// means another thread panicked mid-update and continuing is unsafe.
pub(crate) const MUTEX_POISONED: &str = "mutex poisoned";

/// Locks `mutex` and panics on poison. Centralizes the panic message so
/// individual call sites read as `let mut g = locked(&m);` instead of
/// repeating `.expect("... mutex poisoned")` everywhere.
pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect(MUTEX_POISONED)
}

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

const STARTUP_PUNS: &[&str] = &[
    "Tau is like Pi, but twice as much.",
    "A new angle on coding agents.",
    "Tau day is every day if you care about circles enough.",
    "Come for the agent, stay for the circumference discourse.",
    "Tau is the irrational choice for rational Unix hackers.",
    "Small tools, loosely joined — that’s the Tau of Unix.",
    "In Tau, what goes around comes around over stdio.",
    "We’ve come full τurn.",
    "Tau keeps the loop tight and the pipes honest.",
    "Every extension gets its turn in Tau.",
    "Tau speaks fluent stdio with a circular accent.",
    "Agents, tools, sockets, loops: a well-rounded lineup.",
    "Ready, set, Tau!",
    "Tau day to code.",
    "Tau-tau control.",
    "Tau-tally operational.",
    "Tau much power in one terminal.",
    "Tau infinity and beyond.",
    "Tau the line between human and agent.",
    "Tau’s what I’m talking about.",
    "One shell to Tau them all.",
    "Tau small step for code, one giant leap for CLI-kind.",
    "Tau-powered, terminal-native.",
    "Complete revolution.",
    "Wrapping around nicely.",
    "Continuous on S¹, probably.",
    "Cohomology remains left as exercise.",
];

pub(crate) fn random_startup_pun() -> &'static str {
    use rand::Rng;
    let idx = rand::thread_rng().gen_range(0..STARTUP_PUNS.len());
    STARTUP_PUNS[idx]
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the CLI.
#[derive(Debug)]
pub enum CliError {
    Io(io::Error),
    Encode(tau_proto::EncodeError),
    Harness(tau_harness::HarnessError),
    Inspect(tau_session_inspect::InspectError),
    DaemonExited(String),
    NoRunningDaemon,
    Participant(String),
    SessionNotFound(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Encode(source) => write!(f, "encode error: {source}"),
            Self::Harness(source) => write!(f, "harness error: {source}"),
            Self::Inspect(source) => write!(f, "inspect error: {source}"),
            Self::DaemonExited(msg) => write!(f, "harness daemon exited: {msg}"),
            Self::NoRunningDaemon => f.write_str(
                "no harness daemon running for this project — \
                 drop `--attach` to spawn one",
            ),
            Self::Participant(msg) => write!(f, "participant error: {msg}"),
            Self::SessionNotFound(id) => write!(f, "session not found: `{id}`"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<tau_harness::HarnessError> for CliError {
    fn from(source: tau_harness::HarnessError) -> Self {
        Self::Harness(source)
    }
}

impl From<tau_session_inspect::InspectError> for CliError {
    fn from(source: tau_session_inspect::InspectError) -> Self {
        Self::Inspect(source)
    }
}

// ---------------------------------------------------------------------------
// Build labels and version helpers (shared by chat banner, EventRenderer
// banner, and `tau --version`).
// ---------------------------------------------------------------------------

fn build_revision() -> String {
    match (built_info::GIT_COMMIT_HASH_SHORT, built_info::GIT_DIRTY) {
        (Some(hash), Some(true)) => format!("{hash}-modified"),
        (Some(hash), _) => hash.to_owned(),
        _ => "unknown".to_owned(),
    }
}

fn build_last_modified() -> Option<String> {
    option_env!("TAU_LAST_MODIFIED")
        .filter(|date| !date.is_empty())
        .map(str::to_owned)
        .or_else(|| short_built_time(built_info::BUILT_TIME_UTC))
        .filter(|date| date != "1980-01-01 00:00")
}

fn short_built_time(time: &str) -> Option<String> {
    let input_format = time::macros::format_description!(
        "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    let output_format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::parse(time, input_format)
        .ok()?
        .format(output_format)
        .ok()
}

pub(crate) fn build_label_parts() -> (String, String) {
    let version = format!("tau {}", env!("CARGO_PKG_VERSION"));
    let build = match build_last_modified() {
        Some(date) => format!("({}, {})", build_revision(), date),
        None => format!("({})", build_revision()),
    };
    (version, build)
}

fn version_label() -> String {
    let (version, build) = build_label_parts();
    format!("{version} {build}")
}

// ---------------------------------------------------------------------------
// Short-id minting (used for both session ids and per-UI log dir ids)
// ---------------------------------------------------------------------------

/// Build an id of the form `<prefix>-<6 base36 chars>`. Used for both
/// session and UI ids so the visual shape is consistent.
pub(crate) fn mint_short_id(prefix: &str) -> String {
    use rand::distributions::Distribution;

    struct Base36;
    impl Distribution<char> for Base36 {
        fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> char {
            let n: u8 = rng.gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + (n - 10)) as char
            }
        }
    }

    let suffix: String = Base36.sample_iter(rand::thread_rng()).take(6).collect();
    format!("{prefix}-{suffix}")
}

// ---------------------------------------------------------------------------
// `tau init`
// ---------------------------------------------------------------------------

const SAMPLE_CLI: &str = include_str!("../../../config/cli.json5");
const SAMPLE_HARNESS: &str = include_str!("../../../config/harness.json5");
const SAMPLE_MODELS: &str = include_str!("../../../config/models.json5");

fn run_init(force: bool) -> Result<(), CliError> {
    let Some(dir) = tau_config::settings::config_dir() else {
        return Err(CliError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )));
    };
    std::fs::create_dir_all(&dir)?;

    let files = [
        ("cli.json5", SAMPLE_CLI),
        ("harness.json5", SAMPLE_HARNESS),
        ("models.json5", SAMPLE_MODELS),
    ];

    for (name, content) in &files {
        let path = dir.join(name);
        if path.exists() && !force {
            eprintln!(
                "skip: {} (exists, use --force to overwrite)",
                path.display()
            );
        } else {
            std::fs::write(&path, content)?;
            eprintln!("wrote: {}", path.display());
        }
    }

    eprintln!("next: use `tau provider add` to log in to a hosted LLM provider");

    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

pub type ComponentRunner = fn() -> Result<(), Box<dyn std::error::Error>>;

pub struct Component {
    pub name: &'static str,
    pub runner: ComponentRunner,
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command.
pub fn main_with_args() -> std::process::ExitCode {
    main_with_args_and_components(&[])
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command, using caller-provided component registrations for hidden
/// `ext`/`component` dispatch.
pub fn main_with_args_and_components(components: &[Component]) -> std::process::ExitCode {
    use std::process::ExitCode;

    use clap::Parser;

    let run = || -> Result<(), CliError> {
        let cli::Cli {
            version,
            run,
            command,
        } = cli::Cli::parse();
        if version {
            println!("{}", version_label());
            return Ok(());
        }

        let command = command.unwrap_or(cli::Command::Run(run));

        match command {
            cli::Command::Run(cli::RunArgs {
                resume,
                config: _config,
                attach,
            }) => {
                let (session_id, session_status) = if attach {
                    let cwd = std::env::current_dir()?;
                    let daemon_dir =
                        runtime_dir::find_harness_for_dir(&cwd).ok_or(CliError::NoRunningDaemon)?;
                    let daemon_session_id =
                        runtime_dir::read_session_id(&daemon_dir).ok_or_else(|| {
                            CliError::Participant(
                                "running daemon did not publish its session id".to_owned(),
                            )
                        })?;
                    if let Some(requested) = resume.as_deref().filter(|s| !s.is_empty())
                        && requested != daemon_session_id
                    {
                        return Err(CliError::Participant(format!(
                            "--attach: daemon is bound to session `{daemon_session_id}`, \
                             cannot resume `{requested}` (start a fresh daemon for that)"
                        )));
                    }
                    (daemon_session_id, SessionLaunchStatus::Resumed)
                } else {
                    resolve_run_session_id(resume.as_deref())?
                };
                run_chat(&session_id, attach, session_status)
            }

            cli::Command::SessionList { state_dir } => {
                for line in tau_session_inspect::session_list_lines(state_dir)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::SessionShow {
                session_id,
                state_dir,
            } => {
                for line in tau_session_inspect::session_lines(state_dir, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::PolicyShow { state_dir } => {
                for line in tau_session_inspect::policy_lines(state_dir.join("policy.cbor"))? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Init { force } => run_init(force),

            cli::Command::Provider { args } => {
                tau_provider::run(&args).map_err(|e| CliError::Participant(e.to_string()))
            }

            cli::Command::Dev { command } => match command {
                cli::DevCommand::DumpInitialPrompt { out, message } => {
                    tau_harness::dump_initial_prompt(&out, &message)?;
                    println!("wrote {}", out.display());
                    Ok(())
                }
            },

            cli::Command::Ext { name } => {
                ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info");
                let built_in_components = [
                    Component {
                        name: "agent",
                        runner: tau_agent::run_stdio,
                    },
                    Component {
                        name: "harness",
                        runner: tau_harness::run_component,
                    },
                ];
                let runner = built_in_components
                    .iter()
                    .chain(components)
                    .find(|component| component.name == name)
                    .map(|component| component.runner)
                    .ok_or_else(|| {
                        let available = built_in_components
                            .iter()
                            .chain(components)
                            .map(|component| component.name)
                            .collect::<Vec<_>>()
                            .join(", ");
                        CliError::Participant(format!(
                            "unknown extension: {name}\navailable: {available}"
                        ))
                    })?;
                runner().map_err(|e| CliError::Participant(e.to_string()))
            }
        }
    };

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
