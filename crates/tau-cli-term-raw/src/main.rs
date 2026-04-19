use std::thread;
use std::time::Duration;

use crossterm::terminal;
use tau_cli_term_raw::{
    Align, Color, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (term, handle) = Term::new("> ")?;

    // --- Above-sticky: header bar with background and center alignment ---
    let header_id = handle.new_block(
        StyledBlock::new(StyledText::from(Span::new(
            " tau-cli-term-raw demo ",
            Style::default().fg(Color::White).bold(),
        )))
        .bg(Color::DarkBlue)
        .align(Align::Center),
    );
    handle.push_above_sticky(header_id);

    // --- Above-sticky: help line with margins ---
    let help_id = handle.new_block(
        StyledBlock::new(StyledText::from(vec![
            Span::new("commands: ", Style::default().bold()),
            Span::new("quit", Style::default().fg(Color::Green)),
            Span::plain(" | "),
            Span::new("hello", Style::default().fg(Color::Green)),
            Span::plain(" | "),
            Span::new("remove", Style::default().fg(Color::Green)),
            Span::plain(" (removes the ball)  "),
            Span::new("add", Style::default().fg(Color::Green)),
            Span::plain(" (adds it back)"),
        ]))
        .margin_left(2)
        .margin_right(2),
    );
    handle.push_above_sticky(help_id);

    // --- Below: status bar with background ---
    let status_id = handle.new_block(
        StyledBlock::new(StyledText::from(Span::new(
            " STATUS: ready ",
            Style::default().fg(Color::Black).bold(),
        )))
        .bg(Color::DarkGreen),
    );
    handle.push_below(status_id);

    handle.set_right_prompt(StyledText::from(Span::new(
        "[default]",
        Style::default().fg(Color::DarkGrey),
    )));
    handle.redraw();

    spawn_animator(handle.clone());

    loop {
        match term.get_next_event()? {
            Event::Line(line) => match line.as_str() {
                "quit" => break,
                "hello" => {
                    // Styled history entry with background and margins.
                    handle.print_output(
                        StyledBlock::new(StyledText::from(vec![
                            Span::new("  Hello! ", Style::default().fg(Color::White).bold()),
                            Span::new(
                                "This is a styled history block.  ",
                                Style::default().fg(Color::White),
                            ),
                        ]))
                        .bg(Color::DarkMagenta)
                        .margin_left(1)
                        .margin_right(1),
                    );
                }
                "remove" => {
                    // Demonstrate block removal: remove the ball from
                    // above_active. The block stays in the store but
                    // isn't rendered.
                    handle.remove_above_active(BALL_ID);
                    handle.set_block(
                        STATUS_ID,
                        StyledBlock::new(StyledText::from(Span::new(
                            " STATUS: ball removed ",
                            Style::default().fg(Color::Black).bold(),
                        )))
                        .bg(Color::DarkYellow),
                    );
                    handle.redraw();
                }
                "add" => {
                    // Re-add the ball.
                    handle.push_above_active(BALL_ID);
                    handle.set_block(
                        STATUS_ID,
                        StyledBlock::new(StyledText::from(Span::new(
                            " STATUS: ball restored ",
                            Style::default().fg(Color::Black).bold(),
                        )))
                        .bg(Color::DarkGreen),
                    );
                    handle.redraw();
                }
                other => {
                    // Plain history echo.
                    handle.print_output(format!("you said: {other}"));
                }
            },
            Event::Eof => break,
            Event::Resize { width, height } => {
                handle.set_block(
                    STATUS_ID,
                    StyledBlock::new(StyledText::from(Span::new(
                        format!(" STATUS: resized to {width}x{height} "),
                        Style::default().fg(Color::Black).bold(),
                    )))
                    .bg(Color::DarkCyan),
                );
                handle.redraw();
            }
            Event::BufferChanged => {}
        }
    }

    Ok(())
}

