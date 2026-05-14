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
    let (term, completion_data) = HighTerm::new_for_test(
        raw_term,
        handle.clone(),
        commands,
        Theme::builtin(),
        std::iter::empty::<(String, String)>(),
    );
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
fn up_at_first_match_returns_to_original_buffer_then_wraps() {
    // From idx 0, Up returns to the un-selected state (no preview),
    // restoring the original buffer the user typed. A *second* Up
    // wraps around to the last candidate. This is the symmetric,
    // four-state cycle: None → 0 → 1 → ... → len-1 → 0 → 1 → ...,
    // with one None reachable on the Up-from-0 boundary.
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
        // Up from idx 0 → no selection → buffer is restored to what
        // the user actually typed.
        (KeyCode::Up, "/"),
        // Continuing Up from None wraps to the last match.
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
    // With prior submitted lines, Down at the prompt would normally
    // route to history navigation. The mode-driven dispatch in raw
    // gives the open completion menu first claim on Up/Down, so the
    // arrows cycle the menu and the history is never touched.
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
    // Down four times should cycle: /model, /quit, /model, /quit.
    // Wrapping is the normal `(i + 1) mod len` — the None state is
    // only reachable via Up at idx 0.
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

#[test]
fn editing_after_preview_commits_it_as_the_new_original_buffer() {
    // Once the user has cycled to a candidate and started editing
    // the previewed text, Esc should drop them back at *the edited
    // preview*, not at the prefix they originally typed before
    // opening the menu. This pins the "every edit commits the prior
    // preview" rule the raw layer documents in `refresh_completion`.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    type_text(&mut term, &input_tx, "/m");
    assert_eq!(handle.get_buffer(), "/m");

    // Cycle to "/model" — buffer now previews the candidate.
    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("preview /model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    // Backspace edits the preview. The new buffer ("/mode") still
    // matches "/model" by prefix, so the menu re-opens — but with
    // "/mode" as the new original.
    send_key(&input_tx, KeyCode::Backspace);
    assert!(matches!(
        term.get_next_event().expect("backspace edits preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/mode");

    // Esc dismisses to the edited preview, not back to "/m".
    send_key(&input_tx, KeyCode::Esc);
    assert!(matches!(
        term.get_next_event()
            .expect("esc returns to edited preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/mode");
}

mod trailer {
    use std::sync::{Arc, Mutex};

    use crate::{
        EditorContext, PROMPT_TRAILER_MARKER, append_prompt_trailer, strip_prompt_trailer,
    };

    fn ctx(ec: EditorContext) -> Arc<Mutex<EditorContext>> {
        Arc::new(Mutex::new(ec))
    }

    #[test]
    fn no_context_returns_buffer_unchanged() {
        let out = append_prompt_trailer("hello", &ctx(EditorContext::default()));
        assert_eq!(out, "hello");
    }

    #[test]
    fn roundtrip_strips_trailer_with_active_prompt() {
        let edited = append_prompt_trailer(
            "draft body",
            &ctx(EditorContext {
                active_prompt: Some("agent draft".to_owned()),
                last_agent_response: None,
                previous_prompt: None,
            }),
        );
        assert!(edited.contains(PROMPT_TRAILER_MARKER));
        assert!(edited.contains("agent draft"));
        assert_eq!(strip_prompt_trailer(&edited), "draft body");
    }

    #[test]
    fn roundtrip_strips_trailer_with_all_sections() {
        let edited = append_prompt_trailer(
            "user body",
            &ctx(EditorContext {
                active_prompt: Some("in progress".to_owned()),
                last_agent_response: Some("last".to_owned()),
                previous_prompt: Some("prev".to_owned()),
            }),
        );
        assert!(edited.contains("Current response in progress"));
        assert!(edited.contains("Last agent response"));
        assert!(edited.contains("Previous prompt"));
        assert_eq!(strip_prompt_trailer(&edited), "user body");
    }

    #[test]
    fn empty_section_strings_are_skipped() {
        let edited = append_prompt_trailer(
            "body",
            &ctx(EditorContext {
                active_prompt: Some(String::new()),
                last_agent_response: Some("kept".to_owned()),
                previous_prompt: Some(String::new()),
            }),
        );
        assert!(!edited.contains("Current response in progress"));
        assert!(edited.contains("Last agent response"));
        assert!(!edited.contains("Previous prompt"));
    }

    #[test]
    fn strip_without_marker_is_identity() {
        assert_eq!(strip_prompt_trailer("just text"), "just text");
    }

    #[test]
    fn user_text_containing_marker_is_truncated() {
        // Documents the *current* behavior: if the user's own draft
        // happens to contain the trailer marker, `strip_prompt_trailer`
        // truncates at the first occurrence. The marker is verbose
        // enough that this is unlikely in practice, but pinning the
        // behavior makes the trade-off explicit.
        let mut user_text = String::from("body with marker: ");
        user_text.push_str(PROMPT_TRAILER_MARKER);
        user_text.push_str(" and more");
        let stripped = strip_prompt_trailer(&user_text);
        assert_eq!(stripped, "body with marker: ");
    }
}

mod filesystem_token {
    use crate::completion::{self, CompletionData, SlashCommand, build_candidates};

    #[test]
    fn dotslash_token_triggers_filesystem_candidates() {
        // Empty directory listing is fine — we just need the path to
        // *match* as a filesystem token (vs. returning the slash-cmd
        // candidate list).
        let tmp = tempfile::tempdir().expect("tempdir");
        let prefix = format!("{}/", tmp.path().display());
        // Synthesize a buffer of the form "./" prefix. `build_candidates`
        // requires the prefix to start with "./" or "../"; absolute
        // paths are not a filesystem token.
        let buffer = "./";
        let cursor = buffer.len();
        let cands = build_candidates(
            &[SlashCommand::new("/whatever", "")],
            &CompletionData::new(),
            buffer,
            cursor,
        );
        // No assertion on contents (the test machine's CWD differs);
        // just confirm we didn't fall through to slash-command logic.
        for c in &cands {
            assert!(!c.replacement.starts_with('/'), "expected fs candidate");
        }
        let _ = prefix;
    }

    #[test]
    fn slash_command_buffer_does_not_route_to_filesystem() {
        let cands = build_candidates(
            &[SlashCommand::new("/model", "Switch model")],
            &CompletionData::new(),
            "/mod",
            "/mod".len(),
        );
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].replacement, "/model");
    }

    #[test]
    fn non_slash_non_path_buffer_returns_nothing() {
        let cands = build_candidates(
            &[SlashCommand::new("/model", "Switch model")],
            &CompletionData::new(),
            "hello",
            "hello".len(),
        );
        assert!(cands.is_empty());
    }

    #[test]
    fn parent_traversal_token_is_recognised() {
        let cands = build_candidates(
            &[SlashCommand::new("/whatever", "")],
            &CompletionData::new(),
            "../",
            "../".len(),
        );
        // Non-empty or empty is fine; we just verify it didn't fall
        // back to slash-command behavior (which would have been empty
        // since the buffer doesn't start with '/').
        for c in &cands {
            assert!(!c.replacement.starts_with('/'));
        }
        let _ = completion::SlashCommand::new("/x", "");
    }
}

mod multi_arg_completion {
    use std::sync::Arc;

    use crate::completion::{
        ArgCompleter, CommandName, CompletionData, CompletionItem, SlashCommand, build_candidates,
    };

    /// Build a completer that returns its first/second-arg menus
    /// verbatim, ignoring filtering. Lets the test focus on the
    /// argument-parsing + replacement-prefix logic in
    /// `build_arg_candidates`, not the ranking inside completers.
    fn make_completer() -> ArgCompleter {
        Arc::new(|args: &[&str]| match args.len() {
            1 => vec![
                CompletionItem::new("show-diff", "[false] diffs"),
                CompletionItem::new("show-thinking", "[true] reasoning"),
            ],
            2 => match args[0] {
                "show-diff" => vec![
                    CompletionItem::new("true", "enabled"),
                    CompletionItem::new("false", "disabled"),
                ],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        })
    }

    #[test]
    fn first_arg_completion_lists_names_with_descriptions() {
        let data = CompletionData::new();
        data.set_arg_completer(CommandName::new("/set"), make_completer());
        let buf = "/set ";
        let cands = build_candidates(
            &[SlashCommand::new("/set", "set a UI setting")],
            &data,
            buf,
            buf.len(),
        );
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].label, "show-diff");
        assert_eq!(cands[0].description, "[false] diffs");
        assert_eq!(cands[0].replacement, "/set show-diff");
    }

    #[test]
    fn second_arg_completion_keeps_first_arg_in_replacement() {
        let data = CompletionData::new();
        data.set_arg_completer(CommandName::new("/set"), make_completer());
        let buf = "/set show-diff ";
        let cands = build_candidates(
            &[SlashCommand::new("/set", "set a UI setting")],
            &data,
            buf,
            buf.len(),
        );
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].label, "true");
        // The first arg must be preserved in the replacement so
        // accepting a value completes the full `/set <name> <value>`
        // form rather than dropping the name.
        assert_eq!(cands[0].replacement, "/set show-diff true");
        assert_eq!(cands[1].replacement, "/set show-diff false");
    }

    #[test]
    fn third_arg_returns_no_candidates() {
        let data = CompletionData::new();
        data.set_arg_completer(CommandName::new("/set"), make_completer());
        let buf = "/set show-diff true ";
        let cands = build_candidates(
            &[SlashCommand::new("/set", "set a UI setting")],
            &data,
            buf,
            buf.len(),
        );
        assert!(cands.is_empty());
    }
}

mod prompt_action_parse {
    use crate::PromptShellAction;

    #[test]
    fn parses_history_actions() {
        assert!(matches!(
            PromptShellAction::parse("prompt-next"),
            Some(PromptShellAction::PromptNext)
        ));
        assert!(matches!(
            PromptShellAction::parse("prompt-previous"),
            Some(PromptShellAction::PromptPrevious)
        ));
    }

    #[test]
    fn parses_shell_insert_with_trim() {
        let parsed = PromptShellAction::parse("shell-prompt-insert:trim:echo hi");
        match parsed {
            Some(PromptShellAction::Insert(cmd)) => {
                assert!(cmd.trim);
                assert_eq!(cmd.command, "echo hi");
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parses_shell_edit_preserves_colons_in_command() {
        let parsed = PromptShellAction::parse("shell-prompt-edit:full:bash -c 'echo a:b:c'");
        match parsed {
            Some(PromptShellAction::Edit(cmd)) => {
                assert!(!cmd.trim);
                assert_eq!(cmd.command, "bash -c 'echo a:b:c'");
            }
            _ => panic!("expected Edit"),
        }
    }

    #[test]
    fn parses_fast_toggle() {
        assert!(matches!(
            PromptShellAction::parse("fast-toggle"),
            Some(PromptShellAction::FastToggle)
        ));
    }

    #[test]
    fn parses_role_cycle() {
        assert!(matches!(
            PromptShellAction::parse("role-cycle"),
            Some(PromptShellAction::RoleCycle)
        ));
    }

    #[test]
    fn unknown_action_returns_none() {
        assert!(PromptShellAction::parse("not-a-real-action").is_none());
        assert!(PromptShellAction::parse("shell-prompt-bogus:trim:cmd").is_none());
        assert!(PromptShellAction::parse("shell-prompt-edit:trim").is_none());
    }
}
