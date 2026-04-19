//! Terminal prompt with async output support.
//!
//! Provides a line-editing prompt that can be interrupted by async output
//! without corrupting the display. No alternate screen mode — just
//! diff-based rendering in the normal terminal buffer.
//!
//! # Architecture
//!
//! Three threads cooperate:
//!
//! 1. **Input reader** (internal) — reads terminal events from crossterm and
//!    forwards them to the downstream event loop.
//! 2. **Redraw** (internal) — blocked on a coalescing notify channel; wakes up,
//!    reads current shared state, and diff-renders to stdout.
//! 3. **Downstream event loop** — the caller's thread. Calls
//!    [`Term::get_next_event`] which blocks on input, handles editing
//!    internally, updates shared state, notifies redraw, and surfaces
//!    high-level events.
//!
//! Any thread holding a [`TermHandle`] can mutate prompt zones and
//! trigger a redraw without coordinating with the input loop.
//!
//! # Prompt zones
//!
//! The prompt display is composed of configurable zones (top to bottom):
//!
//! - **Above-prompt**: Optional multi-line text displayed above the input line
//!   (e.g. status information, context).
//! - **Left-prompt**: The prefix before user input (e.g. `"> "`).
//! - **Right-prompt**: Right-justified text on the first physical line of the
//!   input area. Hidden when the user input would overlap it.
//! - **Below-prompt**: Optional multi-line text displayed below the input line
//!   (e.g. completions, status).
//!
//! All zones accept [`StyledText`] (or anything that converts to it,
//! including plain `&str` and `String`), so content can carry colors,
//! bold, underline, etc.
//!
//! # Rendering
//!
//! Uses a dual-buffer diff approach inspired by fish shell's `screen.rs`:
//! the [`screen::Screen`] tracks what is currently on the terminal
//! ("actual") and diffs it against what should be there ("desired"),
//! emitting only the escape sequences for characters that changed.

pub mod screen;
pub mod style;

use std::io::{self, Stdout, Write};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::{QueueableCommand, terminal};
use screen::{Screen, emit_styled_cells, layout_lines};
pub use style::{Cell, Color, Span, Style, StyledText};

/// Mutable state shared between the input loop, redraw thread, and
/// any [`TermHandle`] holders.
struct SharedState {
    above_prompt: StyledText,
    left_prompt: StyledText,
    right_prompt: StyledText,
    below_prompt: StyledText,
    buffer: String,
    cursor: usize,
    width: usize,
}

/// High-level events surfaced to the downstream event loop.
pub enum Event {
    /// The user submitted a line (pressed Enter).
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The input buffer changed (character inserted/deleted/cleared).
    BufferChanged,
}

/// A cloneable handle for mutating prompt zones from any thread.
///
/// Each setter updates the shared state and notifies the redraw thread.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    redraw: tau_blocking_notify_channel::Sender,
    term_output_tx: mpsc::Sender<StyledText>,
}

impl TermHandle {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("term state mutex poisoned")
    }

    /// Updates the above-prompt zone.
    pub fn set_above_prompt(&self, text: impl Into<StyledText>) {
        self.lock().above_prompt = text.into();
        self.redraw.notify();
    }

    /// Updates the left prompt prefix.
    pub fn set_left_prompt(&self, text: impl Into<StyledText>) {
        self.lock().left_prompt = text.into();
        self.redraw.notify();
    }

    /// Updates the right prompt.
    pub fn set_right_prompt(&self, text: impl Into<StyledText>) {
        self.lock().right_prompt = text.into();
        self.redraw.notify();
    }

    /// Updates the below-prompt zone.
    pub fn set_below_prompt(&self, text: impl Into<StyledText>) {
        self.lock().below_prompt = text.into();
        self.redraw.notify();
    }

    /// Prints styled text as persistent output above the prompt.
    ///
    /// This content is written once and scrolled up — it is not part
    /// of the diff-rendered zones.
    pub fn print_output(&self, text: impl Into<StyledText>) {
        let _ = self.term_output_tx.send(text.into());
        self.redraw.notify();
    }
}

