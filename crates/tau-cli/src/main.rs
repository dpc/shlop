use std::process::ExitCode;

use clap::Parser;
use tau_cli::cli::{Cli, Command};
use tau_cli::{
    CliError, ServeOptions, policy_lines, run_daemon, run_daemon_with_config,
    run_embedded_message_with_config, run_embedded_message_with_trace, run_interactive,
    run_interactive_with_config, send_daemon_message_with_trace, session_lines, session_list_lines,
};
use tau_config::{Config, LoadConfigError, LoadOptions};

fn main() -> ExitCode {
    match run_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_main() -> Result<(), CliError> {
    let cli = Cli::parse();

    match cli.command {
        Command::Chat {
            session_id,
            session_store,
            config,
        } => match try_load_config(config.as_deref())? {
            Some(cfg) => run_interactive_with_config(&cfg, session_store, &session_id),
            None => run_interactive(session_store, &session_id),
        },

        Command::Embedded {
            message,
            session_id,
            session_store,
            config,
        } => match message {
            Some(message) => {
                let outcome = match try_load_config(config.as_deref())? {
                    Some(cfg) => run_embedded_message_with_config(
                        &cfg,
                        session_store,
                        &session_id,
                        &message,
                    )?,
                    None => {
                        run_embedded_message_with_trace(session_store, &session_id, &message)?
                    }
                };
                println!("user: {message}");
                for lifecycle in outcome.lifecycle_messages {
                    println!("lifecycle: {lifecycle}");
                }
                for progress in outcome.progress_messages {
                    println!("progress: {progress}");
                }
                println!("agent: {}", outcome.response);
                Ok(())
            }
            None => match try_load_config(config.as_deref())? {
                Some(cfg) => run_interactive_with_config(&cfg, session_store, &session_id),
                None => run_interactive(session_store, &session_id),
            },
        },

        Command::Serve {
            socket,
            session_store,
            policy_store,
            config,
        } => {
            eprintln!("serving on {}", socket.display());
            let options = ServeOptions {
                max_clients: None,
                policy_store_path: Some(policy_store),
            };
            match try_load_config(config.as_deref())? {
                Some(cfg) => run_daemon_with_config(&cfg, socket, session_store, options),
                None => run_daemon(socket, session_store, options),
            }
        }

        Command::Send {
            message,
            session_id,
            socket,
        } => {
            let outcome = send_daemon_message_with_trace(socket, &session_id, &message)?;
            println!("user: {message}");
            for lifecycle in outcome.lifecycle_messages {
                println!("lifecycle: {lifecycle}");
            }
            for progress in outcome.progress_messages {
                println!("progress: {progress}");
            }
            println!("agent: {}", outcome.response);
            Ok(())
        }

        Command::SessionList { session_store } => {
            for line in session_list_lines(session_store)? {
                println!("{line}");
            }
            Ok(())
        }

        Command::SessionShow {
            session_id,
            session_store,
        } => {
            for line in session_lines(session_store, &session_id)? {
                println!("{line}");
            }
            Ok(())
        }

        Command::PolicyShow { policy_store } => {
            for line in policy_lines(policy_store)? {
                println!("{line}");
            }
            Ok(())
        }
    }
}

/// Attempts to load a configuration file.
///
/// If an explicit path is given, it must exist. Otherwise, tries the default
/// user/project config paths and returns `None` if neither exists.
fn try_load_config(explicit_path: Option<&std::path::Path>) -> Result<Option<Config>, CliError> {
    let options = match explicit_path {
        Some(path) => LoadOptions {
            user_config_path: Some(path.to_owned()),
            enable_project_config: false,
            project_config_path: None,
        },
        None => LoadOptions {
            user_config_path: None,
            enable_project_config: true,
            project_config_path: None,
        },
    };

    match tau_config::load(&options) {
        Ok(config) if !config.extensions.is_empty() => Ok(Some(config)),
        Ok(_) => Ok(None),
        Err(LoadConfigError::MissingExplicitFile { path }) => Err(CliError::Participant(
            format!("config file not found: {}", path.display()),
        )),
        Err(
            LoadConfigError::CurrentDirectory(_)
            | LoadConfigError::ReadFile { .. }
            | LoadConfigError::ParseFile { .. },
        ) => Ok(None),
    }
}
