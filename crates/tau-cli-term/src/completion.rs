//! Slash-command and argument completion engine.
//!
//! Contains:
//! - [`SlashCommand`] — static command registration
//! - [`CompletionItem`] / [`CompletionData`] — dynamic argument completions
//! - [`Completer`] — state machine, matching, and menu rendering

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tau_cli_term_raw::{BlockId, Span, StyledBlock, StyledText, TermHandle};
use tau_themes::Theme;

use crate::resolve;

// -----------------------------------------------------------------
// Public types
// -----------------------------------------------------------------

/// A slash-command name, always prefixed with `/` (e.g. `"/model"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandName(String);

impl CommandName {
    pub fn new(name: impl Into<String>) -> Self {
        let s = name.into();
        debug_assert!(s.starts_with('/'), "CommandName must start with '/'");
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A slash command with its name and description.
#[derive(Clone, Debug)]
pub struct SlashCommand {
    /// The command name (e.g. `"/quit"`).
    pub name: CommandName,
    /// Short description shown in the completion menu.
    pub description: String,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: CommandName::new(name),
            description: description.into(),
        }
    }
}

/// A single argument completion candidate.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    /// The completion value (e.g. `"openai/gpt-4o"`).
    pub value: String,
    /// Optional description shown alongside the value.
    pub description: String,
}

impl CompletionItem {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }

    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: String::new(),
        }
    }
}

/// Thread-safe storage for dynamic argument completions.
///
/// Clone this handle and pass it to background threads that need to
/// update available completions (e.g. when the harness sends a model
/// list).
#[derive(Clone)]
pub struct CompletionData {
    inner: Arc<Mutex<HashMap<CommandName, Vec<CompletionItem>>>>,
}

impl Default for CompletionData {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionData {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Sets the argument completions for a slash command.
    pub fn set_arg_completions(&self, command: CommandName, items: Vec<CompletionItem>) {
        self.inner
            .lock()
            .expect("completion data lock")
            .insert(command, items);
    }

    fn get_arg_completions(&self, command: &CommandName) -> Option<Vec<CompletionItem>> {
        self.inner
            .lock()
            .expect("completion data lock")
            .get(command)
            .cloned()
    }
}

// -----------------------------------------------------------------
// Internal types
// -----------------------------------------------------------------

/// Resolved candidate ready for display and insertion.
struct Candidate {
    /// Text shown in the left column of the menu.
    label: String,
    /// Description shown in the right column.
    description: String,
    /// Full buffer text to set when this candidate is accepted.
    buffer_text: String,
}

/// Completion state machine.
struct State {
    candidates: Vec<Candidate>,
    selected: Option<usize>,
    menu_block_id: Option<BlockId>,
}

impl State {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            selected: None,
            menu_block_id: None,
        }
    }

    fn is_active(&self) -> bool {
        !self.candidates.is_empty()
    }

    fn reset(&mut self) {
        self.candidates.clear();
        self.selected = None;
    }
}

// -----------------------------------------------------------------
// Completer
// -----------------------------------------------------------------

/// Slash-command and argument completion engine.
///
/// Manages the completion lifecycle: filtering candidates from the
/// static command registry and dynamic [`CompletionData`], rendering
/// the menu, cycling through selections, and accepting/dismissing.
pub struct Completer {
    commands: Vec<SlashCommand>,
    data: CompletionData,
    theme: Theme,
    state: State,
}

impl Completer {
    pub(crate) fn new(commands: Vec<SlashCommand>, data: CompletionData, theme: Theme) -> Self {
        Self {
            commands,
            data,
            theme,
            state: State::new(),
        }
    }

    /// Returns a reference to the shared completion data.
    pub fn data(&self) -> &CompletionData {
        &self.data
    }

    /// Whether the completion menu is currently shown.
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Whether a candidate is currently highlighted.
    pub fn has_selection(&self) -> bool {
        self.state.selected.is_some()
    }

    /// Update completions based on the current buffer content.
    pub(crate) fn on_buffer_changed(&mut self, handle: &TermHandle) {
        let buffer = handle.get_buffer();

        if !buffer.starts_with('/') {
            if self.state.is_active() {
                self.hide_menu(handle);
                self.state.reset();
            }
            return;
        }

        let candidates = if let Some(space_pos) = buffer.find(' ') {
            let cmd = &buffer[..space_pos];
            let arg_prefix = &buffer[space_pos + 1..];
            self.build_arg_candidates(cmd, arg_prefix)
        } else {
            self.build_cmd_candidates(&buffer)
        };

        if candidates.is_empty() {
            if self.state.is_active() {
                self.hide_menu(handle);
                self.state.reset();
            }
            return;
        }

        self.state.candidates = candidates;
        self.state.selected = None;
        self.render_menu(handle);
    }

    /// Cycle the selection by `delta` (+1 = forward, -1 = backward).
    pub(crate) fn cycle_selection(&mut self, delta: isize, handle: &TermHandle) {
        if self.state.candidates.is_empty() {
            return;
        }
        let len = self.state.candidates.len();
        let new_idx = match self.state.selected {
            None => {
                if delta > 0 {
                    0
                } else {
                    len - 1
                }
            }
            Some(idx) => ((idx as isize + delta).rem_euclid(len as isize)) as usize,
        };
        self.state.selected = Some(new_idx);
        self.render_menu(handle);
    }

