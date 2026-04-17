//! Terminal prompt with async output support.
//!
//! Provides a line-editing prompt that can be interrupted by async output
//! without corrupting the display. No alternate screen mode — just
//! diff-based rendering in the normal terminal buffer.
//!
//! # Prompt zones
//!
//! The prompt display is composed of three configurable zones:
//!
//! - **Above-prompt**: Optional multi-line text displayed above the input
//!   line (e.g. status information, context). Updated via
//!   [`Prompt::set_above_prompt`].
//! - **Left-prompt**: The prefix before user input (e.g. `"> "`). Updated
//!   via [`Prompt::set_left_prompt`].
//! - **Right-prompt**: Right-justified text on the first physical line of
//!   the input area. Hidden when the user input would overlap it. Updated
//!   via [`Prompt::set_right_prompt`].
//!
//! All zones are part of the diff-rendered screen area, so changes are
//! applied with minimal terminal I/O.
//!
//! # Rendering
//!
//! Uses a dual-buffer diff approach inspired by fish shell's `screen.rs`:
//! the [`screen::Screen`] tracks what is currently on the terminal
//! ("actual") and diffs it against what should be there ("desired"),
//! emitting only the escape sequences for characters that changed.
//!
//! References:
//! - fish shell screen rendering: <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>

pub mod screen;

use std::io::{self, Stdout, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::QueueableCommand;

use screen::{Screen, layout_lines};

/// Events flowing into the UI loop from any producer.
pub enum UiEvent {
    /// A terminal key event from the input thread.
    Key(KeyEvent),
    /// Async output that should be printed above the prompt.
    Output(String),
    /// Update a prompt zone and re-render.
    SetAbovePrompt(String),
    /// Update the left prompt and re-render.
    SetLeftPrompt(String),
    /// Update the right prompt and re-render.
    SetRightPrompt(String),
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

    /// Updates the above-prompt text and triggers a re-render.
    pub fn set_above_prompt(&self, text: String) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::SetAbovePrompt(text))
    }

    /// Updates the left prompt and triggers a re-render.
    pub fn set_left_prompt(&self, text: String) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::SetLeftPrompt(text))
    }

    /// Updates the right prompt and triggers a re-render.
    pub fn set_right_prompt(&self, text: String) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::SetRightPrompt(text))
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
    screen: Screen,
    input_handle: Option<JoinHandle<()>>,
    above_prompt: String,
    left_prompt: String,
    right_prompt: String,
    buffer: String,
    cursor: usize,
}

impl Prompt {
    /// Creates a new prompt with the given left prompt prefix (e.g.
    /// `"> "`).
    ///
    /// Returns the prompt and an [`OutputSender`] that async producers can
    /// use to inject output.
    pub fn new(left_prompt: &str) -> io::Result<(Self, OutputSender)> {
        let (tx, rx) = mpsc::channel();
        let output_sender = OutputSender { tx: tx.clone() };
        let width = term_width();
        Ok((
            Self {
                rx,
                tx,
                stdout: io::stdout(),
                screen: Screen::new(width),
                input_handle: None,
                above_prompt: String::new(),
                left_prompt: left_prompt.to_owned(),
                right_prompt: String::new(),
                buffer: String::new(),
                cursor: 0,
            },
            output_sender,
        ))
    }

    /// Sets the text displayed above the input line. Can contain
    /// newlines for multiple lines. Set to empty to hide.
    pub fn set_above_prompt(&mut self, text: &str) {
        self.above_prompt = text.to_owned();
    }

    /// Sets the left prompt prefix (e.g. `"> "`, `"$ "`).
    pub fn set_left_prompt(&mut self, text: &str) {
        self.left_prompt = text.to_owned();
    }

    /// Sets the right-justified text on the first prompt line. Hidden
    /// when the user input would overlap. Set to empty to hide.
    pub fn set_right_prompt(&mut self, text: &str) {
        self.right_prompt = text.to_owned();
    }

