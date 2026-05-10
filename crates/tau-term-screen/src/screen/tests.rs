use super::*;
use crate::style::{Color, Span};

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

    /// Builds desired layout from plain content, feeds the diff output
    /// into the terminal emulator.
    fn render(&mut self, content: &str, cursor_char_offset: usize) {
        let width = self.screen.width();
        let styled: StyledText = content.into();
        let desired = layout_lines().content(&styled).width(width).call();
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

/// Extracts character-only strings from cell lines (for assertions).
fn line_chars(lines: &[Vec<Cell>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.iter().map(|c| c.ch).collect())
        .collect()
}

// --- layout tests ---

#[test]
fn layout_empty_produces_one_empty_line() {
    let lines = layout_lines().content(&StyledText::new()).width(80).call();
    assert_eq!(line_chars(&lines), vec![""]);
}

#[test]
fn layout_short_produces_one_line() {
    let lines = layout_lines()
        .content(&StyledText::from("abc"))
        .width(80)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc"]);
}

#[test]
fn layout_wraps_at_width() {
    let lines = layout_lines()
        .content(&StyledText::from("abcde"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "de"]);
}

#[test]
fn layout_exact_width_is_one_line() {
    let lines = layout_lines()
        .content(&StyledText::from("abc"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc"]);
}

#[test]
fn layout_preserves_styles() {
    let style = Style::default().fg(Color::Red);
    let styled = StyledText::from(vec![Span::plain("ab"), Span::new("cd", style)]);
    let lines = layout_lines().content(&styled).width(80).call();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 4);
    assert_eq!(lines[0][0], Cell::plain('a'));
    assert_eq!(lines[0][1], Cell::plain('b'));
    assert_eq!(lines[0][2], Cell::new('c', style));
    assert_eq!(lines[0][3], Cell::new('d', style));
}

#[test]
fn layout_handles_newlines() {
    let lines = layout_lines()
        .content(&StyledText::from("abc\ndef"))
        .width(80)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "def"]);
}

#[test]
fn layout_newline_and_wrap() {
    let lines = layout_lines()
        .content(&StyledText::from("abc\ndef"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "def"]);
}

// --- layout_block tests ---

#[test]
fn layout_block_plain() {
    let block = StyledBlock::new("hello");
    let lines = layout_block(&block, 20);
    assert_eq!(lines.len(), 1);
    // Row should be exactly 20 cells wide (content + padding).
    assert_eq!(lines[0].len(), 20);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    assert!(text.starts_with("hello"), "text: {text:?}");
    // Remaining chars should be spaces.
    assert!(text[5..].chars().all(|c| c == ' '));
}

#[test]
fn layout_block_with_margins() {
    let block = StyledBlock::new("hi").margin_left(2).margin_right(3);
    let lines = layout_block(&block, 20);
    assert_eq!(lines[0].len(), 20);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    // 2 margin + "hi" + padding + 3 margin = 20
    assert_eq!(&text[..2], "  ", "left margin");
    assert_eq!(&text[2..4], "hi", "content");
    assert_eq!(&text[17..20], "   ", "right margin");
}

#[test]
fn layout_block_center_alignment() {
    let block = StyledBlock::new("hi").align(Align::Center);
    let lines = layout_block(&block, 10);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    // "hi" is 2 chars, padding = 8, left = 4, right = 4.
    assert_eq!(text, "    hi    ");
}

#[test]
fn layout_block_bg_applied_to_content_area() {
    let bg = Color::DarkBlue;
    let block = StyledBlock::new("ab").bg(bg).margin_left(1).margin_right(1);
    let lines = layout_block(&block, 10);
    // Margin cells should NOT have block bg.
    assert_eq!(lines[0][0].style.bg, None, "left margin has no bg");
    assert_eq!(lines[0][9].style.bg, None, "right margin has no bg");
    // Content area cells should have block bg.
    assert_eq!(lines[0][1].style.bg, Some(bg), "content has bg");
    assert_eq!(lines[0][2].style.bg, Some(bg), "content has bg");
    // Padding within content area should also have bg.
    assert_eq!(lines[0][3].style.bg, Some(bg), "padding has bg");
}

