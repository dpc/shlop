//! Screen state tracker and renderer.
//!
//! [`Screen`] maintains an "actual" buffer representing what is
//! currently on the terminal. Two rendering methods use it:
//!
//! - [`Screen::update()`] — **Path 1** (differential update): diffs the visible
//!   viewport against the actual buffer and emits only the escape sequences
//!   needed to update changed cells.
//! - [`Screen::render_scrolling()`] — **Path 2** (scrolling render): diffs the
//!   full content array, renders changed lines in order, and lets `\r\n` at the
//!   bottom edge push content into the terminal's scrollback buffer.
//!
//! See `README.md` for the full rendering strategy.
//!
//! Diff approach borrowed from fish shell's `screen.rs`:
//! <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//!
//! Key design choices:
//! - Simple line model (`Vec<Vec<Cell>>`) — no soft-wrap tracking.
//! - Relative cursor movement only (`MoveUp`, `\r`, `\n`, `MoveToColumn`).
//! - `\n` for downward movement (scrolls at bottom edge, unlike `MoveDown`).

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::{Attribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, ClearType};

use crate::style::{Align, Cell, Style, StyledBlock, StyledText};

/// Column width of a cell slice (sum of individual display widths).
fn cols(cells: &[Cell]) -> usize {
    cells.iter().map(|c| c.col_width()).sum()
}

/// Virtual screen state with diff-based updates.
pub struct Screen {
    /// What we believe is currently displayed on the terminal.
    lines: Vec<Vec<Cell>>,
    /// Current terminal cursor row (relative to prompt start).
    cursor_row: usize,
    /// Current terminal cursor column.
    cursor_col: usize,
    /// Terminal width in columns.
    width: usize,
}

