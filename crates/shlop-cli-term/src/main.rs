use std::thread;
use std::time::Duration;

use shlop_cli_term::{OutputSender, Prompt, PromptResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut prompt, output_tx) = Prompt::new("> ")?;
    prompt.start()?;

    // Spawn a periodic async event source (1 event/sec).
    spawn_ticker(output_tx);

    loop {
        match prompt.read_line()? {
            PromptResult::Line(line) => {
                if line == "quit" {
                    break;
                }
                prompt.print_output(&format!("you said: {line}"))?;
            }
            PromptResult::Eof => break,
        }
    }

    Ok(())
}

fn spawn_ticker(tx: OutputSender) {
    thread::spawn(move || {
        let mut count = 0u64;
        loop {
            thread::sleep(Duration::from_secs(1));
            count += 1;
            if tx.send(format!("[tick {count}]")).is_err() {
                break;
            }
        }
    });
}
