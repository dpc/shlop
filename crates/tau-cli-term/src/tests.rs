use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

fn new_test_term_with_data(
    commands: Vec<SlashCommand>,
) -> (
    HighTerm,
    TermHandle,
    CompletionData,
    std::sync::mpsc::Sender<TestRawEvent>,
) {
    let (raw_term, handle, input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        CursorShape::Bar,
    );
    let (term, completion_data) =
        HighTerm::new_for_test(raw_term, handle.clone(), commands, Theme::builtin());
    (term, handle, completion_data, input_tx)
}

fn new_test_term(
    commands: Vec<SlashCommand>,
) -> (HighTerm, TermHandle, std::sync::mpsc::Sender<TestRawEvent>) {
    let (term, handle, _completion_data, input_tx) = new_test_term_with_data(commands);
    (term, handle, input_tx)
}

fn send_key(input_tx: &std::sync::mpsc::Sender<TestRawEvent>, code: KeyCode) {
    input_tx
        .send(TestRawEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("send key");
}

fn submit(
    term: &mut HighTerm,
    handle: &TermHandle,
    input_tx: &std::sync::mpsc::Sender<TestRawEvent>,
    line: &str,
) {
    handle.set_buffer(line.to_owned(), line.len());
    send_key(input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("submit line"),
        Event::Line(submitted) if submitted == line
    ));
}

fn type_text(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, text: &str) {
    for ch in text.chars() {
        send_key(input_tx, KeyCode::Char(ch));
        assert!(matches!(
            term.get_next_event().expect("type char"),
            Event::BufferChanged
        ));
    }
}

fn submit_typed(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, line: &str) {
    type_text(term, input_tx, line);
    send_key(input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("submit typed line"),
        Event::Line(submitted) if submitted == line
    ));
}

#[test]
fn typed_history_item_matching_completion_needs_one_up_per_item() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit_typed(&mut term, &input_tx, "Hi");
    submit_typed(&mut term, &input_tx, "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to slash history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn history_after_accepting_argument_completion_needs_one_up_per_item() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new("/model"),
        vec![CompletionItem::plain("openai/gpt-5")],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, "/model op");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle argument completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Enter);
    send_key(&input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("accept and submit completion"),
        Event::Line(line) if line == "/model openai/gpt-5"
    ));

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to completed history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn history_items_matching_completion_do_not_steal_following_history_navigation() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");
    submit(&mut term, &handle, &input_tx, "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to slash history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn up_arrow_cycles_completion_after_down_cycles_with_history_present() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new("/model"),
        vec![
            CompletionItem::plain("anthropic/claude-sonnet-4-5"),
            CompletionItem::plain("openai/gpt-5"),
            CompletionItem::plain("openai/gpt-5-mini"),
        ],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, "/model ");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model anthropic/claude-sonnet-4-5");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to second model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("cycle back to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model anthropic/claude-sonnet-4-5");
}

#[test]
fn arrows_cycle_active_completion_even_when_history_exists() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion again with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/quit");
}

#[test]
fn down_then_up_in_completion_does_not_wrap_past_the_first_match() {
    // Symmetry probe: Down/Down/Up/Up should land back on the first
    // match (or back at the un-selected original buffer) — never on
    // the *last* match. Today `cycle_selection(-1)` from index 0 wraps
    // to `len - 1` via `rem_euclid`, so this sequence ends on `/quit`
    // when intuition says `/model` (or `/`). This test pins the
    // current behavior so a future fix has something to flip.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));

    let sequence: &[(KeyCode, &str)] = &[
        (KeyCode::Down, "/model"),
        (KeyCode::Down, "/quit"),
        (KeyCode::Up, "/model"),
        // Surprise: from `/model` (idx 0), Up wraps to the last match
        // rather than dismissing the menu or returning to "/". If you
        // came here looking for the cause of "Up after Down feels
        // weird," this is it — `cycle_selection(-1)` doesn't treat
        // "before the first" as "back to original buffer."
        (KeyCode::Up, "/quit"),
    ];
    for (i, (key, want)) in sequence.iter().enumerate() {
        send_key(&input_tx, *key);
        assert!(matches!(
            term.get_next_event().expect("cycle"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "step {} ({key:?}): expected {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_cycle_repeatedly_through_completion_with_history_present() {
    // Same as the no-history case, but with prior submitted lines. The
    // raw terminal layer reacts to Down by trying to advance history;
    // the high-level handler is supposed to undo that with `move_up`
    // and cycle the completion menu instead. If that undo doesn't
    // hold up across a second pair of arrow presses (e.g. because
    // `history_nav_active` gets set by an earlier press and then the
    // gate at lib.rs:132 misroutes the next arrow into history mode),
    // the second Down pair would either dismiss the menu or jump out
    // of the completion list.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "earlier-1");
    submit(&mut term, &handle, &input_tx, "earlier-2");

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    let expected = ["/model", "/quit", "/model", "/quit"];
    for (i, want) in expected.iter().enumerate() {
        send_key(&input_tx, KeyCode::Down);
        assert!(matches!(
            term.get_next_event().expect("cycle completion"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "after {} Down keypresses (with history present) the buffer \
             should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_cycle_repeatedly_through_completion_suggestions() {
    // Going Down twice and then Down twice more should cycle through
    // the candidate list twice — `cycle_selection` uses `rem_euclid`
    // so the menu wraps. With two commands `/model`, `/quit`, the
    // expected buffer after each Down is: /model, /quit, /model,
    // /quit. A bug where the second pair stops working (e.g. the
    // completer dismisses itself, or `last_key_was_down` gets stuck)
    // would surface here.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    let expected = ["/model", "/quit", "/model", "/quit"];
    for (i, want) in expected.iter().enumerate() {
        send_key(&input_tx, KeyCode::Down);
        assert!(matches!(
            term.get_next_event().expect("cycle completion"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "after {} Down keypresses the buffer should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_still_cycle_active_completion_suggestions() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion again"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/quit");
}
