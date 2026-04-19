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
//!    internally, updates shared state, and surfaces high-level events.
//!
//! Any thread holding a [`TermHandle`] can mutate prompt zones.
//! After making changes, call [`TermHandle::redraw`] to trigger a
//! repaint. Multiple redraws coalesce into one via the notify channel.
//!
//! # Layout
//!
//! All content blocks are stored in a central map keyed by [`BlockId`].
//! Separate ordered lists reference them for rendering (top to bottom):
//!
//! 1. **History** — persistent output (append-only id list).
//! 2. **Above active** — mutable blocks that can be reordered.
//! 3. **Above sticky** — blocks pinned right above the prompt.
//! 4. **Input area** — left-prompt + user input + right-prompt.
//! 5. **Below** — blocks rendered below the input line.
//!
//! On terminal resize, the entire output (history + live area) is
//! re-laid-out at the new width and re-rendered. The terminal's
//! scrollback is rebuilt with correctly reflowed content.

pub mod screen;
pub mod style;

use std::collections::HashMap;
use std::io::{self, Stdout, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::{QueueableCommand, terminal};
use screen::{Screen, emit_styled_cells, layout_block, layout_lines};
pub use style::{Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText};

/// Mutable state shared between the input loop, redraw thread, and
/// any [`TermHandle`] holders.
struct SharedState {
    /// Central block storage.
    blocks: HashMap<BlockId, StyledBlock>,
    /// Next auto-increment id.
    next_id: u64,

    /// Persistent output — append-only ordered list of block ids.
    history: Vec<BlockId>,
    /// Mutable blocks above the prompt (can be reordered).
    above_active: Vec<BlockId>,
    /// Blocks pinned right above the prompt.
    above_sticky: Vec<BlockId>,
    /// Blocks rendered below the input line.
    below: Vec<BlockId>,

    left_prompt: StyledText,
    right_prompt: StyledText,
    buffer: String,
    cursor: usize,
    width: usize,
    height: usize,
}

impl SharedState {
    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }
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
/// Setters update the shared state but do **not** trigger a redraw.
/// Call [`redraw`](TermHandle::redraw) after making all changes.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    redraw: tau_blocking_notify_channel::Sender,
}