    /// Accept the current selection: set the buffer and dismiss the
    /// menu. Returns `true` if a selection was active and accepted.
    pub(crate) fn accept_selection(&mut self, handle: &TermHandle) -> bool {
        let idx = match self.state.selected {
            Some(idx) => idx,
            None => return false,
        };
        let buffer_text = self.state.candidates[idx].buffer_text.clone();
        let cursor = buffer_text.len();
        handle.set_buffer(buffer_text, cursor);
        self.hide_menu(handle);
        self.state.reset();
        true
    }

    /// Dismiss the completion menu without accepting.
    pub(crate) fn dismiss(&mut self, handle: &TermHandle) {
        self.hide_menu(handle);
        self.state.reset();
    }

    /// Rebuild the menu at the current size (e.g. after resize).
    pub(crate) fn rebuild_menu(&mut self, handle: &TermHandle) {
        if self.state.is_active() {
            self.render_menu(handle);
        }
    }

    // -----------------------------------------------------------------
    // Candidate building
    // -----------------------------------------------------------------

    /// Builds command-name candidates from the static registry.
    fn build_cmd_candidates(&self, prefix: &str) -> Vec<Candidate> {
        self.commands
            .iter()
            .filter(|cmd| cmd.name.as_str().starts_with(prefix))
            .map(|cmd| Candidate {
                label: cmd.name.to_string(),
                description: cmd.description.clone(),
                buffer_text: cmd.name.to_string(),
            })
            .collect()
    }

    /// Builds argument candidates from dynamic [`CompletionData`].
    ///
    /// Uses case-insensitive matching with prefix matches sorted
    /// before substring matches.
    fn build_arg_candidates(&self, cmd: &str, arg_prefix: &str) -> Vec<Candidate> {
        let cmd_name = CommandName::new(cmd);
        let items = match self.data.get_arg_completions(&cmd_name) {
            Some(items) => items,
            None => return Vec::new(),
        };

        let needle = arg_prefix.to_lowercase();
        let mut prefix_matches = Vec::new();
        let mut substr_matches = Vec::new();

        for item in &items {
            let hay = item.value.to_lowercase();
            if needle.is_empty() || hay.starts_with(&needle) {
                prefix_matches.push(Candidate {
                    label: item.value.clone(),
                    description: item.description.clone(),
                    buffer_text: format!("{cmd} {}", item.value),
                });
            } else if hay.contains(&needle) {
                substr_matches.push(Candidate {
                    label: item.value.clone(),
                    description: item.description.clone(),
                    buffer_text: format!("{cmd} {}", item.value),
                });
            }
        }

        prefix_matches.extend(substr_matches);
        prefix_matches
    }

    // -----------------------------------------------------------------
    // Menu rendering
    // -----------------------------------------------------------------

    fn render_menu(&mut self, handle: &TermHandle) {
        let selected_style = resolve::resolve(&self.theme, tau_themes::names::COMPLETION_SELECTED);
        let label_style = resolve::resolve(&self.theme, tau_themes::names::COMPLETION_LABEL);
        let desc_style = resolve::resolve(&self.theme, tau_themes::names::COMPLETION_DESC);

        let max_label_len = self
            .state
            .candidates
            .iter()
            .map(|c| c.label.len())
            .max()
            .unwrap_or(0);

        let mut spans: Vec<Span> = Vec::new();
        for (i, candidate) in self.state.candidates.iter().enumerate() {
            if i > 0 {
                spans.push(Span::plain("\n"));
            }

            let is_selected = self.state.selected == Some(i);
            let padding = max_label_len - candidate.label.len() + 2;

            let line_text = if candidate.description.is_empty() {
                format!("  {}  ", candidate.label)
            } else {
                format!(
                    "  {}{:padding$}{}  ",
                    candidate.label,
                    "",
                    candidate.description,
                    padding = padding,
                )
            };

            if is_selected {
                spans.push(Span::new(line_text, selected_style));
            } else {
                spans.push(Span::plain("  "));
                spans.push(Span::new(&candidate.label, label_style));
                if !candidate.description.is_empty() {
                    spans.push(Span::plain(format!("{:padding$}", "", padding = padding)));
                    spans.push(Span::new(&candidate.description, desc_style));
                }
                spans.push(Span::plain("  "));
            }
        }

        let block = StyledBlock::new(StyledText::from(spans));
        let block_id = self.ensure_menu_block(handle);
        handle.set_block(block_id, block);
    }

    fn ensure_menu_block(&mut self, handle: &TermHandle) -> BlockId {
        if let Some(id) = self.state.menu_block_id {
            id
        } else {
            let id = handle.new_block("");
            handle.push_suggestions(id);
            self.state.menu_block_id = Some(id);
            id
        }
    }

    fn hide_menu(&mut self, handle: &TermHandle) {
        if let Some(id) = self.state.menu_block_id.take() {
            handle.remove_suggestions(id);
            handle.remove_block(id);
        }
    }
}
