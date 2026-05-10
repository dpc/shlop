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

    // Screen tracks the visible viewport (5 lines).
    assert_eq!(
        screen.actual_line_count(),
        5,
        "screen tracks visible viewport"
    );
}

// --- full_render: content shorter than terminal ---

#[test]
fn cursor_shape_maps_to_steady_styles() {
    assert_eq!(
        CursorShape::Bar.crossterm_style().to_string(),
        crossterm::cursor::SetCursorStyle::SteadyBar.to_string()
    );
    assert_eq!(
        CursorShape::Block.crossterm_style().to_string(),
        crossterm::cursor::SetCursorStyle::SteadyBlock.to_string()
    );
}

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
    for (i, row) in vis.iter().enumerate().take(10).skip(3) {
        assert_eq!(row, "", "row {i} should be blank");
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

    // Screen tracks the visible viewport (3 lines).
    assert_eq!(
        screen.actual_line_count(),
        3,
        "screen tracks visible viewport"
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

    // Screen tracks the visible viewport (5 lines).
    assert_eq!(screen.actual_line_count(), 5);
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

    // Screen should track 3 lines (visible viewport).
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
fn input_history_navigates_submitted_and_draft_entries() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("first draft".to_owned(), "first draft".len());
    flush_redraws(&handle, &buf, &mut parser);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "first draft");

    handle.set_buffer("one".to_owned(), 3);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "one"
    ));

    handle.set_buffer("two".to_owned(), 3);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "two"
    ));

    handle.set_buffer("draft".to_owned(), 5);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "two");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "one");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "two");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft");
}

#[test]
fn down_from_non_empty_draft_creates_fresh_prompt_and_history_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("draft".to_owned(), 5);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft");
    assert_eq!(handle.get_cursor(), 5);
}

#[test]
fn down_at_wip_slot_in_nav_mode_pushes_and_resets() {
    // Repro: after a Down has pushed once, navigating Up then
    // editing the WIP slot and pressing Down should push again.
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("first".to_owned(), 5);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "first");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");

    for ch in "second".chars() {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )))
            .expect("send char");
        let _ = term.get_next_event().expect("event");
    }
    assert_eq!(handle.get_buffer(), "second");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "second");
}

#[test]
fn down_from_empty_prompt_does_not_create_history_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);
}

#[test]
fn vertical_motion_stays_within_multiline_buffer_before_history() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(10, 5, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abc\ndef".to_owned(), "abc\ndef".len());

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "abc\ndef");
    assert_eq!(handle.get_cursor(), 1);
}

#[test]
fn full_render_then_diff_with_history() {
    // 3 history + 2 live = 5, 5-row terminal.
    // Screen tracks all 5 visible lines. A diff update that
    // changes only the live portion should produce minimal output.
    let lines = plain_lines(&["h0", "h1", "h2", "> cmd", "status"]);
    let (_term, mut screen) = run_full_render(5, 30, lines, 3, 3, 5);

    assert_eq!(screen.actual_line_count(), 5, "visible viewport tracked");

    // Update: change "> cmd" to "> new" (history unchanged).
    let visible2 = plain_lines(&["h0", "h1", "h2", "> new", "status"]);
    let mut buf2: Vec<u8> = Vec::new();
    screen.update(&mut buf2, &visible2, (3, 5)).expect("ok");

    assert!(!buf2.is_empty());
}

// --- Virtual terminal E2E tests ---

/// Shared buffer that implements Write for the redraw thread
/// and can be drained into a vt100 parser by the test.
#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    /// Drain accumulated bytes into a vt100 parser.
    fn drain_into(&self, parser: &mut vt100::Parser) {
        let mut buf = self.0.lock().expect("shared buffer poisoned");
        if !buf.is_empty() {
            parser.process(&buf);
            buf.clear();
        }
    }
}

impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("shared buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Helper: get visible rows from a vt100 parser as trimmed strings.
fn vt100_rows(parser: &vt100::Parser, cols: u16) -> Vec<String> {
    parser.screen().rows(0, cols).collect()
}

/// Helper: check if any visible row contains the given text.
fn screen_contains(parser: &vt100::Parser, cols: u16, text: &str) -> bool {
    vt100_rows(parser, cols).iter().any(|r| r.contains(text))
}

/// Helper: trigger a sync redraw and drain output into the parser.
fn flush_redraws(handle: &TermHandle, buf: &SharedBuffer, parser: &mut vt100::Parser) {
    handle.redraw_sync();
    buf.drain_into(parser);
}

#[test]
fn multiline_buffer_layout_tracks_cursor_after_paste() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 10, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(10, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("abc\ndefghijkl".to_owned(), "abc\ndefghijkl".len());
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(
        vt100_rows(&parser, 10),
        vec!["> abc", "defghijkl", "", "", ""]
    );
    assert_eq!(parser.screen().cursor_position(), (1, 9));
}

#[test]
fn multiline_buffer_vertical_cursor_motion_uses_visual_lines() {
    let width = 10;
    let left_cols = 2;
    let text = "abc\ndefghijkl";

    let (row, col) = buffer_position_for_byte(text, text.len(), width, left_cols);
    assert_eq!((row, col), (1, 9));

    let up = byte_offset_for_buffer_position(text, 0, 5, width, left_cols);
    assert_eq!(up, 3);

    let down = byte_offset_for_buffer_position(text, 1, 9, width, left_cols);
    assert_eq!(down, text.len());
}

#[test]
fn virtual_term_shows_prompt() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    flush_redraws(&handle, &buf, &mut parser);

    assert!(screen_contains(&parser, 80, "> "));

    drop(term);
}