impl TermHandle {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("term state mutex poisoned")
    }

    /// Triggers a redraw of the terminal.
    ///
    /// Call this after updating one or more blocks/zones. Multiple
    /// calls coalesce into a single repaint.
    pub fn redraw(&self) {
        self.redraw.notify();
    }

    // --- Block management ---

    /// Allocates a new [`BlockId`] and stores the block.
    pub fn new_block(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.blocks.insert(id, block.into());
        id
    }

    /// Updates the content of an existing block (or inserts it at
    /// the given id).
    pub fn set_block(&self, id: BlockId, block: impl Into<StyledBlock>) {
        self.lock().blocks.insert(id, block.into());
    }

    /// Removes a block from the central store **and** from every zone
    /// list that references it.
    pub fn remove_block(&self, id: BlockId) {
        let mut st = self.lock();
        st.blocks.remove(&id);
        st.history.retain(|&x| x != id);
        st.above_active.retain(|&x| x != id);
        st.above_sticky.retain(|&x| x != id);
        st.below.retain(|&x| x != id);
    }

    // --- Zone lists ---

    /// Appends a block id to the history (persistent output).
    pub fn push_history(&self, id: BlockId) {
        self.lock().history.push(id);
    }

    /// Appends a block id to the above-active zone (if not already
    /// present).
    pub fn push_above_active(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.above_active.contains(&id) {
            st.above_active.push(id);
        }
    }

    /// Removes a block id from the above-active zone.
    pub fn remove_above_active(&self, id: BlockId) {
        self.lock().above_active.retain(|&x| x != id);
    }

    /// Appends a block id to the above-sticky zone (if not already
    /// present).
    pub fn push_above_sticky(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.above_sticky.contains(&id) {
            st.above_sticky.push(id);
        }
    }

    /// Removes a block id from the above-sticky zone.
    pub fn remove_above_sticky(&self, id: BlockId) {
        self.lock().above_sticky.retain(|&x| x != id);
    }

    /// Appends a block id to the below zone (if not already present).
    pub fn push_below(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.below.contains(&id) {
            st.below.push(id);
        }
    }

    /// Removes a block id from the below zone.
    pub fn remove_below(&self, id: BlockId) {
        self.lock().below.retain(|&x| x != id);
    }

    // --- Convenience ---

    /// Creates a new block and appends it to the history.
    /// Triggers a redraw automatically.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.blocks.insert(id, block.into());
        st.history.push(id);
        drop(st);
        self.redraw.notify();
        id
    }

    /// Updates the left prompt prefix.
    pub fn set_left_prompt(&self, text: impl Into<StyledText>) {
        self.lock().left_prompt = text.into();
    }

    /// Updates the right prompt.
    pub fn set_right_prompt(&self, text: impl Into<StyledText>) {
        self.lock().right_prompt = text.into();
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
    /// Shared mutable state.
    state: Arc<Mutex<SharedState>>,
    /// Notifies the redraw thread that the screen needs updating.
    redraw: tau_blocking_notify_channel::Sender,
    /// Receives raw terminal events from the input reader thread.
    term_input_rx: std::sync::mpsc::Receiver<RawEvent>,
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
        let (width, height) = term_size();
        let state = Arc::new(Mutex::new(SharedState {
            blocks: HashMap::new(),
            next_id: 0,
            history: Vec::new(),
            above_active: Vec::new(),
            above_sticky: Vec::new(),
            below: Vec::new(),
            left_prompt: left_prompt.into(),
            right_prompt: StyledText::new(),
            buffer: String::new(),
            cursor: 0,
            width,
            height,
        }));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();

        terminal::enable_raw_mode()?;

        let redraw_state = Arc::clone(&state);
        let redraw_thread = thread::spawn(move || {
            redraw_loop(redraw_state, redraw_rx);
        });

        let (term_input_tx, term_input_rx) = std::sync::mpsc::channel();
        let term_input_thread = thread::spawn(move || {
            term_input_reader_loop(term_input_tx);
        });

        let handle = TermHandle {
            state: Arc::clone(&state),
            redraw: redraw_tx.clone(),
        };

        redraw_tx.notify();

        Ok((
            Self {
                state,
                redraw: redraw_tx,
                term_input_rx,
                _term_input_thread: term_input_thread,
                _redraw_thread: redraw_thread,
            },
            handle,
        ))
    }

    /// Triggers a redraw of the terminal.
    pub fn redraw(&self) {
        self.redraw.notify();
    }

    /// Blocks until the next meaningful input event.
    ///
    /// Handles key editing internally (insert, delete, cursor movement)
    /// and only surfaces events the downstream cares about. Triggers
    /// a redraw before returning so internal state changes are visible.
    pub fn get_next_event(&self) -> io::Result<Event> {
        loop {
            let raw = match self.term_input_rx.recv() {
                Ok(ev) => ev,
                Err(_) => return Ok(Event::Eof),
            };

            match raw {
                RawEvent::Key(key) => {
                    if let Some(event) = self.handle_key(key)? {
                        self.redraw.notify();
                        return Ok(event);
                    }
                    self.redraw.notify();
                }
                RawEvent::Resize(w, h) => {
                    {
                        let mut st = self.state.lock().expect("term state mutex poisoned");
                        st.width = w as usize;
                        st.height = h as usize;
                    }
                    self.redraw.notify();
                    return Ok(Event::Resize {
                        width: w,
                        height: h,
                    });
                }
            }
        }
    }

    /// Creates a new block and appends it to the history.
    /// Triggers a redraw automatically.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> io::Result<BlockId> {
        let mut st = self.state.lock().expect("term state mutex poisoned");
        let id = st.alloc_id();
        st.blocks.insert(id, block.into());
        st.history.push(id);
        drop(st);
        self.redraw.notify();
        Ok(id)
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
                    return Ok(Some(Event::Eof));
                }
            }

            KeyCode::Char('c') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.buffer.clear();
                    st.cursor = 0;
                }
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Char('u') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    let cursor = st.cursor;
                    st.buffer.drain(..cursor);
                    st.cursor = 0;
                }
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
                    return Ok(Some(Event::BufferChanged));
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.state.lock().expect("term state mutex poisoned").cursor = 0;
            }

            KeyCode::Char('e') if ctrl => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                st.cursor = st.buffer.len();
            }

            KeyCode::Char(ch) => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    let cursor = st.cursor;
                    st.buffer.insert(cursor, ch);
                    st.cursor += ch.len_utf8();
                }
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
                    return Ok(Some(Event::BufferChanged));
                }
            }

            KeyCode::Left => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                if st.cursor > 0 {
                    st.cursor = prev_char_boundary(&st.buffer, st.cursor);
                }
            }

            KeyCode::Right => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                if st.cursor < st.buffer.len() {
                    st.cursor = next_char_boundary(&st.buffer, st.cursor);
                }
            }

            KeyCode::Up => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                if let Some(new_cursor) = move_cursor_vertical(&st, -1) {
                    st.cursor = new_cursor;
                }
            }

            KeyCode::Down => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                if let Some(new_cursor) = move_cursor_vertical(&st, 1) {
                    st.cursor = new_cursor;
                }
            }

            KeyCode::Home => {
                self.state.lock().expect("term state mutex poisoned").cursor = 0;
            }

            KeyCode::End => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                st.cursor = st.buffer.len();
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

