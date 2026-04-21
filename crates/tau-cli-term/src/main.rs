use std::thread;
use std::time::Duration;

use tau_cli_term::{
    Color, Event, HighTerm, SlashCommand, Span, Style, StyledBlock, StyledText, TermHandle,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let commands = vec![
        SlashCommand::new("/new", "Start a new conversation"),
        SlashCommand::new("/new-foo", "Create a new foo resource"),
        SlashCommand::new("/new-bar", "Create a new bar resource"),
        SlashCommand::new("/tree", "Show the project tree"),
        SlashCommand::new("/quit", "Exit the application"),
    ];

    let (mut term, handle, _completion_data) =
        HighTerm::new("> ", commands, tau_themes::Theme::builtin())?;

    // Header.
    let header_id = handle.new_block(
        StyledBlock::new(StyledText::from(Span::new(
            " tau-cli-term demo — type / for commands ",
            Style::default().fg(Color::White).bold(),
        )))
        .bg(Color::DarkBlue)
        .align(tau_cli_term::Align::Center),
    );
    handle.push_above_sticky(header_id);

    // Status bar.
    let status_id = handle.new_block(
        StyledBlock::new(StyledText::from(Span::new(
            " ready ",
            Style::default().fg(Color::Black).bold(),
        )))
        .bg(Color::DarkGreen),
    );
    handle.push_below(status_id);
    handle.redraw();

    spawn_clock(handle.clone());

    loop {
        match term.get_next_event()? {
            Event::Line(line) => {
                if line == "/quit" {
                    break;
                }
                term.print_output(StyledBlock::new(StyledText::from(vec![
                    Span::new("> ", Style::default().fg(Color::DarkGrey)),
                    Span::plain(&line),
                ])));
                handle.set_block(
                    status_id,
                    StyledBlock::new(StyledText::from(Span::new(
                        format!(" executed: {line} "),
                        Style::default().fg(Color::Black).bold(),
                    )))
                    .bg(Color::DarkGreen),
                );
                handle.redraw();
            }
            Event::Eof => break,
            Event::Resize { width, height } => {
                handle.set_block(
                    status_id,
                    StyledBlock::new(StyledText::from(Span::new(
                        format!(" resized: {width}x{height} "),
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

fn spawn_clock(handle: TermHandle) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(1));
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
            handle.redraw();
        }
    });
}
