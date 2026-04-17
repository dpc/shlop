//! Dual-buffer diff-based screen renderer.
//!
//! Maintains an "actual" buffer representing what is currently on the
//! terminal and diffs it against a "desired" buffer to emit only the
//! escape sequences needed to update changed characters. This minimizes
//! terminal I/O, which matters over slow SSH connections.
//!
//! Approach borrowed from fish shell's `screen.rs`:
//! <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//!
//! Key differences from fish:
//! - We don't track styling/attributes (yet), just character content.
//! - We use a simpler line model (Vec<Vec<char>>) instead of fish's
//!   Line struct with soft-wrap tracking.
//! - We always use relative cursor movement (MoveUp, `\r`, `\n`,
//!   MoveToColumn) — never absolute positioning.
//!
//! Downward movement uses `\n` rather than `MoveDown` because `\n`
//! scrolls the terminal when at the bottom of the screen and creates
//! new physical lines, while `MoveDown` silently stops at the edge.

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::QueueableCommand;

/// Virtual screen state with diff-based updates.
pub struct Screen {
    /// What we believe is currently displayed on the terminal.
    lines: Vec<Vec<char>>,
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

    /// Diffs the desired content against the actual screen state and emits
    /// only the escape sequences needed to make the terminal match.
    ///
    /// `desired_lines` is the content split into physical rows.
    /// `desired_cursor` is `(row, col)` where the cursor should end up.
    pub fn update(
        &mut self,
        w: &mut impl Write,
        desired_lines: &[Vec<char>],
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

        for row in 0..desired_count {
            let actual_line = self.lines.get(row);
            let actual_slice = actual_line.map(|l| l.as_slice()).unwrap_or(&[]);
            let desired_slice = desired_lines[row].as_slice();

            // Find the first column where actual and desired differ.
            let common_prefix = actual_slice
                .iter()
                .zip(desired_slice.iter())
                .take_while(|(a, d)| a == d)
                .count();

            let is_last_desired = row == desired_count - 1;
            let actual_longer = actual_slice.len() > desired_slice.len();
            let has_extra_actual_below = is_last_desired && self.lines.len() > desired_count;

            // Skip if this line is completely unchanged and we don't need
            // to clear below.
            if common_prefix == actual_slice.len()
                && common_prefix == desired_slice.len()
                && !has_extra_actual_below
            {
                continue;
            }

            // Move to the first changed column on this row.
            self.move_to(w, row, common_prefix)?;

            // Print the new content from the first difference onward.
            if common_prefix < desired_slice.len() {
                let new_content: String = desired_slice[common_prefix..].iter().collect();
                w.queue(Print(new_content))?;
                // layout_lines guarantees each line is at most `width`
                // chars. At exactly `width`, the terminal enters a
                // "pending wrap" state — the cursor is still on the
                // current row at column `width`, not yet on the next
                // row. We track this accurately so move_to computes
                // correct relative movement.
                self.cursor_col = desired_slice.len();
            }

            // Clear trailing characters / lines below as needed.
            if has_extra_actual_below {
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_longer {
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

    /// Number of physical lines currently tracked as on-screen.
    pub fn actual_line_count(&self) -> usize {
        self.lines.len()
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
            // - \n scrolls at the screen bottom (unlike MoveDown which
            //   silently stops)
            // - \r resets the column to 0, which is needed because \n
            //   alone preserves the column, and in pending-wrap state
            //   the column may be past the screen edge
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

/// Splits content into physical terminal lines based on width.
///
/// Always returns at least one (possibly empty) line.
pub fn layout_lines(content: &str, width: usize) -> Vec<Vec<char>> {
    let width = width.max(1);
    let chars: Vec<char> = content.chars().collect();
    if chars.is_empty() {
        return vec![Vec::new()];
    }
    chars.chunks(width).map(|chunk| chunk.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test harness: pairs our `Screen` with a `vt100::Parser` acting as
    /// a headless terminal emulator. We feed our escape-sequence output
    /// into vt100 and assert on the resulting screen state.
    struct TestTerm {
        screen: Screen,
        term: vt100::Parser,
    }

    impl TestTerm {
        fn new(rows: u16, cols: u16) -> Self {
            Self {
                screen: Screen::new(cols as usize),
                term: vt100::Parser::new(rows, cols, 0),
            }
        }

        /// Builds desired layout from content, feeds the diff output into
        /// the terminal emulator.
        fn render(&mut self, content: &str, cursor_char_offset: usize) {
            let width = self.screen.width();
            let desired = layout_lines(content, width);
            let cursor = (cursor_char_offset / width, cursor_char_offset % width);
            let mut buf = Vec::new();
            self.screen
                .update(&mut buf, &desired, cursor)
                .expect("update should succeed");
            self.term.process(&buf);
        }

        /// Invalidates the screen (as async output would) and re-renders.
        fn invalidate_and_render(&mut self, content: &str, cursor_char_offset: usize) {
            let mut buf = Vec::new();
            self.screen
                .erase_all(&mut buf)
                .expect("erase should succeed");
            self.screen.invalidate();
            self.term.process(&buf);
            self.render(content, cursor_char_offset);
        }

        /// Returns the text on a given terminal row (trimmed of trailing
        /// whitespace).
        fn row_text(&self, row: usize) -> String {
            self.term
                .screen()
                .rows(0, self.term.screen().size().1)
                .nth(row)
                .unwrap_or_default()
        }

        /// Returns the cursor position as (row, col).
        fn cursor(&self) -> (u16, u16) {
            self.term.screen().cursor_position()
        }
    }

    // --- layout tests ---

    #[test]
    fn layout_empty_produces_one_empty_line() {
        assert_eq!(layout_lines("", 80), vec![Vec::<char>::new()]);
    }

    #[test]
    fn layout_short_produces_one_line() {
        assert_eq!(layout_lines("abc", 80), vec![vec!['a', 'b', 'c']]);
    }

    #[test]
    fn layout_wraps_at_width() {
        let result = layout_lines("abcde", 3);
        assert_eq!(result, vec![vec!['a', 'b', 'c'], vec!['d', 'e']]);
    }

    #[test]
    fn layout_exact_width_is_one_line() {
        assert_eq!(layout_lines("abc", 3), vec![vec!['a', 'b', 'c']]);
    }

    // --- screen rendering tests (using vt100 as a headless terminal) ---

    #[test]
    fn first_render_shows_prompt() {
        let mut t = TestTerm::new(24, 80);
        t.render("> hello", 7);
        assert_eq!(t.row_text(0), "> hello");
        assert_eq!(t.cursor(), (0, 7));
    }

    #[test]
    fn appending_one_char_updates_correctly() {
        let mut t = TestTerm::new(24, 80);
        t.render("> hell", 6);
        assert_eq!(t.row_text(0), "> hell");

        t.render("> hello", 7);
        assert_eq!(t.row_text(0), "> hello");
        assert_eq!(t.cursor(), (0, 7));
    }

    #[test]
    fn cursor_moves_without_changing_content() {
        let mut t = TestTerm::new(24, 80);
        t.render("> hello", 7);

        // Move cursor to position 2 (after "> ").
        t.render("> hello", 2);
        assert_eq!(t.row_text(0), "> hello");
        assert_eq!(t.cursor(), (0, 2));
    }

    #[test]
    fn shrinking_clears_old_text() {
        let mut t = TestTerm::new(24, 80);
        t.render("> hello world", 13);
        assert_eq!(t.row_text(0), "> hello world");

        t.render("> hi", 4);
        assert_eq!(t.row_text(0), "> hi");
        assert_eq!(t.cursor(), (0, 4));
    }

    #[test]
    fn wrapping_to_second_line() {
        let mut t = TestTerm::new(24, 10);
        // 12 chars total, wraps at column 10.
        t.render("> abcdefghij", 12);
        assert_eq!(t.row_text(0), "> abcdefgh");
        assert_eq!(t.row_text(1), "ij");
        assert_eq!(t.cursor(), (1, 2));
    }

    #[test]
    fn removing_wrapped_line_clears_it() {
        let mut t = TestTerm::new(24, 10);
        t.render("> abcdefghij", 12);
        assert_eq!(t.row_text(1), "ij");

        t.render("> ab", 4);
        assert_eq!(t.row_text(0), "> ab");
        assert_eq!(t.row_text(1), "");
        assert_eq!(t.cursor(), (0, 4));
    }

    #[test]
    fn invalidate_and_rerender_after_async_output() {
        let mut t = TestTerm::new(24, 80);
        t.render("> hello", 7);
        assert_eq!(t.row_text(0), "> hello");

        // Simulate async output clearing the prompt area.
        t.invalidate_and_render("> hello", 7);
        assert_eq!(t.row_text(0), "> hello");
        assert_eq!(t.cursor(), (0, 7));
    }

    #[test]
    fn growing_from_one_to_two_lines() {
        let mut t = TestTerm::new(24, 10);
        t.render("> abcdefg", 9);
        assert_eq!(t.row_text(0), "> abcdefg");
        assert_eq!(t.row_text(1), "");

        // Add one more char, fills the line exactly.
        t.render("> abcdefgh", 10);
        assert_eq!(t.row_text(0), "> abcdefgh");
        // Cursor offset 10 / width 10 = row 1, col 0 (start of next line).
        assert_eq!(t.cursor(), (1, 0));

        // One more.
        t.render("> abcdefghi", 11);
        assert_eq!(t.row_text(0), "> abcdefgh");
        assert_eq!(t.row_text(1), "i");
        assert_eq!(t.cursor(), (1, 1));
    }

    #[test]
    fn cursor_in_middle_of_wrapped_content() {
        let mut t = TestTerm::new(24, 10);
        // 15 chars, cursor at position 5.
        t.render("> abcdefghijklm", 5);
        assert_eq!(t.row_text(0), "> abcdefgh");
        assert_eq!(t.row_text(1), "ijklm");
        assert_eq!(t.cursor(), (0, 5));
    }

    // --- multi-zone prompt tests ---

    /// Helper to build a multi-zone layout: above-prompt lines, then
    /// input line(s) with optional right-prompt on the first input line.
    fn build_prompt_layout(
        above: &str,
        left: &str,
        input: &str,
        right: &str,
        width: usize,
    ) -> (Vec<Vec<char>>, (usize, usize)) {
        let mut desired: Vec<Vec<char>> = Vec::new();
        let above_row_count;

        if above.is_empty() {
            above_row_count = 0;
        } else {
            for line in above.lines() {
                desired.extend(layout_lines(line, width));
            }
            above_row_count = desired.len();
        }

        let content = format!("{left}{input}");
        let mut input_lines = layout_lines(&content, width);

        // Right prompt on first input line if it fits and input is single-line.
        if !right.is_empty() && !input_lines.is_empty() {
            let first = &input_lines[0];
            let right_chars: Vec<char> = right.chars().collect();
            let needed = first.len() + 1 + right_chars.len();
            if needed <= width && input_lines.len() == 1 {
                let padding = width - first.len() - right_chars.len();
                let mut padded = first.clone();
                padded.extend(std::iter::repeat(' ').take(padding));
                padded.extend(right_chars);
                input_lines[0] = padded;
            }
        }

        desired.extend(input_lines);

        let cursor_chars = left.chars().count() + input.chars().count();
        let cursor_row = above_row_count + cursor_chars / width;
        let cursor_col = cursor_chars % width;

        (desired, (cursor_row, cursor_col))
    }

    #[test]
    fn above_prompt_renders_before_input() {
        let mut t = TestTerm::new(24, 40);
        let (lines, cursor) = build_prompt_layout("status line", "> ", "hello", "", 40);
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        assert_eq!(t.row_text(0), "status line");
        assert_eq!(t.row_text(1), "> hello");
        assert_eq!(t.cursor(), (1, 7));
    }

    #[test]
    fn multi_line_above_prompt() {
        let mut t = TestTerm::new(24, 40);
        let (lines, cursor) = build_prompt_layout("line one\nline two", "> ", "hi", "", 40);
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        assert_eq!(t.row_text(0), "line one");
        assert_eq!(t.row_text(1), "line two");
        assert_eq!(t.row_text(2), "> hi");
        assert_eq!(t.cursor(), (2, 4));
    }

    #[test]
    fn right_prompt_shown_when_space_available() {
        let mut t = TestTerm::new(24, 40);
        let (lines, cursor) = build_prompt_layout("", "> ", "hi", "[ok]", 40);
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        let row = t.row_text(0);
        assert!(row.starts_with("> hi"), "row: {row:?}");
        assert!(row.ends_with("[ok]"), "row: {row:?}");
        assert_eq!(row.len(), 40);
    }

    #[test]
    fn right_prompt_hidden_when_input_too_long() {
        let mut t = TestTerm::new(24, 20);
        // "> " (2) + 15 chars + 1 gap + "[ok]" (4) = 22 > 20.
        let (lines, cursor) = build_prompt_layout(
            "",
            "> ",
            "abcdefghijklmno",
            "[ok]",
            20,
        );
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        let row = t.row_text(0);
        assert!(!row.contains("[ok]"), "right prompt should be hidden, row: {row:?}");
        assert!(row.starts_with("> abcdefghijklmno"), "row: {row:?}");
    }

    #[test]
    fn right_prompt_hidden_when_input_wraps() {
        let mut t = TestTerm::new(24, 10);
        // Input wraps to second line — right prompt should not appear.
        let (lines, cursor) = build_prompt_layout("", "> ", "abcdefghij", "[x]", 10);
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        let row0 = t.row_text(0);
        let row1 = t.row_text(1);
        assert!(!row0.contains("[x]"), "row0: {row0:?}");
        assert_eq!(row1, "ij");
    }

    #[test]
    fn all_three_zones_together() {
        let mut t = TestTerm::new(24, 40);
        let (lines, cursor) = build_prompt_layout(
            "shlop v0.1",
            "$ ",
            "ls",
            "[main]",
            40,
        );
        let mut buf = Vec::new();
        t.screen.update(&mut buf, &lines, cursor).expect("ok");
        t.term.process(&buf);

        assert_eq!(t.row_text(0), "shlop v0.1");
        let prompt_row = t.row_text(1);
        assert!(prompt_row.starts_with("$ ls"), "row: {prompt_row:?}");
        assert!(prompt_row.ends_with("[main]"), "row: {prompt_row:?}");
        assert_eq!(t.cursor(), (1, 4));
    }
}