impl Screen {
    pub fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            width: width.max(1),
        }
    }

    /// Updates the terminal width. Call after a resize.
    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(1);
    }

    /// Returns the current terminal width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns the current cursor row (relative to prompt start).
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Diffs the desired content against the actual screen state and emits
    /// only the escape sequences needed to make the terminal match.
    ///
    /// `desired_lines` is the content split into physical rows.
    /// `desired_cursor` is `(row, col)` where the cursor should end up.
    pub fn update(
        &mut self,
        w: &mut impl Write,
        desired_lines: &[Vec<Cell>],
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        // Handle empty desired.
        if desired_lines.is_empty() {
            if !self.lines.is_empty() {
                self.move_to(w, 0, 0)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            }
            self.lines.clear();
            self.cursor_row = 0;
            self.cursor_col = 0;
            w.flush()?;
            return Ok(());
        }

        let desired_count = desired_lines.len();

        for (row, desired_line) in desired_lines.iter().enumerate() {
            let actual_line = self.lines.get(row);
            let actual_slice = actual_line.map(|l| l.as_slice()).unwrap_or(&[]);
            let desired_slice = desired_line.as_slice();

            // Find the first cell index where actual and desired differ.
            let common_prefix = actual_slice
                .iter()
                .zip(desired_slice.iter())
                .take_while(|(a, d)| a == d)
                .count();

            let is_last_desired = row == desired_count - 1;
            let actual_wider = cols(actual_slice) > cols(desired_slice);
            let has_extra_actual_below = is_last_desired && self.lines.len() > desired_count;

            // Skip if this line is completely unchanged and we don't need
            // to clear below.
            if common_prefix == actual_slice.len()
                && common_prefix == desired_slice.len()
                && !has_extra_actual_below
            {
                continue;
            }

            // Compute column offset for the first changed cell.
            // Done here (before move_to) to avoid borrowing self.lines
            // across the mutable self.move_to call.
            let prefix_cols = cols(&desired_slice[..common_prefix]);

            // Move to the first changed column on this row.
            self.move_to(w, row, prefix_cols)?;

            // Print the new content from the first difference onward.
            if common_prefix < desired_slice.len() {
                emit_styled_cells(w, &desired_slice[common_prefix..])?;
                // Track cursor column as the total column width of the
                // line (not cell count). At exactly `width` columns,
                // the terminal enters "pending wrap" state.
                self.cursor_col = cols(desired_slice);
            }

            // Clear trailing characters / lines below as needed.
            if has_extra_actual_below {
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_wider {
                w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
        }

        // Position the cursor where it should be.
        self.move_to(w, desired_cursor.0, desired_cursor.1)?;

        w.flush()?;

        // Actual now matches desired.
        self.lines = desired_lines.to_vec();

        Ok(())
    }

    /// Resets the actual state to empty. Call this after externally
    /// clearing the prompt area (e.g. before printing async output).
    /// The next `update()` will treat everything as new.
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Moves the cursor to the top of the prompt area and clears
    /// everything from there down. After this, `invalidate()` should
    /// be called to reset the actual state.
    pub fn erase_all(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.cursor_row > 0 {
            w.queue(MoveUp(self.cursor_row as u16))?;
        }
        w.queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::FromCursorDown))?;
        self.cursor_row = 0;
        self.cursor_col = 0;
        Ok(())
    }

    /// Renders all lines with scrolling support (Pi-style).
    ///
    /// Unlike `update()` which diffs only the visible viewport,
    /// this method diffs against the full previous content and
    /// renders changed lines in order. When rendering goes past
    /// the bottom of the terminal, `\r\n` naturally pushes the
    /// top row into the terminal's scrollback buffer.
    ///
    /// Call this instead of `update()` when `viewport_top`
    /// increased (content overflowed the viewport).
    ///
    /// `all_lines` is the complete content (not just the visible
    /// slice). `prev_viewport_top` is where the viewport was on
    /// the previous frame. `height` is the terminal height.
    /// `desired_cursor` is `(row, col)` in absolute line indices.
    ///
    /// Inspired by the Pi coding agent's TUI renderer.
    pub fn render_scrolling(
        &mut self,
        w: &mut impl Write,
        all_lines: &[Vec<Cell>],
        prev_viewport_top: usize,
        height: usize,
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        let total = all_lines.len();
        let new_viewport_top = total.saturating_sub(height);

        // Find first and last changed line across ALL content.
        let max_idx = total.max(prev_viewport_top + self.lines.len());
        let mut first_changed: Option<usize> = None;
        let mut last_changed: Option<usize> = None;
        for i in 0..max_idx {
            let old = if i >= prev_viewport_top {
                self.lines
                    .get(i - prev_viewport_top)
                    .map(|l| l.as_slice())
                    .unwrap_or(&[])
            } else {
                &[]
            };
            let new = all_lines.get(i).map(|l| l.as_slice()).unwrap_or(&[]);
            if old != new {
                if first_changed.is_none() {
                    first_changed = Some(i);
                }
                last_changed = Some(i);
            }
        }

        let Some(first) = first_changed else {
            // Nothing changed — just reposition cursor.
            let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
            self.move_to(w, cursor_screen, desired_cursor.1)?;
            w.flush()?;
            return Ok(());
        };
        let last = last_changed.unwrap_or(first);

        // Clamp first to the previous viewport — we can't render
        // above it (those rows aren't on the physical terminal).
        let render_start = first.max(prev_viewport_top);

        // Track the viewport top as it shifts during scrolling.
        let mut viewport_top = prev_viewport_top;
        let viewport_bottom = || viewport_top + height - 1;

        // Move cursor to render_start's screen row. If it's past
        // the viewport bottom, scroll first.
        if render_start > viewport_bottom() {
            let to_bottom = (height - 1).saturating_sub(self.cursor_row);
            for _ in 0..to_bottom {
                w.queue(Print("\n"))?;
            }
            let scroll = render_start - viewport_bottom();
            for _ in 0..scroll {
                w.queue(Print("\n"))?;
            }
            viewport_top += scroll;
            self.cursor_row = height - 1;
        }
        let start_screen_row = render_start - viewport_top;
        self.move_to(w, start_screen_row, 0)?;

        // Render changed lines. \r\n between lines; scrolling
        // happens naturally when the cursor is at the bottom.
        for i in render_start..=last {
            if i > render_start {
                w.queue(Print("\r\n"))?;
                let screen_row = self.cursor_row + 1;
                if screen_row >= height {
                    // The \r\n scrolled the terminal.
                    viewport_top += 1;
                    self.cursor_row = height - 1;
                } else {
                    self.cursor_row = screen_row;
                }
            }
            // Clear the line and write new content.
            w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            if let Some(line) = all_lines.get(i) {
                emit_styled_cells(w, line)?;
            }
            self.cursor_col = all_lines.get(i).map(|l| cols(l)).unwrap_or(0);
        }

        // Clear any leftover lines below if content shrunk.
        let rendered_up_to = last + 1;
        let old_end = prev_viewport_top + self.lines.len();
        if rendered_up_to < old_end {
            for _ in rendered_up_to..old_end.min(viewport_top + height) {
                w.queue(Print("\r\n"))?;
                w.queue(terminal::Clear(ClearType::UntilNewLine))?;
                if self.cursor_row + 1 < height {
                    self.cursor_row += 1;
                }
            }
        }

        // Position cursor.
        let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
        self.move_to(w, cursor_screen, desired_cursor.1)?;
        w.flush()?;

        // Update tracked state to the new visible viewport.
        self.lines = all_lines[new_viewport_top..].to_vec();
        self.cursor_row = cursor_screen;
        self.cursor_col = desired_cursor.1;

        Ok(())
    }

    /// Number of physical lines currently tracked as on-screen.
    pub fn actual_line_count(&self) -> usize {
        self.lines.len()
    }

    /// Overwrites the internal state to match what is currently on the
    /// terminal. Call after a full render to prepare for future
    /// differential updates.
    pub fn reset_to(&mut self, lines: Vec<Vec<Cell>>, cursor_row: usize, cursor_col: usize) {
        self.lines = lines;
        self.cursor_row = cursor_row;
        self.cursor_col = cursor_col;
    }

    /// Moves the terminal cursor from the current position to `(row, col)`
    /// using relative movement.
    ///
    /// Uses `\n` for downward movement (scrolls at screen bottom, creates
    /// lines) and `MoveUp` for upward movement. Column is set with
    /// `MoveToColumn` after vertical movement.
    fn move_to(&mut self, w: &mut impl Write, row: usize, col: usize) -> io::Result<()> {
        // Vertical movement.
        if row < self.cursor_row {
            w.queue(MoveUp((self.cursor_row - row) as u16))?;
        } else if row > self.cursor_row {
            // Use \r\n for downward movement:
            // - \n scrolls at the screen bottom (unlike MoveDown which silently stops)
            // - \r resets the column to 0, which is needed because \n alone preserves the
            //   column, and in pending-wrap state the column may be past the screen edge
            let down = row - self.cursor_row;
            for _ in 0..down {
                w.queue(Print("\r\n"))?;
            }
            self.cursor_col = 0;
        }

        // Horizontal movement.
        if col != self.cursor_col {
            w.queue(MoveToColumn(col as u16))?;
        }

        self.cursor_row = row;
        self.cursor_col = col;
        Ok(())
    }
}

