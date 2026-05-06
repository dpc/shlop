//! Terminal prompt with async output support.
//!
//! Renders directly to the normal terminal buffer (no alternate screen)
//! so the terminal's native scrollback is preserved. See `README.md`
//! in this crate for the full rendering strategy.
//!
//! Three rendering paths (see `README.md`):
//! - **Differential update** — common case, diffs visible viewport via
//!   [`Screen`]
//! - **Scrolling render** — on overflow, diffs full content and renders in
//!   order; `\r\n` at the bottom pushes content into scrollback
//! - **Full render** — on resize, clears screen and re-renders everything

pub mod screen;
pub mod style;

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::{QueueableCommand, terminal};
use screen::{Screen, emit_styled_cells, layout_block, layout_lines};
pub use style::{Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShape {
    Bar,
    Block,
}

impl CursorShape {
    fn crossterm_style(self) -> crossterm::cursor::SetCursorStyle {
        match self {
            Self::Bar => crossterm::cursor::SetCursorStyle::SteadyBar,
            Self::Block => crossterm::cursor::SetCursorStyle::SteadyBlock,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyDirection {
    Up,
    Down,
}

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
    /// Blocks rendered immediately below the input line (e.g.
    /// completion menus). Sits between the prompt and `below`.
    suggestions: Vec<BlockId>,
    /// Blocks rendered below suggestions.
    below: Vec<BlockId>,

    left_prompt: StyledText,
    right_prompt: StyledText,
    buffer: String,
    cursor: usize,
    input_history: Vec<String>,
    history_nav_entries: Vec<String>,
    history_nav_index: Option<usize>,
    history_nav_active: bool,
    last_key_direction: Option<KeyDirection>,
    last_key_started_history_nav: bool,
    last_key_buffer_before: String,
    width: usize,
    height: usize,
    /// Set by Term::drop to signal the redraw thread to exit.
    shutdown: bool,
    /// Set by `resume_after_external` (and similar) to force the
    /// next redraw to wipe its `Screen` cache and repaint from
    /// scratch. The redraw loop reads-and-clears this flag.
    invalidate_screen: bool,
    /// Generation counter for `redraw_sync`. Caller bumps
    /// `sync_requested`; redraw thread sets `sync_completed =
    /// sync_requested` atomically with going idle (right before
    /// blocking on recv).
    sync_requested: u64,
    sync_completed: u64,
    /// Raw escape sequences (or any other byte string) waiting to be
    /// written by the redraw thread on its next pass. Producers push
    /// here via `TermHandle::print_terminal_escape` to ensure their
    /// bytes don't interleave with the active frame's render output.
    pending_raw: Vec<String>,
}

impl SharedState {
    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }

    fn reset_history_nav(&mut self) {
        self.history_nav_entries.clear();
        self.history_nav_index = None;
        self.history_nav_active = false;
    }

    fn save_buffer_to_history_nav(&mut self) {
        if let Some(index) = self.history_nav_index {
            if index < self.history_nav_entries.len() {
                self.history_nav_entries[index] = self.buffer.clone();
            }
        }
    }

    fn apply_history_nav_entry(&mut self, index: usize) {
        self.buffer = self.history_nav_entries[index].clone();
        self.cursor = self.buffer.len();
        self.history_nav_index = Some(index);
    }

    fn navigate_history(&mut self, delta: isize) -> bool {
        if self.input_history.is_empty() && self.history_nav_index.is_none() {
            return false;
        }

        if self.history_nav_index.is_none() {
            self.history_nav_entries = self.input_history.clone();
            self.history_nav_entries.push(self.buffer.clone());
            self.history_nav_index = Some(self.history_nav_entries.len() - 1);
        } else if let Some(index) = self.history_nav_index
            && index + 1 == self.history_nav_entries.len()
        {
            self.save_buffer_to_history_nav();
        }

        let current = self
            .history_nav_index
            .expect("history nav index initialized");
        let new_index = current as isize + delta;
        if new_index < 0 || self.history_nav_entries.len() as isize <= new_index {
            return false;
        }

        self.apply_history_nav_entry(new_index as usize);
        self.history_nav_active = true;
        true
    }

    fn set_history_nav_active(&mut self, active: bool) {
        self.history_nav_active = active;
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
    /// The user pressed Tab.
    Tab,
    /// The user pressed Shift-Tab (backtab).
    BackTab,
    /// The user pressed Escape.
    Escape,
    /// The user requested an external editor (Ctrl-O / Ctrl-G).
    /// Caller is expected to call [`Term::pause_for_external`], spawn
    /// `$VISUAL`/`$EDITOR`, and call [`Term::resume_after_external`].
    ExternalEditor,
}

/// A cloneable handle for mutating prompt zones from any thread.
///
/// Setters update the shared state but do **not** trigger a redraw.
/// Call [`redraw`](TermHandle::redraw) after making all changes.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    sync_condvar: Arc<std::sync::Condvar>,
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
    ///
    /// This goes through the differential update path — only the
    /// visible viewport is repainted. Use it for any mutation
    /// guaranteed to be inside the viewport (input, status chip,
    /// streaming live blocks, newly-printed blocks). For mutations
    /// to past blocks that may have scrolled into scrollback, use
    /// [`invalidate_screen`](Self::invalidate_screen) instead. See
    /// `README.md` § "When mutations need a full redraw" for the
    /// full rule.
    pub fn redraw(&self) {
        self.redraw.notify();
    }

    /// Drops every rendered block from every output zone and forces a
    /// full repaint. The prompt, current input buffer, and input-line
    /// history are left intact.
    pub fn clear_output(&self) {
        let mut st = self.lock();
        st.blocks.clear();
        st.history.clear();
        st.above_active.clear();
        st.above_sticky.clear();
        st.suggestions.clear();
        st.below.clear();
        st.invalidate_screen = true;
        drop(st);
        self.redraw.notify();
    }

    /// Forces the next redraw to take the full-render path: clear
    /// the visible screen + scrollback (`\x1b[2J\x1b[H\x1b[3J`)
    /// and re-emit every line from `all_lines`. Overflow scrolls
    /// naturally into the (now-empty) scrollback.
    ///
    /// Use this when a mutation affects rows that may already be in
    /// terminal scrollback — e.g. toggling visibility of a block
    /// from a past turn (`/show-diff`, `/show-thinking`). The
    /// differential renderer only repaints the visible window, so
    /// without invalidation those scrolled-out rows would remain as
    /// stale fossils that disagree with current state. See
    /// `README.md` § "When mutations need a full redraw".
    pub fn invalidate_screen(&self) {
        self.lock().invalidate_screen = true;
        self.redraw.notify();
    }

    /// Triggers a redraw and blocks until the redraw thread has
    /// processed it. Uses a generation counter: the caller bumps
    /// `sync_requested`, the redraw thread sets `sync_completed`
    /// atomically with going idle (right before blocking on recv).
    pub fn redraw_sync(&self) {
        let mut st = self.lock();
        st.sync_requested += 1;
        let target = st.sync_requested;
        drop(st);

        self.redraw.notify();

        let st = self.state.lock().expect("term state mutex poisoned");
        let _st = self
            .sync_condvar
            .wait_while(st, |s| s.sync_completed < target)
            .expect("term state mutex poisoned");
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
        st.suggestions.retain(|&x| x != id);
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

    /// Appends a block id to the suggestions zone (if not already
    /// present). Rendered between the prompt and below blocks.
    pub fn push_suggestions(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.suggestions.contains(&id) {
            st.suggestions.push(id);
        }
    }

    /// Removes a block id from the suggestions zone.
    pub fn remove_suggestions(&self, id: BlockId) {
        self.lock().suggestions.retain(|&x| x != id);
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

    /// Returns a clone of the current input buffer.
    pub fn get_buffer(&self) -> String {
        self.lock().buffer.clone()
    }

    /// Returns the current cursor position in bytes.
    pub fn get_cursor(&self) -> usize {
        self.lock().cursor
    }

    /// Replaces the input buffer and cursor position.
    pub fn set_buffer(&self, text: String, cursor: usize) {
        let mut st = self.lock();
        st.cursor = cursor.min(text.len());
        st.buffer = text;
        st.save_buffer_to_history_nav();
    }

    /// Updates the right prompt.
    pub fn set_right_prompt(&self, text: impl Into<StyledText>) {
        self.lock().right_prompt = text.into();
    }

    /// Queues a raw byte string (typically a terminal escape sequence
    /// that doesn't change visible output, like an OSC user-var
    /// notification) to be written by the redraw thread on its next
    /// pass. Goes through the redraw loop so the bytes never
    /// interleave with an in-flight frame.
    pub fn print_terminal_escape(&self, sequence: impl Into<String>) {
        self.lock().pending_raw.push(sequence.into());
        self.redraw.notify();
    }
}

/// Raw terminal events from the crossterm reader thread.
pub enum RawEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    /// One bracketed paste. The whole pasted string is delivered
    /// atomically so a multi-line paste doesn't trigger Enter on
    /// embedded newlines.
    Paste(String),
}

/// The terminal prompt engine.
///
/// Owns the input event loop. Call [`Term::get_next_event`] in a loop to
/// drive it.
///
/// Real terminals read from stdin synchronously inside `get_next_event`
/// — there is intentionally **no** background reader thread, so there
/// is nobody to race a foreground program (like `$EDITOR`) for stdin
/// bytes. While the main thread is blocked in `event::read()`, the
/// redraw thread keeps repainting on its own clock.
///
/// Virtual terminals (tests) use the injected channel branch.
pub struct Term {
    /// Shared mutable state.
    state: Arc<Mutex<SharedState>>,
    /// Notifies the redraw thread that the screen needs updating.
    redraw: tau_blocking_notify_channel::Sender,
    /// For virtual terms only: receives events injected via the test
    /// sender returned from `new_virtual`. Real terms leave this
    /// `None` and read directly from crossterm.
    term_input_rx: Option<std::sync::mpsc::Receiver<RawEvent>>,
    /// Redraw thread handle — taken and joined on drop.
    redraw_thread: Option<JoinHandle<()>>,
    /// Whether to disable raw mode on drop (false for virtual terms).
    owns_raw_mode: bool,
    cursor_shape: CursorShape,
}

impl Term {
    /// Creates a new terminal prompt.
    ///
    /// Enters raw mode, spawns the input reader and redraw threads.
    /// Returns the prompt engine and a cloneable [`TermHandle`].
    pub fn new(
        left_prompt: impl Into<StyledText>,
        cursor_shape: CursorShape,
    ) -> io::Result<(Self, TermHandle)> {
        let (width, height) = term_size();
        let state = Arc::new(Mutex::new(SharedState {
            blocks: HashMap::new(),
            next_id: 0,
            history: Vec::new(),
            above_active: Vec::new(),
            above_sticky: Vec::new(),
            suggestions: Vec::new(),
            below: Vec::new(),
            left_prompt: left_prompt.into(),
            right_prompt: StyledText::new(),
            buffer: String::new(),
            cursor: 0,
            input_history: Vec::new(),
            history_nav_entries: Vec::new(),
            history_nav_index: None,
            history_nav_active: false,
            last_key_direction: None,
            last_key_started_history_nav: false,
            last_key_buffer_before: String::new(),
            width,
            height,
            shutdown: false,
            invalidate_screen: false,
            sync_requested: 0,
            sync_completed: 0,
            pending_raw: Vec::new(),
        }));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();
        let sync_condvar = Arc::new(std::sync::Condvar::new());

        terminal::enable_raw_mode()?;
        // Opt into bracketed paste so the terminal wraps pasted content
        // in `ESC[200~` / `ESC[201~` and crossterm surfaces it as one
        // `CtEvent::Paste(String)` instead of a stream of individual
        // KeyEvents (which, without bracketed paste, leaked literal
        // escape-sequence bytes into the input buffer).
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::event::EnableBracketedPaste,
            cursor_shape.crossterm_style()
        );

        let redraw_state = Arc::clone(&state);
        let redraw_writer: Box<dyn Write + Send> = Box::new(io::stdout());
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_thread = thread::spawn(move || {
            redraw_loop(redraw_state, redraw_rx, redraw_writer, &redraw_sync_cv);
        });

        let handle = TermHandle {
            state: Arc::clone(&state),
            sync_condvar,
            redraw: redraw_tx.clone(),
        };

        redraw_tx.notify();

        Ok((
            Self {
                state,
                redraw: redraw_tx,
                term_input_rx: None,
                redraw_thread: Some(redraw_thread),
                owns_raw_mode: true,
                cursor_shape,
            },
            handle,
        ))
    }

    /// Creates a virtual terminal for testing.
    ///
    /// No raw mode, no crossterm input reader. Output goes to the
    /// provided writer (e.g. a pipe). Input is injected via the
    /// returned `Sender<RawEvent>`.
    pub fn new_virtual(
        width: usize,
        height: usize,
        left_prompt: impl Into<StyledText>,
        output: Box<dyn Write + Send>,
        cursor_shape: CursorShape,
    ) -> (Self, TermHandle, std::sync::mpsc::Sender<RawEvent>) {
        let state = Arc::new(Mutex::new(SharedState {
            blocks: HashMap::new(),
            next_id: 0,
            history: Vec::new(),
            above_active: Vec::new(),
            above_sticky: Vec::new(),
            suggestions: Vec::new(),
            below: Vec::new(),
            left_prompt: left_prompt.into(),
            right_prompt: StyledText::new(),
            buffer: String::new(),
            cursor: 0,
            input_history: Vec::new(),
            history_nav_entries: Vec::new(),
            history_nav_index: None,
            history_nav_active: false,
            last_key_direction: None,
            last_key_started_history_nav: false,
            last_key_buffer_before: String::new(),
            width,
            height,
            shutdown: false,
            invalidate_screen: false,
            sync_requested: 0,
            sync_completed: 0,
            pending_raw: Vec::new(),
        }));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();
        let sync_condvar = Arc::new(std::sync::Condvar::new());

        let redraw_state = Arc::clone(&state);
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_thread = thread::spawn(move || {
            redraw_loop(redraw_state, redraw_rx, output, &redraw_sync_cv);
        });

        let (term_input_tx, term_input_rx) = std::sync::mpsc::channel();

        let handle = TermHandle {
            state: Arc::clone(&state),
            sync_condvar,
            redraw: redraw_tx.clone(),
        };

        redraw_tx.notify();

        let term = Self {
            state,
            redraw: redraw_tx,
            term_input_rx: Some(term_input_rx),
            redraw_thread: Some(redraw_thread),
            owns_raw_mode: false,
            cursor_shape,
        };

        (term, handle, term_input_tx)
    }

    /// Triggers a redraw of the terminal.
    pub fn redraw(&self) {
        self.redraw.notify();
    }

    /// Drops every rendered block from every output zone and forces a
    /// full repaint. The prompt, current input buffer, and input-line
    /// history are left intact.
    pub fn clear_output(&self) {
        let mut st = self.state.lock().expect("term state mutex poisoned");
        st.blocks.clear();
        st.history.clear();
        st.above_active.clear();
        st.above_sticky.clear();
        st.suggestions.clear();
        st.below.clear();
        st.invalidate_screen = true;
        drop(st);
        self.redraw.notify();
    }

    /// Blocks until the next meaningful input event.
    ///
    /// Handles key editing internally (insert, delete, cursor movement)
    /// and only surfaces events the downstream cares about. Triggers
    /// a redraw before returning so internal state changes are visible.
    pub fn get_next_event(&self) -> io::Result<Event> {
        loop {
            let raw = match self.next_raw() {
                Some(ev) => ev,
                None => return Ok(Event::Eof),
            };

            match raw {
                RawEvent::Key(key) => {
                    {
                        let mut st = self.state.lock().expect("term state mutex poisoned");
                        st.last_key_started_history_nav = st.history_nav_active;
                        st.last_key_buffer_before = st.buffer.clone();
                        st.last_key_direction = match key.code {
                            KeyCode::Up => Some(KeyDirection::Up),
                            KeyCode::Down => Some(KeyDirection::Down),
                            _ => None,
                        };
                    }
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
                RawEvent::Paste(text) => {
                    // Insert the whole paste at the cursor in one go.
                    // Going through the per-char path would re-trigger
                    // the redraw thread N times and, more importantly,
                    // would expose embedded `\n` bytes to the Enter
                    // handler and submit the line mid-paste.
                    if text.is_empty() {
                        self.redraw.notify();
                        continue;
                    }
                    {
                        let mut st = self.state.lock().expect("term state mutex poisoned");
                        let cursor = st.cursor;
                        st.buffer.insert_str(cursor, &text);
                        st.cursor = cursor + text.len();
                        st.save_buffer_to_history_nav();
                    }
                    self.redraw.notify();
                    return Ok(Event::BufferChanged);
                }
            }
        }
    }

    /// Reads the next raw event, blocking until one arrives.
    ///
    /// Real terminals call `crossterm::event::read()` inline so there
    /// is no background reader thread fighting a foreground program
    /// (e.g. `$EDITOR`) for stdin bytes. Virtual terminals receive
    /// from the test sender returned by `new_virtual`.
    fn next_raw(&self) -> Option<RawEvent> {
        if let Some(rx) = self.term_input_rx.as_ref() {
            return rx.recv().ok();
        }
        match event::read().ok()? {
            CtEvent::Key(key) => Some(RawEvent::Key(key)),
            CtEvent::Resize(w, h) => Some(RawEvent::Resize(w, h)),
            CtEvent::Paste(text) => Some(RawEvent::Paste(text)),
            // Mouse / focus events: skip and recurse so the caller
            // still observes the channel/stdin as "blocking".
            _ => self.next_raw(),
        }
    }

    /// Moves the cursor/history as if Up was pressed outside completion.
    pub fn move_up(&self) {
        move_up_in_state(&self.state);
        self.redraw.notify();
    }

    /// Moves the cursor/history as if Down was pressed outside completion.
    pub fn move_down(&self) {
        move_down_in_state(&self.state);
        self.redraw.notify();
    }

    /// Returns true if the last raw event read by this terminal was Up.
    pub fn last_key_was_up(&self) -> bool {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .last_key_direction
            == Some(KeyDirection::Up)
    }

    /// Returns true if the last raw event read by this terminal was Down.
    pub fn last_key_was_down(&self) -> bool {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .last_key_direction
            == Some(KeyDirection::Down)
    }

    /// Returns the input buffer before the current raw key was handled.
    pub fn last_key_buffer_before(&self) -> String {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .last_key_buffer_before
            .clone()
    }

    /// Returns true if the current raw key began while input-history navigation
    /// was active.
    pub fn last_key_started_history_navigation(&self) -> bool {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .last_key_started_history_nav
    }

    /// Clears the marker for the current raw key's initial history-navigation
    /// state.
    pub fn clear_last_key_started_history_navigation(&self) {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .last_key_started_history_nav = false;
    }

    /// Returns true while Up/Down is navigating input history.
    pub fn is_history_navigation_active(&self) -> bool {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .history_nav_active
    }

    /// Leaves input-history navigation mode without changing the buffer.
    pub fn reset_history_navigation(&self) {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .reset_history_nav();
    }

    /// Marks whether the current buffer is actively being selected from input
    /// history.
    pub fn set_history_navigation_active(&self, active: bool) {
        self.state
            .lock()
            .expect("term state mutex poisoned")
            .set_history_nav_active(active);
    }

    /// Removes the most recently submitted input-history entry if it matches
    /// `line`.
    pub fn remove_last_history_entry_if_eq(&self, line: &str) {
        let mut st = self.state.lock().expect("term state mutex poisoned");
        if st.input_history.last().is_some_and(|entry| entry == line) {
            st.input_history.pop();
        }
    }

    /// Releases the terminal for an external program (e.g. `$EDITOR`):
    /// disables raw mode + bracketed paste and clears the screen so
    /// the editor starts on a clean canvas.
    ///
    /// No reader-thread coordination is needed — the only reader is
    /// the main thread, which is the same thread that drives the
    /// external program via `Command::status`, so it can't be in
    /// `event::read()` at the same time.
    pub fn pause_for_external(&self) -> io::Result<()> {
        if !self.owns_raw_mode {
            return Ok(());
        }
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        terminal::disable_raw_mode()?;
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::style::ResetColor,
            crossterm::cursor::MoveTo(0, 0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        );
        Ok(())
    }

    /// Re-acquires raw mode + bracketed paste after an external
    /// program. Marks the redraw thread'\''s `Screen` cache stale so the
    /// next render repaints from scratch; without this, the cache
    /// would diff against what we *thought* was on screen and skip
    /// drawing anything since the editor exited.
    pub fn resume_after_external(&self) -> io::Result<()> {
        if !self.owns_raw_mode {
            return Ok(());
        }
        terminal::enable_raw_mode()?;
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::event::EnableBracketedPaste,
            self.cursor_shape.crossterm_style()
        );
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
        {
            let mut st = self.state.lock().expect("term state mutex poisoned");
            st.invalidate_screen = true;
        }
        self.redraw.notify();
        Ok(())
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
                    st.last_key_started_history_nav = false;
                    st.cursor = st.buffer.len();
                    let line = std::mem::take(&mut st.buffer);
                    st.cursor = 0;
                    st.reset_history_nav();
                    st.input_history.push(line.clone());
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
                let mut st = self.state.lock().expect("term state mutex poisoned");
                if st.buffer.is_empty() {
                    return Ok(Some(Event::Eof));
                }
                st.buffer.clear();
                st.cursor = 0;
                st.reset_history_nav();
                drop(st);
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Char('u') if ctrl => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    let cursor = st.cursor;
                    st.buffer.drain(..cursor);
                    st.cursor = 0;
                    st.save_buffer_to_history_nav();
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
                        st.save_buffer_to_history_nav();
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

            KeyCode::Char('o' | 'g') if ctrl => {
                return Ok(Some(Event::ExternalEditor));
            }

            KeyCode::Char(ch) => {
                {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.last_key_direction = None;
                    let cursor = st.cursor;
                    st.buffer.insert(cursor, ch);
                    st.cursor += ch.len_utf8();
                    st.save_buffer_to_history_nav();
                }
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Backspace => {
                let changed = {
                    let mut st = self.state.lock().expect("term state mutex poisoned");
                    st.last_key_direction = None;
                    if st.cursor > 0 {
                        let prev = prev_char_boundary(&st.buffer, st.cursor);
                        let cursor = st.cursor;
                        st.buffer.drain(prev..cursor);
                        st.cursor = prev;
                        st.save_buffer_to_history_nav();
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
                    st.last_key_direction = None;
                    if st.cursor < st.buffer.len() {
                        let next = next_char_boundary(&st.buffer, st.cursor);
                        let cursor = st.cursor;
                        st.buffer.drain(cursor..next);
                        st.save_buffer_to_history_nav();
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
                move_up_in_state(&self.state);
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Down => {
                move_down_in_state(&self.state);
                return Ok(Some(Event::BufferChanged));
            }

            KeyCode::Home => {
                self.state.lock().expect("term state mutex poisoned").cursor = 0;
            }

            KeyCode::End => {
                let mut st = self.state.lock().expect("term state mutex poisoned");
                st.cursor = st.buffer.len();
            }

            KeyCode::Tab => {
                return Ok(Some(Event::Tab));
            }

            KeyCode::BackTab => {
                return Ok(Some(Event::BackTab));
            }

            KeyCode::Esc => {
                return Ok(Some(Event::Escape));
            }

            _ => {}
        }

        Ok(None)
    }
}

impl Term {
    /// Signals the redraw thread to do one final render, reposition
    /// the cursor below all content, and exit. Blocks until complete.
    fn shutdown(&mut self) {
        // Set the flag first, then notify — the redraw thread checks
        // the flag before blocking on recv, so it will see it on the
        // next iteration.
        {
            let mut st = self.state.lock().expect("term state mutex poisoned");
            st.shutdown = true;
        }
        self.redraw.notify();

        if let Some(handle) = self.redraw_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.shutdown();
        if self.owns_raw_mode {
            // Pair the `EnableBracketedPaste` we issued in `new`; the
            // terminal would keep bracketing subsequent pastes in
            // other programs until it's explicitly turned off.
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
            let _ = terminal::disable_raw_mode();
        }
    }
}

// --- Rendering helpers ---

/// Lays out blocks referenced by an id list, skipping missing ids
/// and blocks with empty content (so callers can "hide" a block by
/// swapping its content to empty without leaving a blank row).
fn layout_id_list(
    ids: &[BlockId],
    blocks: &HashMap<BlockId, StyledBlock>,
    width: usize,
    out: &mut Vec<Vec<Cell>>,
) {
    for id in ids {
        if let Some(block) = blocks.get(id) {
            if block.content.is_empty() {
                continue;
            }
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
        let first_cols: usize = first_line.iter().map(|c| c.col_width()).sum();
        let right_cols: usize = right_cells.iter().map(|c| c.col_width()).sum();
        let needed = first_cols + 1 + right_cols;
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_cols - right_cols;
            let mut padded = first_line.clone();
            padded.extend(std::iter::repeat_n(Cell::plain(' '), padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    let left_cols = st.left_prompt.char_count();
    let (buffer_cursor_row, cursor_col) =
        buffer_position_for_byte(&st.buffer, st.cursor, width, left_cols);
    let cursor_row = above_end + buffer_cursor_row;

    all_lines.extend(input_lines);
    layout_id_list(&st.suggestions, &st.blocks, width, &mut all_lines);
    layout_id_list(&st.below, &st.blocks, width, &mut all_lines);

    LayoutAll {
        all_lines,
        live_start,
        cursor_row,
        cursor_col,
    }
}

// --- Redraw thread ---

fn redraw_loop(
    state: Arc<Mutex<SharedState>>,
    notify_rx: tau_blocking_notify_channel::Receiver,
    mut writer: Box<dyn Write + Send>,
    sync_condvar: &std::sync::Condvar,
) {
    let (w, h) = {
        let st = state.lock().expect("term state mutex poisoned");
        (st.width, st.height)
    };
    let mut screen = Screen::new(w);
    let mut prev_width = w;
    let mut prev_height = h;
    let mut prev_visible_start: usize = 0;

    loop {
        // Check shutdown before blocking on the channel.
        {
            let st = state.lock().expect("term state mutex poisoned");
            if st.shutdown {
                // Final render + move cursor below all content.
                let layout = layout_all(&st);
                let total = layout.all_lines.len();
                let visible_start = total.saturating_sub(st.height.max(1));
                let visible = &layout.all_lines[visible_start..];
                let cursor_in_visible = layout.cursor_row.saturating_sub(visible_start);
                drop(st);

                screen.set_width(prev_width);
                let _ = screen.update(&mut writer, visible, (cursor_in_visible, layout.cursor_col));
                let below = total.saturating_sub(layout.cursor_row + 1);
                for _ in 0..=below {
                    let _ = writer.queue(crossterm::style::Print("\r\n"));
                }
                let _ = writer.flush();
                {
                    let mut st = state.lock().expect("term state mutex poisoned");
                    st.sync_completed = st.sync_requested;
                }
                sync_condvar.notify_all();
                break;
            }
        }

        // If a sync was requested but not yet completed, skip
        // blocking on recv and render immediately. Otherwise block
        // until the next notification arrives.
        {
            let st = state.lock().expect("term state mutex poisoned");
            if st.sync_completed >= st.sync_requested {
                drop(st);
                if notify_rx.recv().is_err() {
                    break;
                }
            }
        }

        let mut st = state.lock().expect("term state mutex poisoned");
        let width = st.width;
        let height = st.height.max(1);
        let size_changed = prev_width != width || prev_height != height;
        // Take-and-clear so the flag is one-shot.
        let force_full = std::mem::take(&mut st.invalidate_screen);
        // Capture the sync generation we're rendering against.
        // We must not advance sync_completed beyond this value,
        // because a later bump to sync_requested may have arrived
        // with state changes we haven't read yet.
        let sync_gen = st.sync_requested;
        let pending_raw = std::mem::take(&mut st.pending_raw);

        let layout = layout_all(&st);
        drop(st);

        // Pending escape sequences: emit before the frame so they
        // sit outside any synchronized-update bracket the renderer
        // installs. SetUserVar and similar OSC sequences don't
        // affect visible state, so ordering relative to the frame
        // doesn't matter for correctness — putting them first just
        // avoids any chance of interleaving with a deferred frame.
        for seq in &pending_raw {
            let _ = writer.write_all(seq.as_bytes());
        }
        if !pending_raw.is_empty() {
            let _ = writer.flush();
        }

        if force_full {
            // The terminal was clobbered by an external program
            // (\$EDITOR returned). Wipe Screen's cached idea of what's
            // on the terminal so `full_render` redraws from scratch.
            screen.invalidate();
        }

        if size_changed || force_full {
            // Path 2: Full render (resize, or post-external-program).
            if let Err(e) = full_render(&mut writer, &mut screen, &layout, width, height) {
                eprintln!("redraw: full render error: {e}");
            }
            prev_visible_start = layout.all_lines.len().saturating_sub(height);
        } else {
            let total = layout.all_lines.len();
            let visible_start = total.saturating_sub(height);

            screen.set_width(width);

            if visible_start > prev_visible_start {
                // Content pushed lines off the top. Use the
                // scrolling renderer (Pi-style) which renders
                // changed lines in order and lets \r\n at the
                // bottom naturally push content into scrollback.
                // See README.md.
                if let Err(e) = screen.render_scrolling(
                    &mut writer,
                    &layout.all_lines,
                    prev_visible_start,
                    height,
                    (layout.cursor_row, layout.cursor_col),
                ) {
                    eprintln!("redraw: scroll render error: {e}");
                }
            } else {
                // No overflow — normal differential update.
                let visible = &layout.all_lines[visible_start..];
                let cursor_in_visible = layout.cursor_row.saturating_sub(visible_start);
                if let Err(e) =
                    screen.update(&mut writer, visible, (cursor_in_visible, layout.cursor_col))
                {
                    eprintln!("redraw: update error: {e}");
                }
            }
            prev_visible_start = visible_start;
        }

        prev_width = width;
        prev_height = height;

        // Advance sync_completed to the generation we captured
        // before rendering.  Using max() is defensive — renders
        // are sequential so sync_gen is monotonically increasing,
        // but max() makes the invariant explicit.
        {
            let mut st = state.lock().expect("term state mutex poisoned");
            st.sync_completed = st.sync_completed.max(sync_gen);
        }
        sync_condvar.notify_all();
    }
}

/// Full re-render: clear screen + scrollback, output all lines,
/// position cursor. Used on resize and when content grows beyond
/// the viewport. After rendering, Screen tracks the visible
/// viewport for subsequent differential updates.
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
    // Clear screen, home cursor, and clear scrollback. The
    // scrollback is rebuilt by the overflow lines below.
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

    // Track what's visible on the terminal so the next
    // screen.update() can diff correctly.
    let visible_start = total.saturating_sub(height);
    let visible_lines = all_lines[visible_start..].to_vec();
    let cursor_in_visible = layout.cursor_row.saturating_sub(visible_start);
    screen.reset_to(visible_lines, cursor_in_visible, layout.cursor_col);

    Ok(())
}

// --- Helpers ---

fn move_cursor_vertical(st: &SharedState, delta: isize) -> Option<usize> {
    let width = st.width.max(1);
    let left_cols = st.left_prompt.char_count();
    let (current_row, current_col) =
        buffer_position_for_byte(&st.buffer, st.cursor, width, left_cols);

    let target_row = current_row as isize + delta;
    if target_row < 0 {
        return None;
    }
    let target_row = target_row as usize;

    let (max_row, _) = buffer_end_position(&st.buffer, width, left_cols);
    if max_row < target_row {
        return None;
    }

    Some(byte_offset_for_buffer_position(
        &st.buffer,
        target_row,
        current_col,
        width,
        left_cols,
    ))
}

fn move_up_in_state(state: &Arc<Mutex<SharedState>>) {
    let mut st = state.lock().expect("term state mutex poisoned");
    if st.history_nav_index.is_some() {
        st.navigate_history(-1);
    } else if let Some(new_cursor) = move_cursor_vertical(&st, -1) {
        st.cursor = new_cursor;
    } else {
        st.navigate_history(-1);
    }
}

fn move_down_in_state(state: &Arc<Mutex<SharedState>>) {
    let mut st = state.lock().expect("term state mutex poisoned");
    if st.history_nav_index.is_some() {
        st.navigate_history(1);
    } else if let Some(new_cursor) = move_cursor_vertical(&st, 1) {
        st.cursor = new_cursor;
    } else {
        st.navigate_history(1);
    }
}

fn term_size() -> (usize, usize) {
    terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((80, 24))
}

fn initial_buffer_position(initial_cols: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    (initial_cols / width, initial_cols % width)
}

fn advance_buffer_position(row: &mut usize, col: &mut usize, ch: char, width: usize) {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1);
    if ch == '\n' {
        *row += 1;
        *col = 0;
        return;
    }

    let char_width = ch.width().unwrap_or(0);
    if 0 < *col && width < *col + char_width {
        *row += 1;
        *col = 0;
    }
    *col += char_width;
    if width <= *col {
        *row += *col / width;
        *col %= width;
    }
}

fn buffer_position_for_byte(
    s: &str,
    byte_pos: usize,
    width: usize,
    initial_cols: usize,
) -> (usize, usize) {
    let mut pos = initial_buffer_position(initial_cols, width);
    for (byte, ch) in s.char_indices() {
        if byte_pos <= byte {
            break;
        }
        advance_buffer_position(&mut pos.0, &mut pos.1, ch, width);
    }
    pos
}

fn buffer_end_position(s: &str, width: usize, initial_cols: usize) -> (usize, usize) {
    buffer_position_for_byte(s, s.len(), width, initial_cols)
}

fn byte_offset_for_buffer_position(
    s: &str,
    target_row: usize,
    target_col: usize,
    width: usize,
    initial_cols: usize,
) -> usize {
    let mut row_col = initial_buffer_position(initial_cols, width);

    for (byte, ch) in s.char_indices() {
        let (row, col) = row_col;
        if target_row < row || (target_row == row && target_col <= col) {
            return byte;
        }
        if ch == '\n' {
            if target_row == row {
                return byte;
            }
            advance_buffer_position(&mut row_col.0, &mut row_col.1, ch, width);
            continue;
        }

        let mut next = row_col;
        advance_buffer_position(&mut next.0, &mut next.1, ch, width);
        if target_row < next.0 || (target_row == next.0 && target_col <= next.1) {
            return byte + ch.len_utf8();
        }
        row_col = next;
    }

    s.len()
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

#[cfg(test)]
mod tests;