/// Raw terminal events from the crossterm reader thread.
enum RawEvent {
    Key(KeyEvent),
    Resize(u16, u16),
}

/// The terminal prompt engine.
///
/// Owns the input event loop. Call [`Term::get_next_event`] in a loop to
/// drive it.
pub struct Term {
    /// Shared mutable state (zones, buffer, cursor, width).
    state: Arc<Mutex<SharedState>>,
    /// Notifies the redraw thread that the screen needs updating.
    redraw: tau_blocking_notify_channel::Sender,
    /// Sends persistent output to the redraw thread for direct rendering.
    term_output_tx: mpsc::Sender<StyledText>,
    /// Receives raw terminal events from the input reader thread.
    term_input_rx: mpsc::Receiver<RawEvent>,
    /// Input reader thread handle (kept alive for the lifetime of Term).
    _term_input_thread: JoinHandle<()>,
    /// Redraw thread handle (kept alive for the lifetime of Term).
    _redraw_thread: JoinHandle<()>,
}

impl Term {
    /// Creates a new terminal prompt.
    ///
    /// Enters raw mode, spawns the input reader and redraw threads.
    /// Returns the prompt engine and a cloneable [`TermHandle`].
    pub fn new(left_prompt: impl Into<StyledText>) -> io::Result<(Self, TermHandle)> {
        let width = term_width();
        let state = Arc::new(Mutex::new(SharedState {
            above_prompt: StyledText::new(),
            left_prompt: left_prompt.into(),
            right_prompt: StyledText::new(),
            below_prompt: StyledText::new(),
            buffer: String::new(),
            cursor: 0,
            width,
        }));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();
        let (term_output_tx, term_output_rx) = mpsc::channel();

        terminal::enable_raw_mode()?;

        // Spawn redraw thread.
        let redraw_state = Arc::clone(&state);
        let redraw_thread = thread::spawn(move || {
            redraw_loop(redraw_state, redraw_rx, term_output_rx);
        });

        // Spawn input reader thread.
        let (term_input_tx, term_input_rx) = mpsc::channel();
        let term_input_thread = thread::spawn(move || {
            term_input_reader_loop(term_input_tx);
        });

        let handle = TermHandle {
            state: Arc::clone(&state),
            redraw: redraw_tx.clone(),
            term_output_tx: term_output_tx.clone(),
        };

        // Trigger initial render.
        redraw_tx.notify();

        Ok((
            Self {
                state,
                redraw: redraw_tx,
                term_output_tx,
                term_input_rx,
                _term_input_thread: term_input_thread,
                _redraw_thread: redraw_thread,
            },
            handle,
        ))
    }

    /// Blocks until the next meaningful input event.
    ///
    /// Handles key editing internally (insert, delete, cursor movement)
    /// and only surfaces events the downstream cares about.
    pub fn get_next_event(&self) -> io::Result<Event> {
        loop {
            let raw = match self.term_input_rx.recv() {
                Ok(ev) => ev,
                Err(_) => return Ok(Event::Eof),
            };

            match raw {
                RawEvent::Key(key) => {
                    if let Some(event) = self.handle_key(key)? {
                        return Ok(event);
                    }
                }
                RawEvent::Resize(w, h) => {
                    self.state.lock().expect("term state mutex poisoned").width = w as usize;
                    self.redraw.notify();
                    return Ok(Event::Resize {
                        width: w,
                        height: h,
                    });
                }
            }
        }
    }

    /// Prints styled text as persistent output above the prompt.
    ///
    /// This content is written once and scrolled up — it is not part
    /// of the diff-rendered zones.
    pub fn print_output(&self, text: impl Into<StyledText>) -> io::Result<()> {
        let _ = self.term_output_tx.send(text.into());
        self.redraw.notify();
        Ok(())
    }

    fn handle_key(&self, key: KeyEvent) -> io::Result<Option<Event>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Enter => {
                let line = {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.cursor = st.buffer.len();
                    let line = std::mem::take(&mut st.buffer);
                    st.cursor = 0;
                    line
                };
                self.redraw.notify();
                // Send an empty persistent-output to force a newline
                // after the prompt line before continuing.
                let _ = self.term_output_tx.send(StyledText::new());
                self.redraw.notify();
                return Ok(Some(Event::Line(line)));
            }