#[test]
fn virtual_term_renders_typed_input() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Simulate typing by setting the buffer directly (avoids
    // needing to drive the input event loop).
    handle.set_buffer("hello".to_owned(), 5);
    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "> hello"),
        "expected '> hello' on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

#[test]
fn virtual_term_renders_print_output() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.print_output(StyledBlock::new(StyledText::from(Span::plain(
        "Hello from output",
    ))));

    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "Hello from output"),
        "expected output on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

#[test]
fn virtual_term_updates_block_in_place() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Create a block in above_active (live area).
    let block_id = handle.new_block(StyledBlock::new(StyledText::from(Span::plain(
        "loading...",
    ))));
    handle.push_above_active(block_id);
    handle.redraw();

    flush_redraws(&handle, &buf, &mut parser);
    assert!(screen_contains(&parser, 80, "loading..."));

    // Update it in place.
    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain("done!"))),
    );
    handle.redraw();

    flush_redraws(&handle, &buf, &mut parser);
    assert!(
        screen_contains(&parser, 80, "done!"),
        "expected 'done!' on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
    assert!(
        !screen_contains(&parser, 80, "loading..."),
        "old content should be gone"
    );
}

#[test]
fn virtual_term_block_removed_from_active_then_printed_to_history() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Simulate streaming: create live block, update, finalize.
    let block_id = handle.new_block(StyledBlock::new(StyledText::from(Span::plain(
        "streaming...",
    ))));
    handle.push_above_active(block_id);
    handle.redraw();
    flush_redraws(&handle, &buf, &mut parser);

    // Update with partial text.
    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain("partial response"))),
    );
    handle.redraw();
    flush_redraws(&handle, &buf, &mut parser);
    assert!(screen_contains(&parser, 80, "partial response"));

    // Finalize: remove live block, print to history.
    handle.remove_block(block_id);
    handle.print_output(StyledBlock::new(StyledText::from(Span::plain(
        "final response",
    ))));
    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "final response"),
        "final should be visible, got: {:?}",
        vt100_rows(&parser, 80)
    );
    // The old "partial response" should be gone — only "final response" remains.
    assert!(
        !screen_contains(&parser, 80, "partial response"),
        "partial should be gone, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

/// Calling redraw_sync immediately after creating a virtual
/// terminal must not deadlock.  Before the fix, if the redraw
/// thread hadn't consumed the initial notification yet, the
/// sync check saw `sync_completed < sync_requested` and did
/// `continue`, looping forever without rendering.
#[test]
fn redraw_sync_does_not_deadlock_on_fresh_term() {
    for _ in 0..50 {
        let buf = SharedBuffer::new();
        let mut parser = vt100::Parser::new(10, 40, 0);
        let (term, handle, _input_tx) =
            Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

        // This would hang before the fix.
        handle.redraw_sync();
        buf.drain_into(&mut parser);
        assert!(screen_contains(&parser, 40, "> "));

        drop(term);
    }
}

/// Multiple concurrent redraw_sync calls must all complete.
#[test]
fn concurrent_redraw_syncs_all_complete() {
    let buf = SharedBuffer::new();
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Warm up — make sure redraw thread has done its first cycle.
    handle.redraw_sync();

    let barrier = Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let h = handle.clone();
            let b = barrier.clone();
            thread::spawn(move || {
                b.wait();
                h.redraw_sync();
            })
        })
        .collect();

    for t in threads {
        t.join().expect("redraw_sync thread panicked");
    }
}

/// A writer that can block on flush() and counts completed
/// flushes. Each flush corresponds to one render cycle.
#[derive(Clone)]
struct GatedWriter {
    inner: Arc<Mutex<GatedWriterInner>>,
    condvar: Arc<std::sync::Condvar>,
}

struct GatedWriterInner {
    /// When true, flush() blocks until gate is opened.
    gate_closed: bool,
    /// The writer is currently blocked inside flush().
    blocked: bool,
    /// Total number of flush() calls that have completed.
    flush_count: u64,
}