// Well-known block ids shared between main and the animator thread.
// These are created by the animator with new_block, but since id
// allocation is sequential and the animator runs first, we know the
// ids.  A real app would pass them explicitly; here we hard-code for
// demo simplicity.
const BALL_ID: tau_cli_term_raw::BlockId = tau_cli_term_raw::BlockId(4);
const STATUS_ID: tau_cli_term_raw::BlockId = tau_cli_term_raw::BlockId(3);
// (ids 0-2 are header, help, status; 3=status was created as the 4th
//  block above, then ball=4, busy=5 in spawn_animator)

fn spawn_animator(handle: TermHandle) {
    // Pre-allocate block ids for the animated zones.
    let ball_id = handle.new_block("");
    handle.push_above_active(ball_id);
    let busy_id = handle.new_block("");
    handle.push_below(busy_id);

    thread::spawn(move || {
        let mut tick = 0u64;
        let mut ball_x: usize = 1;
        let mut ball_y: usize = 0;
        let mut ball_dx: isize = 1;
        let mut ball_dy: isize = 1;

        let ball_style = Style::default().fg(Color::Blue).bold();

        loop {
            thread::sleep(Duration::from_millis(200));
            tick += 1;

            let term_width = terminal::size()
                .map(|(w, _)| w as usize)
                .unwrap_or(80)
                .max(2);

            // --- Above-active: bouncing ball with dark background ---
            let mut ball_text = StyledText::new();
            for row in 0..3_usize {
                let mut plain_run = String::new();
                for col in 0..term_width {
                    if row == ball_y && col == ball_x {
                        if !plain_run.is_empty() {
                            ball_text.push(Span::plain(std::mem::take(&mut plain_run)));
                        }
                        ball_text.push(Span::new("o", ball_style));
                    } else {
                        plain_run.push(' ');
                    }
                }
                if !plain_run.is_empty() {
                    ball_text.push(Span::plain(plain_run));
                }
                if row < 2 {
                    ball_text.push(Span::plain("\n"));
                }
            }

            if ball_x >= term_width.saturating_sub(1) {
                ball_x = term_width.saturating_sub(2);
                ball_dx = -1;
            }
            ball_x = (ball_x as isize + ball_dx) as usize;
            ball_y = (ball_y as isize + ball_dy) as usize;
            if ball_x == 0 || ball_x >= term_width.saturating_sub(1) {
                ball_dx = -ball_dx;
            }
            if ball_y == 0 || ball_y >= 2 {
                ball_dy = -ball_dy;
            }

            handle.set_block(
                ball_id,
                StyledBlock::new(ball_text).bg(Color::Rgb {
                    r: 20,
                    g: 20,
                    b: 40,
                }),
            );

            // --- Left prompt: tick counter with color ---
            handle.set_left_prompt(StyledText::from(vec![
                Span::new(format!("[{tick}]"), Style::default().fg(Color::DarkYellow)),
                Span::plain(" > "),
            ]));

            // --- Right prompt: clock ---
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let hours = (secs / 3600) % 24;
            let mins = (secs / 60) % 60;
            let s = secs % 60;
            handle.set_right_prompt(StyledText::from(Span::new(
                format!("{hours:02}:{mins:02}:{s:02}"),
                Style::default().fg(Color::DarkGrey),
            )));

            // --- Below: progress bar ---
            let bar_inner = term_width.saturating_sub(2); // minus [ and ]
            let cycle = (tick as usize) % (bar_inner + 1);
            let filled: String = "=".repeat(cycle);
            let empty: String = " ".repeat(bar_inner - cycle);
            let bar_style = Style::default().fg(Color::DarkYellow);
            handle.set_block(
                busy_id,
                StyledBlock::new(StyledText::from(vec![
                    Span::new("[", bar_style),
                    Span::new(filled, bar_style),
                    Span::new(empty, Style::default()),
                    Span::new("]", bar_style),
                ])),
            );

            handle.redraw();

            // --- History: periodic tick message (styled) ---
            if tick % 5 == 0 {
                let t = tick / 5;
                handle.print_output(StyledBlock::new(StyledText::from(vec![
                    Span::new(
                        format!("[tick {t}] "),
                        Style::default().fg(Color::DarkCyan).bold(),
                    ),
                    Span::new(
                        "periodic status update",
                        Style::default().fg(Color::DarkGrey).italic(),
                    ),
                ])));
            }
        }
    });
}