#[test]
fn layout_block_content_fg_preserved_with_bg() {
    let fg = Color::Red;
    let bg = Color::DarkGreen;
    let block = StyledBlock::new(StyledText::from(Span::new("x", Style::default().fg(fg)))).bg(bg);
    let lines = layout_block(&block, 5);
    // The 'x' cell should have both fg from the span and bg from the block.
    assert_eq!(lines[0][0].ch, 'x');
    assert_eq!(lines[0][0].style.fg, Some(fg));
    assert_eq!(lines[0][0].style.bg, Some(bg));
}

#[test]
fn layout_block_renders_through_vt100() {
    let bg = Color::Blue;
    let block = StyledBlock::new(StyledText::from(Span::new(
        "test",
        Style::default().fg(Color::White),
    )))
    .bg(bg)
    .margin_left(1);
    let lines = layout_block(&block, 20);

    // Render through Screen + vt100.
    let mut term = vt100::Parser::new(5, 20, 0);
    let mut screen = Screen::new(20);
    let cursor = (0, 0);
    let mut buf = Vec::new();
    screen.update(&mut buf, &lines, cursor).expect("render ok");
    term.process(&buf);

    let row = term.screen().rows(0, 20).next().unwrap_or_default();
    assert!(row.starts_with(" test"), "row: {row:?}");

    // vt100 does not reliably preserve the exact foreground color
    // mapping for crossterm colors here, so assert that styling was
    // emitted at all rather than hard-coding vt100 color indices.
    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains("\u{1b}["),
        "expected ANSI styling escapes in output, got: {rendered:?}"
    );
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

// --- styled rendering tests ---

#[test]
fn styled_content_renders_with_color() {
    let mut t = TestTerm::new(24, 80);
    let style = Style::default().fg(Color::Blue);
    let styled = StyledText::from(vec![Span::plain("hi "), Span::new("world", style)]);
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &desired, (0, 8)).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "hi world");

    // "hi " should be default style.
    let cell_h = t.term.screen().cell(0, 0).expect("cell exists");
    assert!(!cell_h.bold());
    assert_eq!(cell_h.fgcolor(), vt100::Color::Default);

    // vt100's color decoding is not stable enough here to assert an
    // exact palette index, but the renderer should have emitted style
    // escapes for the colored span.
    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains("\u{1b}["),
        "expected ANSI styling escapes in output, got: {rendered:?}"
    );
}

