use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::terminal;
use tau_cli_term::{OutputSender, Prompt, PromptResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut prompt, output_tx) = Prompt::new("> ")?;
    prompt.set_above_prompt("tau v0.1.0 | type 'quit' to exit");
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
        let mut ball_x: usize = 1;
        let mut ball_y: usize = 0;
        let mut ball_dx: isize = 1;
        let mut ball_dy: isize = 1;

        loop {
            thread::sleep(Duration::from_millis(200));
            tick += 1;

            // Bouncing ball in a 3-line-high box spanning terminal width.
            let ball_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80).max(2);
            let mut above = String::new();
            for row in 0..3_usize {
                for col in 0..ball_width {
                    if row == ball_y && col == ball_x {
                        above.push('o');
                    } else {
                        above.push(' ');
                    }
                }
                if row < 2 {
                    above.push('\n');
                }
            }

            // Clamp in case terminal was resized smaller.
            if ball_x >= ball_width.saturating_sub(1) {
                ball_x = ball_width.saturating_sub(2);
                ball_dx = -1;
            }
            ball_x = (ball_x as isize + ball_dx) as usize;
            ball_y = (ball_y as isize + ball_dy) as usize;
            if ball_x == 0 || ball_x >= ball_width.saturating_sub(1) {
                ball_dx = -ball_dx;
            }
            if ball_y == 0 || ball_y >= 2 {
                ball_dy = -ball_dy;
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