    /// Enters raw mode and starts the input reader thread.
    ///
    /// Must be called before [`read_line`]. Raw mode is exited on
    /// [`Drop`].
    pub fn start(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        self.render()?;

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
                UiEvent::SetAbovePrompt(text) => {
                    self.above_prompt = text;
                    self.render()?;
                }
                UiEvent::SetLeftPrompt(text) => {
                    self.left_prompt = text;
                    self.render()?;
                }
                UiEvent::SetRightPrompt(text) => {
                    self.right_prompt = text;
                    self.render()?;
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
                self.cursor = self.buffer.len();
                self.render()?;
                self.stdout.queue(Print("\r\n"))?.flush()?;
                self.screen.invalidate();
                let line = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                return Ok(Some(PromptResult::Line(line)));
            }

            KeyCode::Char('d') if ctrl => {
                if self.buffer.is_empty() {
                    self.stdout.queue(Print("\r\n"))?.flush()?;
                    self.screen.invalidate();
                    return Ok(Some(PromptResult::Eof));
                }
            }

            KeyCode::Char('c') if ctrl => {
                self.buffer.clear();
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('u') if ctrl => {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('w') if ctrl => {
                if self.cursor > 0 {
                    let before = &self.buffer[..self.cursor];
                    let new_end = before
                        .trim_end()
                        .rfind(' ')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    self.buffer.drain(new_end..self.cursor);
                    self.cursor = new_end;
                    self.render()?;
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                self.render()?;
            }

            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.render()?;
            }

            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.render()?;
                }
            }

            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = next_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(self.cursor..next);
                    self.render()?;
                }
            }

            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.render()?;
                }
            }

            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    self.render()?;
                }
            }

            KeyCode::Home => {
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.render()?;
            }

            _ => {}
        }

        Ok(None)
    }

    /// Prints text above the prompt, then redraws the prompt.
    pub fn print_output(&mut self, text: &str) -> io::Result<()> {
        self.print_above(text)
    }

    fn print_above(&mut self, text: &str) -> io::Result<()> {
        self.screen.erase_all(&mut self.stdout)?;
        for line in text.lines() {
            self.stdout.queue(Print(line))?.queue(Print("\r\n"))?;
        }
        self.stdout.flush()?;
        self.screen.invalidate();
        self.render()
    }

    /// Builds the desired screen content and diffs it against the actual
    /// state.
    ///
    /// Layout (top to bottom):
    /// 1. Above-prompt lines (if non-empty)
    /// 2. Left-prompt + user input (possibly wrapped), with right-prompt
    ///    on the first physical line if it fits
    fn render(&mut self) -> io::Result<()> {
        self.screen.set_width(term_width());
        let width = self.screen.width();

        let mut desired: Vec<Vec<char>> = Vec::new();

        // 1. Above-prompt lines.
        let above_row_count = if self.above_prompt.is_empty() {
            0
        } else {
            for line in self.above_prompt.lines() {
                let laid_out = layout_lines(line, width);
                desired.extend(laid_out);
            }
            desired.len()
        };

        // 2. Input area: left_prompt + buffer.
        let input_content = format!("{}{}", self.left_prompt, self.buffer);
        let mut input_lines = layout_lines(&input_content, width);

        // 3. Right-prompt on the first input line, if it fits.
        if !self.right_prompt.is_empty() && !input_lines.is_empty() {
            let first_line = &input_lines[0];
            let right_chars: Vec<char> = self.right_prompt.chars().collect();
            // Need at least 1 space gap between content and right prompt.
            let needed = first_line.len() + 1 + right_chars.len();
            if needed <= width && input_lines.len() == 1 {
                // Pad with spaces and append right prompt.
                let padding = width - first_line.len() - right_chars.len();
                let mut padded = first_line.clone();
                padded.extend(std::iter::repeat(' ').take(padding));
                padded.extend(right_chars);
                input_lines[0] = padded;
            }
        }

        desired.extend(input_lines);

        // Cursor position: offset by above-prompt rows.
        let left_chars = self.left_prompt.chars().count();
        let cursor_chars = left_chars + char_count_for_bytes(&self.buffer, self.cursor);
        let cursor_row = above_row_count + cursor_chars / width;
        let cursor_col = cursor_chars % width;

        self.screen
            .update(&mut self.stdout, &desired, (cursor_row, cursor_col))
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn term_width() -> usize {
    terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(1)
}

/// Returns the number of `char`s in `s[..byte_pos]`.
fn char_count_for_bytes(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos].chars().count()
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