// --- Rendering helpers ---

/// Lays out blocks referenced by an id list, skipping missing ids.
fn layout_id_list(
    ids: &[BlockId],
    blocks: &HashMap<BlockId, StyledBlock>,
    width: usize,
    out: &mut Vec<Vec<Cell>>,
) {
    for id in ids {
        if let Some(block) = blocks.get(id) {
            out.extend(layout_block(block, width));
        }
    }
}

/// Result of laying out all content.
struct LayoutAll {
    /// All rendered lines (history + live area).
    all_lines: Vec<Vec<Cell>>,
    /// Index in `all_lines` where the live area starts (after history).
    live_start: usize,
    /// Absolute cursor row in `all_lines`.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

/// Lays out the full content (history + above + input + below).
fn layout_all(st: &SharedState) -> LayoutAll {
    let width = st.width;
    let mut all_lines: Vec<Vec<Cell>> = Vec::new();

    layout_id_list(&st.history, &st.blocks, width, &mut all_lines);
    let live_start = all_lines.len();
    layout_id_list(&st.above_active, &st.blocks, width, &mut all_lines);
    layout_id_list(&st.above_sticky, &st.blocks, width, &mut all_lines);

    let above_end = all_lines.len();

    let mut input_content = st.left_prompt.clone();
    input_content.push(Span::plain(&st.buffer));
    let mut input_lines = layout_lines(&input_content, width);

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

    let left_chars = st.left_prompt.char_count();
    let cursor_chars = left_chars + char_count_for_bytes(&st.buffer, st.cursor);
    let cursor_row = above_end + cursor_chars / width;
    let cursor_col = cursor_chars % width;

    all_lines.extend(input_lines);
    layout_id_list(&st.below, &st.blocks, width, &mut all_lines);

    LayoutAll {
        all_lines,
        live_start,
        cursor_row,
        cursor_col,
    }
}

/// Renders the live area (above_active + above_sticky + input + below).
fn render_live(stdout: &mut impl Write, screen: &mut Screen, st: &SharedState) -> io::Result<()> {
    let width = screen.width();
    let mut desired: Vec<Vec<Cell>> = Vec::new();

    layout_id_list(&st.above_active, &st.blocks, width, &mut desired);
    layout_id_list(&st.above_sticky, &st.blocks, width, &mut desired);
    let above_row_count = desired.len();

    let mut input_content = st.left_prompt.clone();
    input_content.push(Span::plain(&st.buffer));
    let mut input_lines = layout_lines(&input_content, width);

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

    let left_chars = st.left_prompt.char_count();
    let cursor_chars = left_chars + char_count_for_bytes(&st.buffer, st.cursor);
    let cursor_row = above_row_count + cursor_chars / width;
    let cursor_col = cursor_chars % width;

    layout_id_list(&st.below, &st.blocks, width, &mut desired);

    screen.update(stdout, &desired, (cursor_row, cursor_col))
}

// --- Redraw thread ---

fn redraw_loop(state: Arc<Mutex<SharedState>>, notify_rx: tau_blocking_notify_channel::Receiver) {
    let mut stdout = io::stdout();
    let (w, h) = {
        let st = state.lock().expect("term state mutex poisoned");
        (st.width, st.height)
    };
    let mut screen = Screen::new(w);
    let mut prev_width = w;
    let mut prev_height = h;
    let mut rendered_history_len: usize = 0;

    loop {
        if notify_rx.recv().is_err() {
            break;
        }

        let st = state.lock().expect("term state mutex poisoned");
        let width = st.width;
        let height = st.height.max(1);
        let width_changed = prev_width != width;
        let height_changed = prev_height != height;
        let new_history = st.history.len() > rendered_history_len;

        if width_changed || height_changed {
            let layout = layout_all(&st);
            rendered_history_len = st.history.len();
            drop(st);
            if let Err(e) = full_render(&mut stdout, &mut screen, &layout, width, height) {
                eprintln!("redraw: full render error: {e}");
            }
        } else if new_history {
            // Lay out only the new history blocks, print them, then
            // re-render the live area.
            let new_ids: Vec<BlockId> = st.history[rendered_history_len..].to_vec();
            let mut new_lines: Vec<Vec<Cell>> = Vec::new();
            layout_id_list(&new_ids, &st.blocks, width, &mut new_lines);
            rendered_history_len = st.history.len();
            drop(st);

            if let Err(e) = print_new_history(&mut stdout, &mut screen, &new_lines) {
                eprintln!("redraw: history output error: {e}");
            }

            let st = state.lock().expect("term state mutex poisoned");
            screen.set_width(width);
            if let Err(e) = render_live(&mut stdout, &mut screen, &st) {
                eprintln!("redraw: render error: {e}");
            }
        } else {
            screen.set_width(width);
            if let Err(e) = render_live(&mut stdout, &mut screen, &st) {
                eprintln!("redraw: render error: {e}");
            }
            drop(st);
        }

        prev_width = width;
        prev_height = height;
    }
}

/// Full re-render: clear screen + scrollback, output all lines,
/// position cursor. Used on resize.
///
/// After rendering, Screen is set to track **only the live area**
/// (the portion after history). This matches what `render_live`
/// produces, so subsequent differential updates work correctly.
fn full_render(
    stdout: &mut impl Write,
    screen: &mut Screen,
    layout: &LayoutAll,
    width: usize,
    height: usize,
) -> io::Result<()> {
    screen.set_width(width);

    let all_lines = &layout.all_lines;
    let total = all_lines.len();

    stdout.queue(terminal::BeginSynchronizedUpdate)?;
    // Clear screen + scrollback and home cursor.
    stdout.queue(Print("\x1b[2J\x1b[H\x1b[3J"))?;

    // Output all lines starting at the top. Overflow scrolls into
    // scrollback naturally.
    for (i, line) in all_lines.iter().enumerate() {
        if i > 0 {
            stdout.queue(Print("\r\n"))?;
        }
        emit_styled_cells(stdout, line)?;
    }

    stdout.queue(terminal::EndSynchronizedUpdate)?;
    stdout.flush()?;

    // Position the cursor within the live area.
    let live_lines = &all_lines[layout.live_start..];
    let cursor_in_live = layout.cursor_row.saturating_sub(layout.live_start);

    // After outputting, the cursor is at the last content line.
    // When total >= height, overflow scrolled and the cursor is at
    // screen row (height - 1). When total < height, the cursor is
    // at screen row (total - 1).
    let current_screen_row = if total >= height {
        height - 1
    } else {
        total.saturating_sub(1)
    };

    // The live area starts at this screen row:
    let viewport_top = total.saturating_sub(height);
    let live_screen_start = layout.live_start.saturating_sub(viewport_top);
    let cursor_screen_row = live_screen_start + cursor_in_live;

    let up = current_screen_row.saturating_sub(cursor_screen_row);
    if up > 0 {
        stdout.queue(MoveUp(up as u16))?;
    }
    stdout.queue(MoveToColumn(layout.cursor_col as u16))?;
    stdout.flush()?;

    // Set Screen to track only the live area. This matches what
    // render_live produces, so the next differential update diffs
    // correctly.
    screen.reset_to(live_lines.to_vec(), cursor_in_live, layout.cursor_col);

    Ok(())
}

/// Prints new history lines above the live area, then invalidates
/// the screen so the next render_live redraws from scratch.
fn print_new_history(
    stdout: &mut Stdout,
    screen: &mut Screen,
    lines: &[Vec<Cell>],
) -> io::Result<()> {
    screen.erase_all(stdout)?;
    for line in lines {
        emit_styled_cells(stdout, line)?;
        stdout.queue(Print("\r\n"))?;
    }
    stdout.flush()?;
    screen.invalidate();
    Ok(())
}

// --- Input reader thread ---

fn term_input_reader_loop(tx: std::sync::mpsc::Sender<RawEvent>) {
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

fn term_size() -> (usize, usize) {
    terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((80, 24))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: builds Cell lines from plain strings.
    fn plain_lines(texts: &[&str]) -> Vec<Vec<Cell>> {
        texts
            .iter()
            .map(|s| s.chars().map(Cell::plain).collect())
            .collect()
    }

    /// Helper: runs full_render into a vt100 parser and returns it.
    ///
    /// `history_lines` is the number of lines at the top of
    /// `all_lines` that belong to history (before the live area).
    fn run_full_render(
        rows: u16,
        cols: u16,
        all_lines: Vec<Vec<Cell>>,
        history_lines: usize,
        cursor_row: usize,
        cursor_col: usize,
    ) -> (vt100::Parser, Screen) {
        let mut term = vt100::Parser::new(rows, cols, 200);
        let mut screen = Screen::new(cols as usize);
        let mut buf: Vec<u8> = Vec::new();

        let layout = LayoutAll {
            all_lines,
            live_start: history_lines,
            cursor_row,
            cursor_col,
        };

        full_render(&mut buf, &mut screen, &layout, cols as usize, rows as usize)
            .expect("full_render should succeed");

        term.process(&buf);
        (term, screen)
    }

    /// Helper: visible rows as trimmed strings.
    fn visible_rows(term: &vt100::Parser) -> Vec<String> {
        let (_, cols) = term.screen().size();
        term.screen().rows(0, cols).collect()
    }

    // --- full_render: content overflows terminal height ---

    #[test]
    fn full_render_overflow_visible_and_scrollback() {
        // 3 history lines + 4 live lines = 7 total, 5-row terminal.
        let lines = plain_lines(&[
            "history 0",
            "history 1",
            "history 2",
            "above A",
            "above B",
            "> hello",
            "below",
        ]);
        let (mut term, _screen) = run_full_render(5, 30, lines, 3, 5, 7);

        // Visible: last 5 lines (indices 2..7).
        let vis = visible_rows(&term);
        assert_eq!(vis[0], "history 2");
        assert_eq!(vis[1], "above A");
        assert_eq!(vis[2], "above B");
        assert_eq!(vis[3], "> hello");
        assert_eq!(vis[4], "below");

        // Scrollback: indices 0..2.
        term.screen_mut().set_scrollback(2);
        let sb = visible_rows(&term);
        assert_eq!(sb[0], "history 0");
        assert_eq!(sb[1], "history 1");
    }

    #[test]
    fn full_render_overflow_cursor_and_screen_state() {
        // 3 history + 4 live = 7, 5-row terminal.
        let lines = plain_lines(&[
            "history 0",
            "history 1",
            "history 2",
            "above A",
            "above B",
            "> hello",
            "below",
        ]);
        let (term, screen) = run_full_render(5, 30, lines, 3, 5, 7);

        // Terminal cursor: row 5 is "> hello", viewport_top=2,
        // live_start=3, cursor_in_live=2 → screen row = 3.
        let (r, c) = term.screen().cursor_position();
        assert_eq!(r, 3, "cursor row in viewport");
        assert_eq!(c, 7, "cursor col");

        // Screen tracks only the live area (4 lines), not the
        // full viewport.
        assert_eq!(
            screen.actual_line_count(),
            4,
            "screen tracks live area only"
        );
    }

    // --- full_render: content shorter than terminal ---

    #[test]
    fn full_render_short_content_at_top() {
        // 0 history + 3 live = 3, 10-row terminal.
        // Content starts at the top (no blank padding).
        let lines = plain_lines(&["above", "> hi", "below"]);
        let (term, _screen) = run_full_render(10, 30, lines, 0, 1, 4);

        let vis = visible_rows(&term);
        assert_eq!(vis[0], "above");
        assert_eq!(vis[1], "> hi");
        assert_eq!(vis[2], "below");
        // Rest is empty.
        for i in 3..10 {
            assert_eq!(vis[i], "", "row {i} should be blank");
        }
    }

    #[test]
    fn full_render_short_content_cursor() {
        // 0 history + 3 live = 3, 10-row terminal.
        let lines = plain_lines(&["above", "> hi", "below"]);
        let (term, screen) = run_full_render(10, 30, lines, 0, 1, 4);

        // Content starts at the top. cursor_row=1 → screen row 1.
        let (r, c) = term.screen().cursor_position();
        assert_eq!(r, 1, "cursor row");
        assert_eq!(c, 4, "cursor col");

        // Screen tracks only the live area (3 lines).
        assert_eq!(
            screen.actual_line_count(),
            3,
            "screen tracks live area only"
        );
    }

    // --- full_render: exact fit ---

    #[test]
    fn full_render_exact_fit() {
        // 2 history + 3 live = 5, 5-row terminal.
        let lines = plain_lines(&["hist 0", "hist 1", "> cmd", "status A", "status B"]);
        let (term, screen) = run_full_render(5, 30, lines, 2, 2, 5);

        let vis = visible_rows(&term);
        assert_eq!(vis[0], "hist 0");
        assert_eq!(vis[4], "status B");

        // cursor_row=2, live_start=2, cursor_in_live=0.
        // Screen row = 0 (padding) + 2 (live_start) + 0 = 2.
        // Wait — viewport_top = 0 for exact fit, live_screen_start = 0 + 2 = 2.
        let (r, c) = term.screen().cursor_position();
        assert_eq!(r, 2, "cursor row");
        assert_eq!(c, 5, "cursor col");

        // Screen tracks the live area (3 lines).
        assert_eq!(screen.actual_line_count(), 3);
    }

    // --- full_render: Screen state allows correct subsequent diffs ---

    #[test]
    fn full_render_then_diff_render() {
        // After full_render, Screen tracks the live area.
        // A subsequent Screen::update (as render_live does) should
        // diff only against the live area.
        //
        // 0 history + 3 live = 3, 10-row terminal.
        let lines = plain_lines(&["above", "> hello", "below"]);
        let (_term, mut screen) = run_full_render(10, 30, lines, 0, 1, 7);

        // Screen should track 3 lines (the live area).
        assert_eq!(screen.actual_line_count(), 3);

        // Diff update: change "> hello" to "> world".
        let live_lines2 = plain_lines(&["above", "> world", "below"]);
        let mut buf2: Vec<u8> = Vec::new();
        screen
            .update(&mut buf2, &live_lines2, (1, 7))
            .expect("update should succeed");

        assert!(!buf2.is_empty(), "diff should produce output");
    }

    #[test]
    fn full_render_then_diff_with_history() {
        // 3 history + 2 live = 5, 5-row terminal.
        // After full_render, Screen tracks only the 2 live lines.
        // A diff update with 2 lines should work without touching
        // the history rows.
        let lines = plain_lines(&["h0", "h1", "h2", "> cmd", "status"]);
        let (_term, mut screen) = run_full_render(5, 30, lines, 3, 3, 5);

        assert_eq!(screen.actual_line_count(), 2, "only live area tracked");

        // Update live area: change "> cmd" to "> new".
        let live2 = plain_lines(&["> new", "status"]);
        let mut buf2: Vec<u8> = Vec::new();
        screen.update(&mut buf2, &live2, (0, 5)).expect("ok");

        assert!(!buf2.is_empty());
    }
}