            KeyCode::Char('d') if ctrl => {
                let is_empty = self
                    .state
                    .lock()
                    .expect("term state mutex poisoned")
                    .buffer
                    .is_empty();
                if is_empty {
                    let _ = self.term_output_tx.send(StyledText::new());
                    self.redraw.notify();
                    return Ok(Some(Event::Eof));
                }
            }

            KeyCode::Char('c') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.buffer.clear();
                    st.cursor = 0;
                }
                self.redraw.notify();
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Char('u') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    let cursor = st.cursor;
                    st.buffer.drain(..cursor);
                    st.cursor = 0;
                }
                self.redraw.notify();
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Char('w') if ctrl => {
                let changed = {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if st.cursor > 0 {
                        let new_end = st.buffer[..st.cursor]
                            .trim_end()
                            .rfind(' ')
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        let cursor = st.cursor;
                        st.buffer.drain(new_end..cursor);
                        st.cursor = new_end;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.redraw.notify();
                    return Ok(Some(Event::BufferChanged));
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.state.lock().expect("term state mutex poisoned").cursor = 0;
                self.redraw.notify();
            }

            KeyCode::Char('e') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.cursor = st.buffer.len();
                }
                self.redraw.notify();
            }

            KeyCode::Char(ch) => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    let cursor = st.cursor;
                    st.buffer.insert(cursor, ch);
                    st.cursor += ch.len_utf8();
                }
                self.redraw.notify();
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Backspace => {
                let changed = {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if st.cursor > 0 {
                        let prev = prev_char_boundary(&st.buffer, st.cursor);
                        let cursor = st.cursor;
                        st.buffer.drain(prev..cursor);
                        st.cursor = prev;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.redraw.notify();
                    return Ok(Some(Event::BufferChanged));
                }
            }

            KeyCode::Delete => {
                let changed = {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if st.cursor < st.buffer.len() {
                        let next = next_char_boundary(&st.buffer, st.cursor);
                        let cursor = st.cursor;
                        st.buffer.drain(cursor..next);
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.redraw.notify();
                    return Ok(Some(Event::BufferChanged));
                }
            }

            KeyCode::Left => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if st.cursor > 0 {
                        st.cursor = prev_char_boundary(&st.buffer, st.cursor);
                    }
                }
                self.redraw.notify();
            }

            KeyCode::Right => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if st.cursor < st.buffer.len() {
                        st.cursor = next_char_boundary(&st.buffer, st.cursor);
                    }
                }
                self.redraw.notify();
            }

            KeyCode::Up => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if let Some(new_cursor) = move_cursor_vertical(&st, -1) {
                        st.cursor = new_cursor;
                    }
                }
                self.redraw.notify();
            }

            KeyCode::Down => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    if let Some(new_cursor) = move_cursor_vertical(&st, 1) {
                        st.cursor = new_cursor;
                    }
                }
                self.redraw.notify();
            }

            KeyCode::Home => {
                self.state.lock().expect("term state mutex poisoned").cursor = 0;
                self.redraw.notify();
            }

            KeyCode::End => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.cursor = st.buffer.len();
                }
                self.redraw.notify();
            }

            _ => {}
        }

        Ok(None)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

// --- Redraw thread ---

fn redraw_loop(
    state: Arc<Mutex<SharedState>>,
    notify_rx: tau_blocking_notify_channel::Receiver,
    term_output_rx: mpsc::Receiver<StyledText>,
) {
    let mut stdout = io::stdout();
    let mut screen = {
        let st = state.lock().expect("term state mutex poisoned");
        Screen::new(st.width)
    };

    loop {
        if notify_rx.recv().is_err() {
            break;
        }

        // Drain any persistent output first.
        while let Ok(text) = term_output_rx.try_recv() {
            if let Err(e) = print_persistent(&mut stdout, &mut screen, &text) {
                eprintln!("redraw: persistent output error: {e}");
            }
        }

        // Read current state and render.
        let st = state.lock().expect("term state mutex poisoned");
        screen.set_width(st.width);
        if let Err(e) = render(&mut stdout, &mut screen, &st) {
            eprintln!("redraw: render error: {e}");
        }
    }
}

