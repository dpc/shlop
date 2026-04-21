//! Higher-level terminal prompt with fish-like slash-command completion.
//!
//! Wraps [`tau_cli_term_raw`] and adds:
//!
//! - Slash-command registry with descriptions
//! - Tab-completion menu rendered below the prompt
//! - Tab / Shift-Tab cycling through matches
//! - Inline buffer replacement on selection
//! - Dynamic argument completion (e.g. model names for `/model`)

mod completion;
pub mod resolve;

use std::io;

pub use completion::{CommandName, Completer, CompletionData, CompletionItem, SlashCommand};
use tau_cli_term_raw::Event as RawEvent;
pub use tau_cli_term_raw::{
    Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText, TermHandle,
};
use tau_themes::Theme;

/// High-level events surfaced to the caller.
pub enum Event {
    /// The user submitted a line (pressed Enter).
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The input buffer changed.
    BufferChanged,
}

/// Higher-level terminal prompt with completion support.
pub struct HighTerm {
    term: tau_cli_term_raw::Term,
    handle: TermHandle,
    completer: Completer,
}

impl HighTerm {
    /// Creates a new terminal with the given prompt and slash commands.
    ///
    /// Returns the terminal, a thread-safe handle for rendering, and a
    /// [`CompletionData`] handle for pushing dynamic argument completions
    /// from background threads.
    pub fn new(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        let (term, handle) = tau_cli_term_raw::Term::new(left_prompt)?;
        let handle_clone = handle.clone();
        let data = CompletionData::new();
        let data_clone = data.clone();
        let completer = Completer::new(commands, data, theme);
        Ok((
            Self {
                term,
                handle,
                completer,
            },
            handle_clone,
            data_clone,
        ))
    }

    /// Returns a reference to the [`TermHandle`].
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Triggers a redraw.
    pub fn redraw(&self) {
        self.handle.redraw();
    }

    /// Appends persistent output to history.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> BlockId {
        self.handle.print_output(block)
    }

    /// Blocks until the next high-level event, handling completion
    /// menu interaction internally.
    pub fn get_next_event(&mut self) -> io::Result<Event> {
        loop {
            let raw = self.term.get_next_event()?;

            match raw {
                RawEvent::BufferChanged => {
                    self.completer.on_buffer_changed(&self.handle);
                    self.handle.redraw();
                    return Ok(Event::BufferChanged);
                }

                RawEvent::Tab => {
                    if self.completer.is_active() {
                        self.completer.cycle_selection(1, &self.handle);
                        self.handle.redraw();
                    }
                    continue;
                }

                RawEvent::BackTab => {
                    if self.completer.is_active() {
                        self.completer.cycle_selection(-1, &self.handle);
                        self.handle.redraw();
                    }
                    continue;
                }

                RawEvent::Escape => {
                    if self.completer.is_active() {
                        self.completer.dismiss(&self.handle);
                        self.handle.redraw();
                    }
                    continue;
                }

                RawEvent::Line(line) => {
                    if self.completer.is_active() && self.completer.has_selection() {
                        self.completer.accept_selection(&self.handle);
                        self.handle.redraw();
                        continue;
                    }
                    self.completer.dismiss(&self.handle);
                    self.handle.redraw();
                    return Ok(Event::Line(line));
                }

                RawEvent::Eof => {
                    self.completer.dismiss(&self.handle);
                    return Ok(Event::Eof);
                }

                RawEvent::Resize { width, height } => {
                    self.completer.rebuild_menu(&self.handle);
                    self.handle.redraw();
                    return Ok(Event::Resize { width, height });
                }
            }
        }
    }
}