impl GatedWriter {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GatedWriterInner {
                gate_closed: false,
                blocked: false,
                flush_count: 0,
            })),
            condvar: Arc::new(std::sync::Condvar::new()),
        }
    }

    /// Close the gate — the next flush() will block.
    fn close_gate(&self) {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .gate_closed = true;
    }

    /// Block until the writer is actually stuck inside flush().
    fn wait_until_blocked(&self) {
        let guard = self.inner.lock().expect("gated writer poisoned");
        let _g = self
            .condvar
            .wait_while(guard, |s| !s.blocked)
            .expect("gated writer poisoned");
    }

    /// Open the gate — unblocks a stuck flush() and keeps it open.
    fn open_gate(&self) {
        let mut s = self.inner.lock().expect("gated writer poisoned");
        s.gate_closed = false;
        self.condvar.notify_all();
    }

    /// How many flush() calls have completed so far.
    fn flush_count(&self) -> u64 {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .flush_count
    }
}

impl io::Write for GatedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.inner.lock().expect("gated writer poisoned");
        if s.gate_closed {
            s.blocked = true;
            self.condvar.notify_all();
            s = self
                .condvar
                .wait_while(s, |s| s.gate_closed)
                .expect("gated writer poisoned");
            s.blocked = false;
        }
        s.flush_count += 1;
        self.condvar.notify_all();
        Ok(())
    }
}

/// Verify that notifications coalesce: while the redraw thread
/// is blocked mid-render, many notifications pile up and produce
/// at most one additional render after unblocking.
///
/// Uses the gated writer to create deterministic windows where
/// notifications must accumulate:
///
/// 1. Close gate → trigger render → redraw thread blocks in flush
/// 2. Fire N notifications (all coalesce into one pending flag)
/// 3. Open gate → blocked render completes → one coalesced render
/// 4. redraw_sync settles any remaining async renders
///
/// Per round we expect at most 3 flushes (blocked + coalesced +
/// sync). Without coalescing we'd see N+2 per round.
#[test]
fn notifications_coalesce_while_rendering() {
    let writer = GatedWriter::new();
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(writer.clone()), CursorShape::Bar);

    // Let the initial render finish so the redraw thread is idle
    // at recv(). The gate is open, so the render completes.
    handle.redraw_sync();

    const ROUNDS: usize = 5;
    const NOTIFICATIONS_PER_ROUND: usize = 10;

    for round in 0..ROUNDS {
        let before = writer.flush_count();

        // Close the gate so the next render blocks in flush().
        writer.close_gate();

        // Trigger a render — the redraw thread wakes from recv(),
        // renders, enters flush(), and blocks.
        handle.set_buffer(format!("r{round}"), 0);
        handle.redraw();
        writer.wait_until_blocked();

        // Redraw thread is stuck in flush. Fire many notifications.
        // They all coalesce into a single pending flag in the
        // notify channel.
        for j in 0..NOTIFICATIONS_PER_ROUND {
            handle.set_buffer(format!("r{round}-{j}"), 0);
            handle.redraw();
        }

        // Open the gate. The blocked flush completes, the loop
        // picks up the coalesced notification and renders once
        // more.
        writer.open_gate();

        // Settle: redraw_sync guarantees at least one render
        // after this point completes, draining any stragglers.
        handle.redraw_sync();

        let after = writer.flush_count();
        let renders = after - before;

        // Without coalescing we'd see NOTIFICATIONS_PER_ROUND + 2
        // (= 12) renders. With coalescing: the blocked render (1)
        // + the coalesced render (1) + possibly the sync render
        // (1) = at most 3.
        assert!(
            renders <= 3,
            "round {round}: expected ≤3 renders, got {renders}. \
                 Without coalescing this would be {}.",
            NOTIFICATIONS_PER_ROUND + 2,
        );
    }
}

/// Coalescing still works after sync: many async redraws followed
/// by a sync should reflect the final state, not spin.
#[test]
fn coalescing_preserved_after_sync() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(10, 40, 0);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Fire a bunch of async redraws, then one sync.
    for i in 0..20 {
        handle.set_buffer(format!("v{i}"), 2);
        handle.redraw();
    }
    handle.set_buffer("final".into(), 5);
    flush_redraws(&handle, &buf, &mut parser);
    assert!(
        screen_contains(&parser, 40, "> final"),
        "expected '> final', got: {:?}",
        vt100_rows(&parser, 40)
    );
}