/// Emits a sequence of styled cells to the writer.
///
/// Tracks style changes and only emits escape codes when the style
/// differs from the previous cell. Resets to default style at the end
/// if any non-default style was active.
///
/// The caller must ensure the terminal is in default style state before
/// calling this function.
pub fn emit_styled_cells(w: &mut impl Write, cells: &[Cell]) -> io::Result<()> {
    let mut current = Style::default();

    for cell in cells {
        if cell.style != current {
            // Reset to clean slate, then apply new style.
            if current != Style::default() {
                w.queue(SetAttribute(Attribute::Reset))?;
            }
            if cell.style != Style::default() {
                apply_style(w, &cell.style)?;
            }
            current = cell.style;
        }
        w.queue(Print(cell.ch))?;
    }

    // Restore default state.
    if current != Style::default() {
        w.queue(SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

/// Applies non-default style attributes (without resetting first).
fn apply_style(w: &mut impl Write, style: &Style) -> io::Result<()> {
    if let Some(fg) = style.fg {
        w.queue(SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        w.queue(SetBackgroundColor(bg))?;
    }
    if style.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if style.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }
    if style.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    Ok(())
}

/// Splits styled content into physical terminal lines based on width.
///
/// Handles newlines within spans (each newline starts a new logical
/// line) and wraps at the terminal width. Always returns at least one
/// (possibly empty) line.
///
/// By default a single trailing empty logical line — the one
/// introduced by a `\n` at the very end of the input — is collapsed
/// away. That's what static blocks (agent responses, tool output,
/// anything ending with the LLM/shell's terminating `\n`) want so
/// they don't render a phantom blank row at the bottom. Pass
/// `preserve_last_newline(true)` for the live input buffer, where a
/// buffer ending in `\n` (after Shift+Enter / Alt+Enter) needs the
/// extra row for the cursor to sit on.
#[bon::builder]
pub fn layout_lines(
    content: &StyledText,
    width: usize,
    #[builder(default = false)] preserve_last_newline: bool,
) -> Vec<Vec<Cell>> {
    let width = width.max(1);

    // Split into logical lines at newlines.
    let mut logical_lines: Vec<Vec<Cell>> = vec![Vec::new()];
    for span in content.spans() {
        for ch in span.text.chars() {
            if ch == '\n' {
                logical_lines.push(Vec::new());
            } else {
                logical_lines
                    .last_mut()
                    .expect("logical_lines always has at least one entry")
                    .push(Cell::new(ch, span.style));
            }
        }
    }

    if !preserve_last_newline
        && logical_lines.len() > 1
        && logical_lines.last().is_some_and(|l| l.is_empty())
    {
        logical_lines.pop();
    }

    // Wrap each logical line at width (measured in terminal columns,
    // not cell count — wide chars like emoji occupy 2 columns).
    let mut result: Vec<Vec<Cell>> = Vec::new();
    for line in logical_lines {
        if line.is_empty() {
            result.push(Vec::new());
        } else {
            let mut row = Vec::new();
            let mut col = 0usize;
            for cell in line {
                let w = cell.col_width();
                if col + w > width && !row.is_empty() {
                    result.push(row);
                    row = Vec::new();
                    col = 0;
                }
                row.push(cell);
                col += w;
            }
            if !row.is_empty() {
                result.push(row);
            }
        }
    }

    if result.is_empty() {
        result.push(Vec::new());
    }

    result
}

/// Lays out a [`StyledBlock`] into physical terminal lines.
///
/// Subtracts margins from `width`, wraps content to the remaining
/// space, applies alignment, and fills background. Each returned row
/// is exactly `width` cells wide.
pub fn layout_block(block: &StyledBlock, width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let ml = block.margin_left as usize;
    let mr = block.margin_right as usize;
    let content_width = width.saturating_sub(ml + mr).max(1);

    let content_lines = layout_lines()
        .content(&block.content)
        .width(content_width)
        .call();

    let fill_style = Style {
        bg: block.bg,
        ..Style::default()
    };
    let fill = Cell::new(' ', fill_style);

    content_lines
        .iter()
        .map(|line| {
            let mut row = Vec::with_capacity(width);

            // Left margin (always default bg, not block bg).
            row.extend(std::iter::repeat_n(Cell::plain(' '), ml));

            // Content with alignment (column width, not cell count).
            let cw = cols(line);
            let padding = content_width.saturating_sub(cw);
            match block.align {
                Align::Left => {
                    row.extend(line.iter().copied());
                    row.extend(std::iter::repeat_n(fill, padding));
                }
                Align::Center => {
                    let left = padding / 2;
                    let right = padding - left;
                    row.extend(std::iter::repeat_n(fill, left));
                    row.extend(line.iter().copied());
                    row.extend(std::iter::repeat_n(fill, right));
                }
            }

            // Right margin (always default bg).
            row.extend(std::iter::repeat_n(Cell::plain(' '), mr));

            // Apply block bg to content cells that don't set their own.
            // Use cell indices (not column count) to avoid
            // overrun when wide chars reduce the cell count.
            if let Some(bg) = block.bg {
                let content_end = row.len().saturating_sub(mr);
                for cell in &mut row[ml..content_end] {
                    if cell.style.bg.is_none() {
                        cell.style.bg = Some(bg);
                    }
                }
            }

            row
        })
        .collect()
}

#[cfg(test)]
mod tests;
