//! Terminal prompt with async output support.
//!
//! Provides a line-editing prompt that can be interrupted by async output
//! without corrupting the display. No alternate screen mode — just
//! erase/print/redraw in the normal terminal buffer.
//!
//! # Multi-line wrapping
//!
//! When the prompt + input exceeds the terminal width, the terminal wraps
//! it onto multiple physical lines. To erase everything before printing
//! async output, we:
//!
//! 1. Calculate which physical row the cursor is on:
//!    `cursor_row = (prefix_len + cursor_pos) / terminal_width`
//! 2. Move up to row 0 with `CursorUp(cursor_row)`
//! 3. Clear from there down with `ClearFromCursorDown` (one-shot wipe)
//! 4. Print async output, then redraw prompt + buffer
//!
//! This approach is borrowed from fish shell's `screen.rs`, which uses
//! relative cursor movement + clear-to-end-of-screen rather than
//! absolute positioning. Fish goes further with a dual-buffer diff
//! system (tracking "desired" vs "actual" screen state to emit minimal
//! escape sequences), but for our simpler case the erase-and-redraw
//! approach is sufficient.
//!
//! References:
//! - fish shell screen rendering: <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//! - fish `Screen::update()` for the diff-based repaint logic
//! - fish `reset_line()` / `actual_lines_before_reset` for the clear strategy

use std::io::{self, Stdout, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crossterm::cursor::{MoveToColumn, MoveUp};
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
                // Move to end of content, then newline.
                self.move_cursor_to_content_end()?;
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
                self.buffer.clear();
                self.cursor = 0;
                self.redraw()?;
            }

            KeyCode::Char('u') if ctrl => {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                self.redraw()?;
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
                    self.redraw()?;
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                self.place_cursor()?;
            }

            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                self.place_cursor()?;
            }

            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.redraw()?;
            }

            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.redraw()?;
                }
            }

            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = next_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(self.cursor..next);
                    self.redraw()?;
                }
            }

            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.place_cursor()?;
                }
            }

            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    self.place_cursor()?;
                }
            }

            KeyCode::Home => {
                self.cursor = 0;
                self.place_cursor()?;
            }

            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.place_cursor()?;
            }

            _ => {}
        }

        Ok(None)
    }

    /// Prints text above the prompt, then redraws the prompt line.
    pub fn print_output(&mut self, text: &str) -> io::Result<()> {
        self.print_above(text)
    }

    /// Erases the entire prompt (including wrapped lines), prints text,
    /// then redraws the prompt.
    fn print_above(&mut self, text: &str) -> io::Result<()> {
        self.erase_prompt()?;
        for line in text.lines() {
            self.stdout.queue(Print(line))?.queue(Print("\r\n"))?;
        }
        self.draw_prompt_queued()?;
        self.stdout.flush()
    }

    /// Moves cursor to line 0, column 0 of the prompt area and clears
    /// everything from there down.
    fn erase_prompt(&mut self) -> io::Result<()> {
        let width = self.term_width();
        let cursor_offset = self.prefix.len() + self.cursor;

        // Which physical row is the cursor on (0-based from prompt start)?
        let cursor_row = cursor_offset / width;
        // Move up from cursor_row to row 0.
        if cursor_row > 0 {
            self.stdout.queue(MoveUp(cursor_row as u16))?;
        }
        self.stdout
            .queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::FromCursorDown))?;
        Ok(())
    }

    /// Full redraw: erase everything, then draw prompt + buffer + place
    /// cursor.
    fn redraw(&mut self) -> io::Result<()> {
        self.erase_prompt()?;
        self.draw_prompt_queued()?;
        self.stdout.flush()
    }

    /// Draws the prompt fresh (assumes cursor is at the right position to
    /// start writing). Used after erase or at startup.
    fn draw_prompt(&mut self) -> io::Result<()> {
        self.draw_prompt_queued()?;
        self.stdout.flush()
    }

    fn draw_prompt_queued(&mut self) -> io::Result<()> {
        let width = self.term_width();
        let cursor_offset = self.prefix.len() + self.cursor;
        let cursor_row = cursor_offset / width;
        let cursor_col = cursor_offset % width;

        // Print the full prompt + buffer. The terminal will wrap naturally.
        self.stdout
            .queue(Print(&self.prefix))?
            .queue(Print(&self.buffer))?;

        // Now position the cursor. After printing, the terminal cursor is
        // at the end of the content. We need to move it to (cursor_row,
        // cursor_col) relative to the start of the prompt.
        let total_chars = self.prefix.len() + self.buffer.len();
        let end_row = if total_chars == 0 {
            0
        } else {
            (total_chars.saturating_sub(1)) / width
        };

        // Move up from end to cursor row.
        let rows_up = end_row.saturating_sub(cursor_row);
        if rows_up > 0 {
            self.stdout.queue(MoveUp(rows_up as u16))?;
        }
        self.stdout.queue(MoveToColumn(cursor_col as u16))?;
        Ok(())
    }

    /// Just repositions the cursor without redrawing content (for arrow
    /// keys, home/end).
    fn place_cursor(&mut self) -> io::Result<()> {
        // We need to know where the cursor currently is on screen
        // vs where it should be. Simplest approach: full redraw.
        // For arrow keys this is fine — the content hasn't changed,
        // and the terminal just needs to reposition.
        self.redraw()
    }

    /// Moves the terminal cursor to the end of the prompt content.
    /// Used before printing a newline (Enter, Ctrl-C) so the newline
    /// appears after the last character.
    fn move_cursor_to_content_end(&mut self) -> io::Result<()> {
        let width = self.term_width();
        let cursor_offset = self.prefix.len() + self.cursor;
        let total_chars = self.prefix.len() + self.buffer.len();

        let cursor_row = cursor_offset / width;
        let end_row = if total_chars == 0 {
            0
        } else {
            (total_chars.saturating_sub(1)) / width
        };
        let end_col = if total_chars == 0 {
            0
        } else {
            (total_chars.saturating_sub(1)) % width + 1
        };

        // Move down from cursor to end.
        let rows_down = end_row.saturating_sub(cursor_row);
        if rows_down > 0 {
            for _ in 0..rows_down {
                self.stdout.queue(Print("\n"))?;
            }
        }
        self.stdout.queue(MoveToColumn(end_col as u16))?.flush()
    }

    fn term_width(&self) -> usize {
        terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
            .max(1)
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