/// full_render pushes overflow lines into terminal scrollback.
#[test]
fn full_render_populates_scrollback() {
    // Exact same params as the passing overflow test — only
    // line contents differ.
    let lines = plain_lines(&[
        "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> prompt",
    ]);
    let (mut term, _screen) = run_full_render(5, 30, lines, 3, 5, 7);

    // Scroll back 2 lines (the overflow amount).
    term.screen_mut().set_scrollback(2);
    let sb = visible_rows(&term);
    assert_eq!(
        sb[0], "line 0",
        "line 0 should be in scrollback, got: {sb:?}"
    );
    assert_eq!(
        sb[1], "line 1",
        "line 1 should be in scrollback, got: {sb:?}"
    );
}

/// Diff-path scrolling: history that overflows the viewport
/// during normal operation enters the terminal scrollback.
#[test]
fn diff_update_scrolls_overflow_into_scrollback() {
    let buf = SharedBuffer::new();
    // 5-row terminal with scrollback capacity.
    let mut parser = vt100::Parser::new(5, 40, 50);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    // Add 6 history lines — total is 7 (6 + prompt), viewport
    // is 5, so 2 lines overflow.
    for i in 0..6 {
        handle.print_output(StyledBlock::new(StyledText::from(Span::plain(format!(
            "line {i}"
        )))));
    }
    flush_redraws(&handle, &buf, &mut parser);

    // The prompt + last few history lines are visible.
    assert!(
        screen_contains(&parser, 40, "> "),
        "prompt should be visible, got: {:?}",
        vt100_rows(&parser, 40)
    );

    // The earliest lines should be in terminal scrollback.
    parser.screen_mut().set_scrollback(2);
    let sb_rows = vt100_rows(&parser, 40);
    assert!(
        sb_rows[0].contains("line 0"),
        "line 0 should be in scrollback, got: {sb_rows:?}"
    );
    assert!(
        sb_rows[1].contains("line 1"),
        "line 1 should be in scrollback, got: {sb_rows:?}"
    );
}

/// Pi-style overflow must also work when the content growth comes
/// from updating an existing live block in place, not only from
/// appending new history entries.
#[test]
fn live_block_growth_scrolls_updated_lines_into_scrollback() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let block_id = handle.new_block(StyledBlock::new(StyledText::from(Span::plain("starting"))));
    handle.push_above_active(block_id);
    flush_redraws(&handle, &buf, &mut parser);

    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain(
            "stream 0\nstream 1\nstream 2\nstream 3\nstream 4\nstream 5",
        ))),
    );
    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 40, "stream 5"),
        "latest line should remain visible, got: {:?}",
        vt100_rows(&parser, 40)
    );
    assert!(
        screen_contains(&parser, 40, "> "),
        "prompt should remain visible, got: {:?}",
        vt100_rows(&parser, 40)
    );

    parser.screen_mut().set_scrollback(2);
    let sb_rows = vt100_rows(&parser, 40);
    assert!(
        sb_rows[0].contains("stream 0"),
        "updated line 0 should be in scrollback, got: {sb_rows:?}"
    );
    assert!(
        sb_rows[1].contains("stream 1"),
        "updated line 1 should be in scrollback, got: {sb_rows:?}"
    );
}

/// Shift+Enter and Alt+Enter both insert a `\n` at the cursor
/// without submitting the line, while plain Enter still submits.
/// Mirrors the affordance users expect from chat UIs. Shift+Enter
/// covers terminals that speak the kitty keyboard protocol;
/// Alt+Enter (the `\e\r` byte sequence) is the universal fallback
/// for terminals that don't.
/// A buffer ending in `\n` (as produced by Shift+Enter / Alt+Enter)
/// must render with an extra blank row so the cursor visibly lands
/// on a new line — otherwise the prompt height doesn't grow until
/// the next character is typed.
#[test]
fn trailing_newline_buffer_grows_prompt_height() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 10, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(10, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("abc\n".to_owned(), "abc\n".len());
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(vt100_rows(&parser, 10), vec!["> abc", "", "", "", ""]);
    assert_eq!(parser.screen().cursor_position(), (1, 0));
}

#[test]
fn prompt_grows_when_cursor_wraps_at_exact_width() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 5, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(5, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("abc".to_owned(), "abc".len());
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(vt100_rows(&parser, 5), vec!["> abc", "", "", "", ""]);
    assert_eq!(parser.screen().cursor_position(), (1, 0));
}

#[test]
fn shift_or_alt_enter_inserts_newline_without_submitting() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("line one".to_owned(), "line one".len());

    // Shift+Enter: stay on the line, surface BufferChanged.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )))
        .expect("send shift+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n");

    // Alt+Enter: same behavior as shift, exercises the universal
    // fallback path.
    handle.set_buffer("line one\n".to_owned(), "line one\n".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )))
        .expect("send alt+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n\n");

    // Type more, then plain Enter to submit the whole multi-line
    // buffer as one Line event.
    handle.set_buffer("line one\nline two".to_owned(), "line one\nline two".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "line one\nline two"
    ));
}
