//! Terminal prompt with async output support.
//!
//! Provides a line-editing prompt that can be interrupted by async output
//! without corrupting the display. No alternate screen mode — just
//! erase/print/redraw in the normal terminal buffer.

use std::io::{self, Stdout, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crossterm::cursor::MoveToColumn;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::QueueableCommand;

/// Events flowing into the UI loop from any producer.
pub enum UiEvent {
    /// A terminal key event from the input thread.
    Key(KeyEvent),
    /// Async output that should be printed above the prompt.
    Output(String),
    /// A request to shut down the UI loop.
    Quit,
}

/// Result of one prompt interaction.
pub enum PromptResult {
    /// The user submitted a line.
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
}

/// A channel-based sender for injecting async output into the prompt.
#[derive(Clone)]
pub struct OutputSender {
    tx: Sender<UiEvent>,
}

impl OutputSender {
    /// Sends an output line to be printed above the prompt.
    pub fn send(&self, text: String) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::Output(text))
    }

    /// Requests the UI loop to shut down.
    pub fn quit(&self) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::Quit)
    }
}

/// The interactive prompt engine.
pub struct Prompt {
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
    stdout: Stdout,
    input_handle: Option<JoinHandle<()>>,
    prefix: String,
    buffer: String,
    cursor: usize,
}

impl Prompt {
    /// Creates a new prompt with the given display prefix (e.g. `"> "`).
    ///
    /// Returns the prompt and an [`OutputSender`] that async producers can
    /// use to inject output.
    pub fn new(prefix: &str) -> io::Result<(Self, OutputSender)> {
        let (tx, rx) = mpsc::channel();
        let output_sender = OutputSender { tx: tx.clone() };
        Ok((
            Self {
                rx,
                tx,
                stdout: io::stdout(),
                input_handle: None,
                prefix: prefix.to_owned(),
                buffer: String::new(),
                cursor: 0,
            },
            output_sender,
        ))
    }

    /// Enters raw mode and starts the input reader thread.
    ///
    /// Must be called before [`read_line`]. Raw mode is exited on
    /// [`Drop`].
    pub fn start(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        self.draw_prompt()?;

        let tx = self.tx.clone();
        self.input_handle = Some(thread::spawn(move || {
            loop {
                let ev = match event::read() {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                if let CtEvent::Key(key) = ev {
                    if tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
            }
        }));

        Ok(())
    }

    /// Blocks until the user submits a line or signals EOF.
    pub fn read_line(&mut self) -> io::Result<PromptResult> {
        loop {
            let event = match self.rx.recv() {
                Ok(event) => event,
                Err(_) => return Ok(PromptResult::Eof),
            };

            match event {
                UiEvent::Key(key) => {
                    if let Some(result) = self.handle_key(key)? {
                        return Ok(result);
                    }
                }
                UiEvent::Output(text) => {
                    self.print_above(&text)?;
                }
                UiEvent::Quit => {
                    return Ok(PromptResult::Eof);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<PromptResult>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Enter => {
                // Move past the prompt line, then reset for next input.
                self.stdout.queue(Print("\r\n"))?.flush()?;
                let line = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                return Ok(Some(PromptResult::Line(line)));
            }

            KeyCode::Char('d') if ctrl => {
                if self.buffer.is_empty() {
                    self.stdout.queue(Print("\r\n"))?.flush()?;
                    return Ok(Some(PromptResult::Eof));
                }
            }

            KeyCode::Char('c') if ctrl => {
                // Cancel current line, start fresh.
                self.stdout.queue(Print("^C\r\n"))?.flush()?;
                self.buffer.clear();
                self.cursor = 0;
                self.draw_prompt()?;
            }

            KeyCode::Char('u') if ctrl => {
                // Clear from cursor to start of line.
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                self.draw_prompt()?;
            }

            KeyCode::Char('w') if ctrl => {
                // Delete word before cursor.
                if self.cursor > 0 {
                    let before = &self.buffer[..self.cursor];
                    let new_end = before
                        .trim_end()
                        .rfind(' ')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    self.buffer.drain(new_end..self.cursor);
                    self.cursor = new_end;
                    self.draw_prompt()?;
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                self.draw_prompt()?;
            }

            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                self.draw_prompt()?;
            }

            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.draw_prompt()?;
            }

            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.draw_prompt()?;
                }
            }

            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = next_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(self.cursor..next);
                    self.draw_prompt()?;
                }
            }

            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.draw_prompt()?;
                }
            }

            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    self.draw_prompt()?;
                }
            }

            KeyCode::Home => {
                self.cursor = 0;
                self.draw_prompt()?;
            }

            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.draw_prompt()?;
            }

            _ => {}
        }

        Ok(None)
    }

    /// Prints text above the prompt, then redraws the prompt line.
    pub fn print_output(&mut self, text: &str) -> io::Result<()> {
        self.print_above(text)
    }

    /// Erases the prompt line, prints text, then redraws the prompt.
    fn print_above(&mut self, text: &str) -> io::Result<()> {
        self.stdout
            .queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::CurrentLine))?;
        for line in text.lines() {
            self.stdout
                .queue(Print(line))?
                .queue(Print("\r\n"))?;
        }
        self.draw_prompt_queued()?;
        self.stdout.flush()
    }

    /// Redraws the prompt and buffer on the current line.
    fn draw_prompt(&mut self) -> io::Result<()> {
        self.draw_prompt_queued()?;
        self.stdout.flush()
    }

    fn draw_prompt_queued(&mut self) -> io::Result<()> {
        let cursor_col = self.prefix.len() + self.cursor;
        self.stdout
            .queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::CurrentLine))?
            .queue(Print(&self.prefix))?
            .queue(Print(&self.buffer))?
            .queue(MoveToColumn(cursor_col as u16))?;
        Ok(())
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_sub(1);
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
