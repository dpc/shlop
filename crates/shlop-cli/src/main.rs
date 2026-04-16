use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use shlop_cli::{
    CliError, ServeOptions, default_policy_store_path, default_session_id,
    default_session_store_path, default_socket_path, run_daemon, run_embedded_message_with_trace,
    send_daemon_message_with_trace,
};

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
                    _ => print_help(),
                }
            }
            let message = message.unwrap_or_else(|| "hello".to_owned());
            let outcome = run_embedded_message_with_trace(session_store, &session_id, &message)?;
            println!("user: {message}");
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
                    _ => print_help(),
                }
            }
            eprintln!("serving on {}", socket_path.display());
            run_daemon(
                socket_path,
                session_store,
                ServeOptions {
                    max_clients: None,
                    policy_store_path: Some(policy_store),
                },
            )
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
            for progress in outcome.progress_messages {
                println!("progress: {progress}");
            }
            println!("agent: {}", outcome.response);
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

fn print_help() {
    eprintln!("shlop-cli commands:");
    eprintln!("  embedded [--message TEXT] [--session-id ID] [--session-store PATH]");
    eprintln!("  serve [--socket PATH] [--session-store PATH] [--policy-store PATH]");
    eprintln!("  send [--message TEXT] [--session-id ID] [--socket PATH]");
}