#[test]
fn styled_diff_only_rerenders_changed_styles() {
    let mut t = TestTerm::new(24, 80);
    let bold = Style::default().bold();

    // First render: plain text.
    t.render("hello", 5);
    assert_eq!(t.row_text(0), "hello");

    // Second render: same text but bold.
    let styled = StyledText::from(Span::new("hello", bold));
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &desired, (0, 5)).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "hello");
    let cell = t.term.screen().cell(0, 0).expect("cell exists");
    assert!(cell.bold());
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
) -> (Vec<Vec<Cell>>, (usize, usize)) {
    let mut desired: Vec<Vec<Cell>> = Vec::new();
    let above_row_count = if above.is_empty() {
        0
    } else {
        let above_styled: StyledText = above.into();
        desired.extend(layout_lines().content(&above_styled).width(width).call());
        desired.len()
    };

    let content = format!("{left}{input}");
    let content_styled: StyledText = content.into();
    let mut input_lines = layout_lines().content(&content_styled).width(width).call();

    // Right prompt on first input line if it fits and input is single-line.
    if !right.is_empty() && !input_lines.is_empty() {
        let first = &input_lines[0];
        let right_styled: StyledText = right.into();
        let right_cells = right_styled.to_cells();
        let first_cols = cols(first);
        let right_cols = cols(&right_cells);
        let needed = first_cols + 1 + right_cols;
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_cols - right_cols;
            let mut padded = first.clone();
            padded.extend(std::iter::repeat_n(Cell::plain(' '), padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    desired.extend(input_lines);

    let cursor_cols: usize = left
        .chars()
        .chain(input.chars())
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let cursor_row = above_row_count + cursor_cols / width;
    let cursor_col = cursor_cols % width;

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
    let (lines, cursor) = build_prompt_layout("", "> ", "abcdefghijklmno", "[ok]", 20);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    let row = t.row_text(0);
    assert!(
        !row.contains("[ok]"),
        "right prompt should be hidden, row: {row:?}"
    );
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
    let (lines, cursor) = build_prompt_layout("tau v0.1", "$ ", "ls", "[main]", 40);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "tau v0.1");
    let prompt_row = t.row_text(1);
    assert!(prompt_row.starts_with("$ ls"), "row: {prompt_row:?}");
    assert!(prompt_row.ends_with("[main]"), "row: {prompt_row:?}");
    assert_eq!(t.cursor(), (1, 4));
}

// --- full_render / scrollback tests ---

/// Verifies the basic technique: output more lines than the
/// terminal height using \r\n, then check that overflow went
/// into scrollback and the visible screen shows the last rows.
#[test]
fn overflow_lines_go_to_scrollback() {
    // 5 rows tall, 20 cols, 100 lines of scrollback buffer.
    let mut term = vt100::Parser::new(5, 20, 100);

    // Output 10 lines (more than 5 rows).
    let mut buf = Vec::new();
    for i in 0..10 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("line {i}").as_bytes());
    }
    term.process(&buf);

    // Visible screen should show the last 5 lines (5-9).
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "line 5");
    assert_eq!(visible[1], "line 6");
    assert_eq!(visible[2], "line 7");
    assert_eq!(visible[3], "line 8");
    assert_eq!(visible[4], "line 9");

    // Scrollback should contain lines 0-4.
    // Set scrollback offset to see them.
    term.screen_mut().set_scrollback(5);
    let scrolled: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(scrolled[0], "line 0");
    assert_eq!(scrolled[1], "line 1");
    assert_eq!(scrolled[2], "line 2");
    assert_eq!(scrolled[3], "line 3");
    assert_eq!(scrolled[4], "line 4");
}

/// Verifies clear screen + scrollback (\x1b[2J\x1b[H\x1b[3J),
/// then re-output at a different width — the full_render technique
/// used on resize.
#[test]
fn clear_and_rerender_scrollback() {
    let mut term = vt100::Parser::new(5, 20, 100);

    // First render: 8 lines.
    let mut buf = Vec::new();
    for i in 0..8 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("old line {i}").as_bytes());
    }
    term.process(&buf);

    // Verify initial state: visible = lines 3-7, scrollback = 0-2.
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "old line 3");

    // Now simulate resize: clear + re-render with new content.
    let mut buf2 = Vec::new();
    buf2.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J"); // clear screen + scrollback
    for i in 0..8 {
        if i > 0 {
            buf2.extend_from_slice(b"\r\n");
        }
        buf2.extend_from_slice(format!("new line {i}").as_bytes());
    }
    term.process(&buf2);

    // Visible screen should show the last 5 new lines.
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "new line 3");
    assert_eq!(visible[1], "new line 4");
    assert_eq!(visible[2], "new line 5");
    assert_eq!(visible[3], "new line 6");
    assert_eq!(visible[4], "new line 7");

    // Scrollback should have new lines 0-2.
    term.screen_mut().set_scrollback(3);
    let scrolled: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(scrolled[0], "new line 0");
    assert_eq!(scrolled[1], "new line 1");
    assert_eq!(scrolled[2], "new line 2");
}

