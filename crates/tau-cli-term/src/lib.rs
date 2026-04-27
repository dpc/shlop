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
    Align, BlockId, Cell, Color, CursorShape, Span, Style, StyledBlock, StyledText, TermHandle,
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
    /// Shift+Tab pressed outside of completion — caller decides what
    /// to do with it (Pi-style: cycle effort).
    BackTab,
}

/// Higher-level terminal prompt with completion support.
pub struct HighTerm {
    term: tau_cli_term_raw::Term,
    handle: TermHandle,
    completer: Completer,
    theme: Theme,
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
        cursor_shape: CursorShape,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        let (term, handle) = tau_cli_term_raw::Term::new(left_prompt, cursor_shape)?;
        let handle_clone = handle.clone();
        let data = CompletionData::new();
        let data_clone = data.clone();
        let completer = Completer::new(commands, data, theme.clone());
        Ok((
            Self {
                term,
                handle,
                completer,
                theme,
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
                        continue;
                    }
                    return Ok(Event::BackTab);
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

                RawEvent::ExternalEditor => {
                    self.completer.dismiss(&self.handle);
                    self.run_external_editor();
                    self.handle.redraw_sync();
                    return Ok(Event::BufferChanged);
                }
            }
        }
    }

    /// Spawns `$VISUAL || $EDITOR` synchronously with the current
    /// input buffer in a tempfile, releases raw mode while it runs,
    /// and replaces the buffer with the result on success. Errors
    /// (no editor, spawn failure, non-zero exit) surface as a themed
    /// info line above the prompt.
    fn run_external_editor(&self) {
        match try_run_external_editor(&self.term, &self.handle) {
            Ok(Some(new_text)) => {
                let cursor = new_text.len();
                self.handle.set_buffer(new_text, cursor);
            }
            Ok(None) => {} // editor exited non-zero or text unchanged.
            Err(msg) => self.print_local(&format!("external editor: {msg}")),
        }
    }

    fn print_local(&self, message: &str) {
        let block = resolve::themed_block(
            &self.theme,
            tau_themes::names::SYSTEM_INFO,
            message.to_owned(),
        );
        self.handle.print_output(block);
    }
}

fn try_run_external_editor(
    term: &tau_cli_term_raw::Term,
    handle: &TermHandle,
) -> Result<Option<String>, String> {
    let editor_cmd = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no editor configured (\\$VISUAL / \\$EDITOR not set)".to_owned())?;

    let current = handle.get_buffer();
    let tmp = tempfile::Builder::new()
        .prefix("tau-edit-")
        .suffix(".tau.md")
        .tempfile()
        .map_err(|e| format!("could not create tempfile: {e}"))?;
    std::fs::write(tmp.path(), current.as_bytes())
        .map_err(|e| format!("could not write tempfile: {e}"))?;

    let mut parts = editor_cmd.split_whitespace();
    let editor = parts
        .next()
        .expect("split_whitespace yields at least one part");
    let editor_args: Vec<&str> = parts.collect();

    term.pause_for_external()
        .map_err(|e| format!("could not release terminal: {e}"))?;
    // RAII so a spawn error / panic still restores raw mode.
    struct ResumeGuard<'a>(&'a tau_cli_term_raw::Term);
    impl Drop for ResumeGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.resume_after_external();
        }
    }
    let _guard = ResumeGuard(term);

    let status = std::process::Command::new(editor)
        .args(&editor_args)
        .arg(tmp.path())
        .status()
        .map_err(|e| format!("could not spawn `{editor}`: {e}"))?;
    if !status.success() {
        return Ok(None);
    }
    let new_text =
        std::fs::read_to_string(tmp.path()).map_err(|e| format!("could not read tempfile: {e}"))?;
    let new_text = new_text.strip_suffix('\n').unwrap_or(&new_text).to_owned();
    Ok(Some(new_text))
}