/// Builds the desired screen content from shared state and diffs it.
fn render(stdout: &mut Stdout, screen: &mut Screen, st: &SharedState) -> io::Result<()> {
    let width = screen.width();
    let mut desired: Vec<Vec<Cell>> = Vec::new();

    // 1. Above-prompt lines.
    let above_row_count = if st.above_prompt.is_empty() {
        0
    } else {
        desired.extend(layout_lines(&st.above_prompt, width));
        desired.len()
    };

    // 2. Input area: left_prompt + buffer.
    let mut input_content = st.left_prompt.clone();
    input_content.push(Span::plain(&st.buffer));
    let mut input_lines = layout_lines(&input_content, width);

    // 3. Right-prompt on the first input line, if it fits.
    if !st.right_prompt.is_empty() && !input_lines.is_empty() {
        let first_line = &input_lines[0];
        let right_cells = st.right_prompt.to_cells();
        let needed = first_line.len() + 1 + right_cells.len();
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_line.len() - right_cells.len();
            let mut padded = first_line.clone();
            padded.extend(std::iter::repeat(Cell::plain(' ')).take(padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    desired.extend(input_lines);

    // Cursor position: offset by above-prompt rows.
    let left_chars = st.left_prompt.char_count();
    let cursor_chars = left_chars + char_count_for_bytes(&st.buffer, st.cursor);
    let cursor_row = above_row_count + cursor_chars / width;
    let cursor_col = cursor_chars % width;

    // 4. Below-prompt lines.
    if !st.below_prompt.is_empty() {
        desired.extend(layout_lines(&st.below_prompt, width));
    }

    screen.update(stdout, &desired, (cursor_row, cursor_col))
}

/// Prints persistent output above the prompt area, then invalidates.
fn print_persistent(stdout: &mut Stdout, screen: &mut Screen, text: &StyledText) -> io::Result<()> {
    screen.erase_all(stdout)?;
    // Empty text is used as a "newline" signal (e.g. after Enter).
    if text.is_empty() {
        stdout.queue(Print("\r\n"))?;
    } else {
        let width = screen.width();
        let lines = layout_lines(text, width);
        for line in &lines {
            emit_styled_cells(stdout, line)?;
            stdout.queue(Print("\r\n"))?;
        }
    }
    stdout.flush()?;
    screen.invalidate();
    Ok(())
}

// --- Input reader thread ---

fn term_input_reader_loop(tx: mpsc::Sender<RawEvent>) {
    loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => break,
        };
        let raw = match ev {
            CtEvent::Key(key) => RawEvent::Key(key),
            CtEvent::Resize(w, h) => RawEvent::Resize(w, h),
            _ => continue,
        };
        if tx.send(raw).is_err() {
            break;
        }
    }
}

// --- Helpers ---

/// Computes a new buffer byte offset after moving the cursor up or down
/// by `delta` physical rows.
fn move_cursor_vertical(st: &SharedState, delta: isize) -> Option<usize> {
    let width = st.width;
    let left_chars = st.left_prompt.char_count();
    let cursor_chars = left_chars + char_count_for_bytes(&st.buffer, st.cursor);
    let current_row = cursor_chars / width;
    let current_col = cursor_chars % width;

    let target_row = current_row as isize + delta;
    if target_row < 0 {
        return None;
    }
    let target_row = target_row as usize;

    let total_chars = left_chars + st.buffer.chars().count();
    let max_row = if total_chars == 0 {
        0
    } else {
        (total_chars.saturating_sub(1)) / width
    };
    if target_row > max_row {
        return None;
    }

    let target_offset = (target_row * width + current_col).min(total_chars);
    let target_buffer_chars = target_offset.saturating_sub(left_chars);
    let new_cursor = char_offset_to_byte(&st.buffer, target_buffer_chars);
    Some(new_cursor)
}

fn term_width() -> usize {
    terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(1)
}

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

fn char_offset_to_byte(s: &str, n: usize) -> usize {
    s.char_indices()
        .nth(n)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}
