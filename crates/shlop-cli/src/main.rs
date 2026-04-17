use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use shlop_cli::{
    CliError, ServeOptions, default_policy_store_path, default_session_id,
    default_session_store_path, default_socket_path, policy_lines, run_daemon,
    run_daemon_with_config, run_embedded_message_with_config, run_embedded_message_with_trace,
    send_daemon_message_with_trace, session_lines, session_list_lines,
};
use shlop_config::{Config, LoadConfigError, LoadOptions};

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
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.as_str() {
        "embedded" => {
            let mut message = None;
            let mut session_id = default_session_id().to_owned();
            let mut session_store = default_session_store_path();
            let mut config_path: Option<PathBuf> = None;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--message" => message = args.next(),
                    "--session-id" => {
                        if let Some(value) = args.next() {
                            session_id = value;
                        }
                    }
                    "--session-store" => {
                        if let Some(value) = args.next() {
                            session_store = PathBuf::from(value);
                        }
                    }
                    "--config" => {
                        config_path = args.next().map(PathBuf::from);
                    }
                    _ => print_help(),
                }
            }
            let message = message.unwrap_or_else(|| "hello".to_owned());
            let outcome = match try_load_config(config_path.as_deref())? {
                Some(config) => {
                    run_embedded_message_with_config(&config, session_store, &session_id, &message)?
                }
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
        "serve" => {
            let mut socket_path = default_socket_path();
            let mut session_store = default_session_store_path();
            let mut policy_store = default_policy_store_path();
            let mut config_path: Option<PathBuf> = None;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--socket" => {
                        if let Some(value) = args.next() {
                            socket_path = PathBuf::from(value);
                        }
                    }
                    "--session-store" => {
                        if let Some(value) = args.next() {
                            session_store = PathBuf::from(value);
                        }
                    }
                    "--policy-store" => {
                        if let Some(value) = args.next() {
                            policy_store = PathBuf::from(value);
                        }
                    }
                    "--config" => {
                        config_path = args.next().map(PathBuf::from);
                    }
                    _ => print_help(),
                }
            }
            eprintln!("serving on {}", socket_path.display());
            let options = ServeOptions {
                max_clients: None,
                policy_store_path: Some(policy_store),
            };
            match try_load_config(config_path.as_deref())? {
                Some(config) => {
                    run_daemon_with_config(&config, socket_path, session_store, options)
                }
                None => run_daemon(socket_path, session_store, options),
            }
        }
        "send" => {
            let mut message = None;
            let mut session_id = default_session_id().to_owned();
            let mut socket_path = default_socket_path();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--message" => message = args.next(),
                    "--session-id" => {
                        if let Some(value) = args.next() {
                            session_id = value;
                        }
                    }
                    "--socket" => {
                        if let Some(value) = args.next() {
                            socket_path = PathBuf::from(value);
                        }
                    }
                    _ => print_help(),
                }
            }
            let message = message.unwrap_or_else(|| "hello".to_owned());
            let outcome = send_daemon_message_with_trace(socket_path, &session_id, &message)?;
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
        "session-list" => {
            let mut session_store = default_session_store_path();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--session-store" => {
                        if let Some(value) = args.next() {
                            session_store = PathBuf::from(value);
                        }
                    }
                    _ => print_help(),
                }
            }
            for line in session_list_lines(session_store)? {
                println!("{line}");
            }
            Ok(())
        }
        "session-show" => {
            let mut session_id = default_session_id().to_owned();
            let mut session_store = default_session_store_path();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--session-id" => {
                        if let Some(value) = args.next() {
                            session_id = value;
                        }
                    }
                    "--session-store" => {
                        if let Some(value) = args.next() {
                            session_store = PathBuf::from(value);
                        }
                    }
                    _ => print_help(),
                }
            }
            for line in session_lines(session_store, &session_id)? {
                println!("{line}");
            }
            Ok(())
        }
        "policy-show" => {
            let mut policy_store = default_policy_store_path();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--policy-store" => {
                        if let Some(value) = args.next() {
                            policy_store = PathBuf::from(value);
                        }
                    }
                    _ => print_help(),
                }
            }
            for line in policy_lines(policy_store)? {
                println!("{line}");
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
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

    match shlop_config::load(&options) {
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

fn print_help() {
    eprintln!("shlop-cli commands:");
    eprintln!(
        "  embedded [--message TEXT] [--session-id ID] [--session-store PATH] [--config PATH]"
    );
    eprintln!(
        "  serve [--socket PATH] [--session-store PATH] [--policy-store PATH] [--config PATH]"
    );
    eprintln!("  send [--message TEXT] [--session-id ID] [--socket PATH]");
    eprintln!("  session-list [--session-store PATH]");
    eprintln!("  session-show [--session-id ID] [--session-store PATH]");
    eprintln!("  policy-show [--policy-store PATH]");
}
