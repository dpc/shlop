use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shlop_cli_term::{OutputSender, Prompt, PromptResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut prompt, output_tx) = Prompt::new("> ")?;
    prompt.set_above_prompt("shlop v0.1.0 | type 'quit' to exit");
    prompt.set_right_prompt("[default]");
    prompt.start()?;

    spawn_animator(output_tx);

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

fn spawn_animator(tx: OutputSender) {
    thread::spawn(move || {
        let mut tick = 0u64;
        let ball_width = 30;
        let mut ball_pos: usize = 0;
        let mut ball_dir: isize = 1;

        loop {
            thread::sleep(Duration::from_millis(200));
            tick += 1;

            // Bouncing ball in the above-prompt area.
            let mut above = String::new();
            for row in 0..3 {
                let line_pos = ball_pos.wrapping_add(row * 3) % (ball_width * 2);
                let col = if line_pos < ball_width {
                    line_pos
                } else {
                    ball_width * 2 - line_pos
                };
                let mut line = " ".repeat(col);
                line.push('o');
                above.push_str(&line);
                if row < 2 {
                    above.push('\n');
                }
            }

            ball_pos = (ball_pos as isize + ball_dir) as usize;
            if ball_pos == 0 || ball_pos >= ball_width - 1 {
                ball_dir = -ball_dir;
            }

            let _ = tx.set_above_prompt(above);

            // Left prompt shows tick count.
            let _ = tx.set_left_prompt(format!("[{tick}] > "));

            // Right prompt shows current time.
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let hours = (secs / 3600) % 24;
            let mins = (secs / 60) % 60;
            let s = secs % 60;
            let _ = tx.set_right_prompt(format!("{hours:02}:{mins:02}:{s:02}"));

            // Log a tick message every second (every 5th iteration).
            if tick % 5 == 0 {
                let _ = tx.send(format!("[tick {}]", tick / 5));
            }
        }
    });
}