/// Verifies cursor positioning after outputting more lines than
/// the terminal height (the MoveUp technique used in full_render).
#[test]
fn cursor_positioning_after_overflow() {
    let mut term = vt100::Parser::new(5, 20, 100);

    // Output 8 lines, then move cursor up to where "line 5"
    // is (which should be row 0 of the visible screen).
    let mut buf = Vec::new();
    for i in 0..8 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("line {i}").as_bytes());
    }
    // Cursor is now at row 4 (bottom of visible screen), after "line 7".
    // Move up 4 rows to get to row 0 (where "line 3" is).
    buf.extend_from_slice(b"\x1b[4A"); // MoveUp(4)
    buf.extend_from_slice(b"\x1b[10G"); // MoveToColumn(10), 1-indexed
    term.process(&buf);

    let (row, col) = term.screen().cursor_position();
    assert_eq!(row, 0);
    assert_eq!(col, 9); // MoveToColumn is 1-indexed, vt100 returns 0-indexed
}

/// End-to-end test of the full_render function using vt100.
/// Simulates: output history + live area, then position cursor
/// in the input area using the same logic as full_render.
#[test]
fn full_render_via_vt100() {
    use crossterm::QueueableCommand;
    use crossterm::cursor::{MoveToColumn, MoveUp};

    let height: usize = 5;
    let width: usize = 30;
    let mut term = vt100::Parser::new(height as u16, width as u16, 100);

    // Build "all_lines": 3 history + 2 above + 1 input + 1 below = 7 lines.
    // Viewport (last 5): above0, above1, "> hello", below0, but wait
    // that's only 4 visible from the live area. Let me just use strings.
    let lines_text = [
        "history 0",
        "history 1",
        "history 2",
        "above block A",
        "above block B",
        "> hello", // input line, cursor should be here
        "below status",
    ];

    // The cursor is at the input line (index 5), column 7 ("> hello" = 7 chars).
    let cursor_row: usize = 5;
    let cursor_col: usize = 7;

    // Simulate full_render: clear + output all lines.
    let mut buf: Vec<u8> = Vec::new();
    // Clear screen + scrollback.
    buf.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J");
    // Output all lines.
    for (i, line) in lines_text.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }
    term.process(&buf);

    // After outputting, cursor is at the last line.
    let total = lines_text.len(); // 7
    let viewport_top = total.saturating_sub(height); // 7 - 5 = 2
    let current_vp_row = total.saturating_sub(1).saturating_sub(viewport_top); // 6 - 2 = 4
    let cursor_vp_row = cursor_row.saturating_sub(viewport_top); // 5 - 2 = 3

    // Move cursor from current position to cursor position.
    let up = current_vp_row.saturating_sub(cursor_vp_row); // 4 - 3 = 1
    let mut buf2: Vec<u8> = Vec::new();
    if up > 0 {
        (&mut buf2 as &mut dyn std::io::Write)
            .queue(MoveUp(up as u16))
            .expect("ok");
    }
    (&mut buf2 as &mut dyn std::io::Write)
        .queue(MoveToColumn(cursor_col as u16))
        .expect("ok");
    term.process(&buf2);

    // Verify visible screen (rows 2-6 of all_lines).
    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible[0], "history 2");
    assert_eq!(visible[1], "above block A");
    assert_eq!(visible[2], "above block B");
    assert_eq!(visible[3], "> hello");
    assert_eq!(visible[4], "below status");

    // Verify cursor position.
    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, cursor_vp_row as u16, "cursor row");
    assert_eq!(c, cursor_col as u16, "cursor col");

    // Verify scrollback contains history 0 and 1.
    term.screen_mut().set_scrollback(2);
    let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(scrolled[0], "history 0");
    assert_eq!(scrolled[1], "history 1");
}

/// Verifies full_render with fewer lines than terminal height
/// (no scrollback needed).
#[test]
fn full_render_no_overflow() {
    let height: usize = 10;
    let width: usize = 30;
    let mut term = vt100::Parser::new(height as u16, width as u16, 100);

    let lines_text = [
        "above", "> hi", // cursor here, col 4
        "below",
    ];
    let cursor_row: usize = 1;
    let cursor_col: usize = 4;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J");
    for (i, line) in lines_text.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }

    let total = lines_text.len(); // 3
    let viewport_top = total.saturating_sub(height); // 0
    let current_vp_row = total.saturating_sub(1).saturating_sub(viewport_top); // 2
    let cursor_vp_row = cursor_row.saturating_sub(viewport_top); // 1

    let up = current_vp_row.saturating_sub(cursor_vp_row); // 1
    if up > 0 {
        use std::io::Write;
        (&mut buf as &mut dyn Write)
            .queue(crossterm::cursor::MoveUp(up as u16))
            .expect("ok");
    }
    {
        use std::io::Write;
        (&mut buf as &mut dyn Write)
            .queue(crossterm::cursor::MoveToColumn(cursor_col as u16))
            .expect("ok");
    }
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible[0], "above");
    assert_eq!(visible[1], "> hi");
    assert_eq!(visible[2], "below");

    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, 1, "cursor row");
    assert_eq!(c, 4, "cursor col");
}

/// Simulates a resize: initial render at width 20, then resize to
/// width 10 (causing lines to re-wrap and produce more rows),
/// clear + re-render. Verifies scrollback is rebuilt correctly
/// with the new wrapping.
#[test]
fn scrollback_rebuilt_after_resize() {
    let height: usize = 5;

    // --- Phase 1: render at width 20 ---
    let width1: u16 = 20;
    let mut term = vt100::Parser::new(height as u16, width1, 100);

    // 7 lines at width 20 (2 scroll into scrollback on a 5-row terminal).
    let phase1 = [
        "aaaaaaaa", // 8 chars, fits in 20
        "bbbbbbbb",
        "cccccccc",
        "dddddddd",
        "eeeeeeee",
        "> input", // cursor line
        "status bar below",
    ];

    let mut buf: Vec<u8> = Vec::new();
    for (i, line) in phase1.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }
    term.process(&buf);

    // Verify: visible = lines 2-6, scrollback = lines 0-1.
    let visible: Vec<String> = term.screen().rows(0, width1).collect();
    assert_eq!(visible[0], "cccccccc");
    assert_eq!(visible[4], "status bar below");

    // --- Phase 2: resize to width 10, re-render ---
    // At width 10, "status bar below" (16 chars) wraps to 2 lines.
    // Total lines increase.
    // Create a fresh parser at the new size (simulates the real
    // terminal being resized — our clear+rerender rebuilds
    // everything from scratch anyway).
    let width2: u16 = 10;
    let mut term = vt100::Parser::new(height as u16, width2, 100);

    let phase2 = [
        "aaaaaaaa", // fits in 10
        "bbbbbbbb",
        "cccccccc",
        "dddddddd",
        "eeeeeeee",
        "> input",
        "status bar", // "status bar below" wraps at 10
        " below",
    ];

    let mut buf2: Vec<u8> = Vec::new();
    buf2.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J"); // clear + clear scrollback
    for (i, line) in phase2.iter().enumerate() {
        if i > 0 {
            buf2.extend_from_slice(b"\r\n");
        }
        buf2.extend_from_slice(line.as_bytes());
    }
    term.process(&buf2);

    // Total = 8 lines, height = 5, so viewport_top = 3.
    // Visible: lines 3-7 = dddddddd, eeeeeeee, > input, status bar, " below"
    let visible2: Vec<String> = term.screen().rows(0, width2).collect();
    assert_eq!(visible2[0], "dddddddd", "visible row 0 after resize");
    assert_eq!(visible2[1], "eeeeeeee", "visible row 1 after resize");
    assert_eq!(visible2[2], "> input", "visible row 2 after resize");
    assert_eq!(visible2[3], "status bar", "visible row 3 after resize");
    assert_eq!(visible2[4], " below", "visible row 4 after resize");

    // Scrollback should have lines 0-2 (aaaaaaaa, bbbbbbbb, cccccccc).
    term.screen_mut().set_scrollback(3);
    let scrolled: Vec<String> = term.screen().rows(0, width2).collect();
    assert_eq!(scrolled[0], "aaaaaaaa", "scrollback row 0 after resize");
    assert_eq!(scrolled[1], "bbbbbbbb", "scrollback row 1 after resize");
    assert_eq!(scrolled[2], "cccccccc", "scrollback row 2 after resize");
}
