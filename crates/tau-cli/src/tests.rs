use std::sync::{Arc, Mutex};
use std::time::Duration;

use tau_cli_term::TermHandle;
use tau_cli_term_raw::Term;
use tau_proto::{
    AgentResponseFinished, AgentResponseUpdated, CborValue, Event, ExtAgentsMdAvailable,
    ExtensionReady, HarnessModelSelected, SessionPromptCreated, SessionPromptQueued,
    SessionStartReason, SessionStarted, ToolResult, UiPromptSubmitted,
};

use super::chat::{DraftSlot, should_send_draft_snapshot};
use super::event_renderer::EventRenderer;
use super::tool_render::{
    ToolStatus, build_osc1337_set_user_var, cache_hit_percent, format_context_chip,
    format_token_stats_line, format_turn_metrics_chip, render_tool_display, streaming_block,
    synthesize_fallback_display,
};

/// Writer that feeds bytes into a vt100::Parser. Bytes are
/// buffered per-write and flushed atomically to the parser on
/// flush(), so the test thread never sees a partial render.
#[derive(Clone)]
struct VtWriter {
    parser: Arc<Mutex<vt100::Parser>>,
}

impl VtWriter {
    fn new(parser: vt100::Parser) -> Self {
        Self {
            parser: Arc::new(Mutex::new(parser)),
        }
    }

    fn screen_text(&self, w: u16) -> Vec<String> {
        self.parser
            .lock()
            .expect("vt")
            .screen()
            .rows(0, w)
            .collect()
    }

    fn screen_contains(&self, w: u16, needle: &str) -> bool {
        self.screen_text(w).iter().any(|r| r.contains(needle))
    }
}

impl std::io::Write for VtWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Process bytes directly into the parser. The mutex
        // ensures the test thread sees a consistent state.
        self.parser.lock().expect("vt").process(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn setup(w: u16, h: u16) -> (Term, TermHandle, VtWriter) {
    let vt = VtWriter::new(vt100::Parser::new(h, w, 100));
    let (term, handle, _input) = Term::new_virtual(
        w as usize,
        h as usize,
        "> ",
        Box::new(vt.clone()),
        tau_cli_term::CursorShape::Bar,
    );
    (term, handle, vt)
}

fn sync(handle: &TermHandle) {
    handle.redraw_sync();
}

#[test]
fn stale_draft_snapshot_is_dropped_after_submit_epoch_bump() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: "s1".into(),
                text: "old".into(),
            },
        ));
    }

    let (epoch, _draft) = {
        let (mtx, _cv) = &handle;
        super::locked(mtx).pending.take().expect("pending draft")
    };
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.pending = None;
    }

    assert!(!should_send_draft_snapshot(&handle, epoch));
}

#[test]
fn current_draft_snapshot_is_sent_when_epoch_matches() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());

    assert!(should_send_draft_snapshot(&handle, 0));
}

#[test]
fn draft_snapshot_is_dropped_after_shutdown() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        super::locked(mtx).done = true;
    }

    assert!(!should_send_draft_snapshot(&handle, 0));
}

#[test]
fn new_session_clears_session_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "old prompt".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("old response".into()),
        tool_calls: vec![tau_proto::AgentToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            )]),
            display: None,
        }],
        input_tokens: Some(100),
        cached_tokens: Some(50),
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            ),
            (
                CborValue::Text("content".into()),
                CborValue::Text("fn main() {}\n".into()),
            ),
        ]),
        display: Some(tau_proto::ToolDisplay {
            args: "src/lib.rs".into(),
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old prompt"));
    assert!(vt.screen_contains(80, "old response"));
    assert!(vt.screen_contains(80, "read src/lib.rs"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "old prompt"));
    assert!(!vt.screen_contains(80, "old response"));
    assert!(!vt.screen_contains(80, "read src/lib.rs"));
    assert!(!vt.screen_contains(80, "no model selected"));
}

#[test]
fn new_session_replays_startup_context_and_kept_extensions() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: std::path::PathBuf::from("/tmp/AGENTS.md"),
        content: "# Test\n".into(),
    }));
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: "core-shell".into(),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "tau"));
    assert!(vt.screen_contains(80, "extension core-shell kept"));
}

#[test]
fn new_session_preserves_model_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessModelSelected(HarnessModelSelected {
        model: Some("test/model".into()),
        context_window: Some(100_000),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "test/model"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "test/model"));
    assert!(!vt.screen_contains(80, "no model selected"));
}

#[test]
fn single_prompt_response_cycle() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // User submits prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "> hello"));

    // Harness creates session prompt.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    // Agent streams response.
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hi there!".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hi there!"));

    // Agent finishes.
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("Hi there! How can I help?".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hi there! How can I help?"),
        "final response should be visible, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn thinking_renders_as_separate_block_above_response() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    sync(&handle);

    // Thinking arrives before the response text. Both should be
    // visible simultaneously, with thinking above response.
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: String::new(),
        thinking: Some("planning the answer".into()),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "planning the answer"),
        "thinking block should be live: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "actual answer".into(),
        thinking: Some("planning the answer".into()),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "actual answer"));
    assert!(vt.screen_contains(80, "planning the answer"));

    // Order matters even during live streaming: thinking should
    // render ABOVE the response, not below it.
    let live = vt.screen_text(80);
    let live_thinking = live
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("live thinking missing: {live:?}"));
    let live_response = live
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("live response missing: {live:?}"));
    assert!(
        live_thinking < live_response,
        "live thinking should render above live response (thinking @ {live_thinking}, response @ {live_response}); lines: {live:?}",
    );

    // On finish both stick in history.
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("actual answer".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: Some("planning the answer".into()),
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    // Thinking should appear above the response in the history.
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("thinking should remain in history: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("response should remain in history: {lines:?}"));
    assert!(
        thinking_row < response_row,
        "thinking should render above response (thinking @ {thinking_row}, response @ {response_row}); lines: {lines:?}",
    );
}

#[test]
fn set_show_thinking_round_trip_restores_history() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("the_response".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: Some("the_thinking_text".into()),
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "the_thinking_text"));
    assert!(vt.screen_contains(80, "the_response"));

    // Off — thinking content disappears, no placeholder, no
    // blank row left behind: the response should be on the same
    // row as the (now-empty) thinking block sat before. We assert
    // this indirectly by counting non-blank lines.
    let lines_before = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    renderer.apply_setting("show-thinking", "false");
    sync(&handle);
    assert!(!vt.screen_contains(80, "the_thinking_text"));
    assert!(!vt.screen_contains(80, "thinking hidden"));
    assert!(vt.screen_contains(80, "the_response"));
    let lines_after = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    // Hiding the one thinking block should remove exactly one
    // visible line of content from the screen.
    assert_eq!(lines_after + 1, lines_before);

    // Back on — original thinking text returns in its original
    // position above the response.
    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("the_thinking_text"))
        .unwrap_or_else(|| panic!("thinking should reappear: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("the_response"))
        .unwrap_or_else(|| panic!("response should still be visible: {lines:?}"));
    assert!(thinking_row < response_row);
}

#[test]
fn thinking_created_while_off_stays_invisible_after_toggle_on() {
    // Blocks that arrive while `show_thinking == false` are
    // never rendered and never tracked, so toggling back on
    // doesn't suddenly resurrect them. Only blocks that were
    // visible at some point round-trip through `set_block`.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-thinking", "false");

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("answer".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: Some("hidden reasoning".into()),
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "answer"));
    assert!(!vt.screen_contains(80, "hidden reasoning"));

    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "hidden reasoning"),
        "blocks created while off should not appear after toggle on"
    );
}

#[test]
fn no_thinking_block_when_summary_absent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("hello".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    // Just make sure we didn't crash and the response is visible.
    assert!(vt.screen_contains(80, "hello"));
}

#[test]
fn queued_prompt_renders_after_first_completes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // First prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "first".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));

    // Second prompt queued.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "second".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "second".into(),
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "second (queued)"),
        "queued indicator should show, got: {:?}",
        vt.screen_text(80)
    );

    // First finishes.
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("response one".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "response one"));

    // Second dispatched — "(queued)" should be removed.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-1".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "(queued)"),
        "queued indicator should be gone after dispatch, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> second"),
        "dispatched prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-1".into(),
        text: "response two".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two"),
        "second response should stream, got: {:?}",
        vt.screen_text(80)
    );

    // Second finishes.
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-1".into(),
        text: Some("response two complete".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two complete"),
        "final second response should show, got: {:?}",
        vt.screen_text(80)
    );
    // First response should still be visible.
    assert!(
        vt.screen_contains(80, "response one"),
        "first response should still show, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn three_queued_prompts_render_sequentially() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Three rapid prompts.
    for i in 0..3 {
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: format!("msg-{i}"),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
        if i == 0 {
            renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                session_prompt_id: "sp-0".into(),
                session_id: "s1".into(),
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                model: None,
                model_params: tau_proto::ModelParams::default(),
                tool_choice: Default::default(),
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
                previous_response: None,
            }));
        } else {
            renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
                session_id: "s1".into(),
                text: format!("msg-{i}"),
            }));
        }
    }

    // Process all three sequentially, flushing between each.
    for i in 0..3 {
        let spid: tau_proto::SessionPromptId = format!("sp-{i}").into();
        if i > 0 {
            renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                session_prompt_id: spid.clone(),
                session_id: "s1".into(),
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                model: None,
                model_params: tau_proto::ModelParams::default(),
                tool_choice: Default::default(),
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
                previous_response: None,
            }));
        }
        renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: spid.clone(),
            text: format!("partial-{i}"),
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        }));
        renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: spid,
            text: Some(format!("response-{i}")),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
            response_id: None,
            phase: None,
            reasoning_items: Vec::new(),
        }));
        sync(&handle);
    }

    // All three responses should be visible.
    // Extra flush to catch any delayed renders.
    sync(&handle);
    for i in 0..3 {
        assert!(
            vt.screen_contains(80, &format!("response-{i}")),
            "response-{i} should be visible, got: {:?}",
            vt.screen_text(80)
        );
    }
    // No stale "..." blocks.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn streaming_indicator_appends_during_updates() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));

    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("Hello".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "Hello …"));
}

#[test]
fn running_tool_call_shows_ellipsis_until_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: None,
        tool_calls: vec![tau_proto::AgentToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
            display: Some(tau_proto::ToolDisplay {
                args: "src/main.rs".into(),
                status: tau_proto::ToolDisplayStatus::InProgress,
                status_text: "…".into(),
                ..Default::default()
            }),
        }],
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs …"));

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            ),
            (
                CborValue::Text("content".into()),
                CborValue::Text("fn main() {}\n".into()),
            ),
        ]),
        display: Some(tau_proto::ToolDisplay {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolDisplayStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs (1L, 13B) ok"));
    assert!(!vt.screen_contains(80, "read src/main.rs …"));
}

#[test]
fn websearch_tool_result_shows_result_count_and_size() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-web".into(),
        tool_name: tau_proto::ToolName::new("websearch_exa"),
        result: CborValue::Text(
            "Title: One\nURL: https://one.example\n\nTitle: Two\nURL: https://two.example\n".into(),
        ),
        display: Some(tau_proto::ToolDisplay {
            args: String::new(),
            info_chips: vec!["(2 results, 5L, 73B)".into()],
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "websearch_exa (2 results, 5L, 73B) ok"));
}

#[test]
fn streaming_block_does_not_duplicate_on_finish() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "hello!".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("hello!".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);

    // Count how many rows contain "hello!".
    let count = vt
        .screen_text(80)
        .iter()
        .filter(|r| r.contains("hello!"))
        .count();
    assert_eq!(
        count,
        1,
        "response should appear exactly once, got {count}: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn agents_md_loaded_event_shows_output_stats() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: "/tmp/AGENTS.md".into(),
        content: "alpha\nbeta\n".into(),
    }));
    sync(&handle);

    let rows = vt.screen_text(80);
    assert!(
        rows.iter()
            .any(|row| row.contains("loaded: /tmp/AGENTS.md (2L, 11B)")),
        "loaded event should include output stats: {rows:?}"
    );
}

#[test]
fn render_tool_display_assembles_chips_in_order() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

    // grep-style: matches + stats + status.
    let display = ToolDisplay {
        args: "\"foo\" in src".into(),
        stats: ToolDisplayStats {
            matches: Some(3),
            lines: Some(7),
            bytes: Some(120),
        },
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_display("grep", &display);
    assert_eq!(rendered.tool_name, "grep");
    assert_eq!(rendered.args, "\"foo\" in src");
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["(3M, 7L, 120B)", "ok"]);
    assert!(matches!(
        rendered.suffixes.last().unwrap().status,
        ToolStatus::Success
    ));
}

#[test]
fn render_tool_display_diff_payload_adds_plus_minus_chips() {
    use tau_proto::{DiffSummary, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "src/main.rs".into(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolDisplayPayload::Diff(DiffSummary {
            added: 12,
            removed: 3,
            hunks: vec![],
        })),
        ..Default::default()
    };
    let rendered = render_tool_display("edit", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["+12", "-3", "ok"]);
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::DiffAdded));
    assert!(matches!(
        rendered.suffixes[1].status,
        ToolStatus::DiffRemoved
    ));
}

#[test]
fn synthesize_fallback_display_is_minimal() {
    let ok = synthesize_fallback_display("my_tool", None);
    assert_eq!(ok.args, "");
    assert_eq!(ok.status_text, "ok");
    assert!(matches!(ok.status, tau_proto::ToolDisplayStatus::Success));

    let err =
        synthesize_fallback_display("my_tool", Some("failure description\nwith trailing line"));
    assert_eq!(err.status_text, "err: failure description");
    assert!(matches!(err.status, tau_proto::ToolDisplayStatus::Error));
}

#[test]
fn render_tool_display_error_status_picks_error_severity() {
    use tau_proto::{ToolDisplay, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "/etc".into(),
        status: ToolDisplayStatus::Error,
        status_text: "err: permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_display("ls", &display);
    assert_eq!(rendered.suffixes.len(), 1);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::Error));
}

#[test]
fn build_osc1337_set_user_var_encodes_value_and_respects_tmux() {
    let plain = build_osc1337_set_user_var("user-notification", "hello", false);
    assert_eq!(plain, "\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07");
    let wrapped = build_osc1337_set_user_var("user-notification", "hello", true);
    assert_eq!(
        wrapped,
        "\x1bPtmux;\x1b\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07\x1b\\",
    );
}

#[test]
fn format_context_chip_picks_format_by_known_fields() {
    // Both window and percent known → percent/window chip.
    assert_eq!(
        format_context_chip(Some(12_000), Some(6), Some(200_000)),
        " ctx:6%/200k",
    );
    // Window unknown, tokens reported → tokens/? fallback.
    assert_eq!(format_context_chip(Some(12_000), None, None), " ctx:12k/?",);
    // Window known but no usage report yet still shows the percent
    // (which the harness initialized to 0 on model select).
    assert_eq!(
        format_context_chip(None, Some(0), Some(200_000)),
        " ctx:0%/200k",
    );
    // Nothing known → empty.
    assert_eq!(format_context_chip(None, None, None), "");
}

#[test]
fn format_turn_metrics_chip_includes_latency() {
    assert_eq!(
        format_turn_metrics_chip(Some(Duration::from_millis(1_240))),
        " resp:1.2s",
    );
    assert_eq!(format_turn_metrics_chip(None), "");
}

#[test]
fn format_token_stats_line_appends_hit_percent_when_cache_hits() {
    let usage = tau_proto::AgentTokenUsage {
        prompt_sent_tokens: 17_341,
        prompt_cached_tokens: 16_896,
        response_received_tokens: 29,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 100_000,
                cached_tokens: 50_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_token_stats_line(&usage);

    assert!(line.contains(" ↑ΔH97%"), "{line}");
    assert!(line.contains(" ↑ΣH50%"), "{line}");
    assert!(line.contains("↑Δ445/17.3k"), "{line}");
}

#[test]
fn format_token_stats_line_omits_hit_chip_when_no_prompt_sent() {
    let usage = tau_proto::AgentTokenUsage::default();
    let line = format_token_stats_line(&usage);

    assert!(!line.contains('H'), "{line}");
}

#[test]
fn cache_hit_percent_clamps_to_input_tokens() {
    assert_eq!(cache_hit_percent(Some(2_000), Some(1_500)), Some(75));
    assert_eq!(cache_hit_percent(Some(2_000), Some(3_000)), Some(100));
    assert_eq!(cache_hit_percent(Some(0), Some(0)), Some(0));
    assert_eq!(cache_hit_percent(Some(2_000), None), None);
}

#[test]
fn streaming_block_handles_each_trailing_case() {
    let theme = tau_themes::Theme::builtin();
    let cases = [
        ("", "…"),
        ("Hello", "Hello …"),
        ("Hello ", "Hello …"),
        ("Hello\t", "Hello\t…"),
        ("line\n", "line\n…"),
        ("line\n  ", "line\n  …"),
    ];
    for (input, expected) in cases {
        let block = streaming_block(&theme, tau_themes::names::AGENT_RESPONSE, input);
        let actual: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(actual, expected, "input was {input:?}");
    }
}

/// Reproduces the user-reported bug: send 3 prompts during the
/// first response's streaming. After all responses complete, the
/// prompt must be visible and all 3 responses rendered.
#[test]
fn three_prompts_during_streaming_all_render_correctly() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // User sends first prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));

    // Agent starts streaming response 1.
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hello"),
        "streaming should show, got: {:?}",
        vt.screen_text(80)
    );

    // User sends 2nd and 3rd prompts while streaming.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "hi".into(),
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "hi".into(),
    }));

    // More streaming updates (multi-line, like a real LLM).
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello!\n\nHow can I help you today?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    // Response 1 finishes.
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some("Hello!\n\nHow can I help you today?".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 should be in history, got: {:?}",
        vt.screen_text(80)
    );

    // Second prompt dispatched.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-1".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-1".into(),
        text: "Hello again!\n\nHow can I help you?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-1".into(),
        text: Some("Hello again!\n\nHow can I help you?".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 should be visible, got: {:?}",
        vt.screen_text(80)
    );

    // Third prompt dispatched.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-2".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-2".into(),
        text: "Hi there!\n\nWhat can I help you with?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-2".into(),
        text: Some("Hi there!\n\nWhat can I help you with?".into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);

    // All three responses should be visible.
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "What can I help you with?"),
        "response 3 missing, got: {:?}",
        vt.screen_text(80)
    );

    // The prompt must be visible at the bottom.
    assert!(
        vt.screen_contains(80, "> "),
        "prompt should be visible after all responses, got: {:?}",
        vt.screen_text(80)
    );

    // No stale streaming blocks should remain.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

/// Emoji (wide characters) in responses must not corrupt the
/// layout. Each emoji occupies 2 terminal columns; if we count
/// them as 1, text after the emoji shifts right and wraps
/// incorrectly.
#[test]
fn emoji_in_response_renders_correctly() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));

    // Response with emoji followed by text on next line.
    let response = "Hello! 👋\n\nHow can I help you today?";
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: response.into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some(response.into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);

    let text = vt.screen_text(40);

    // "Hello! 👋" should be on its own line, not merged with the
    // next line.
    assert!(
        vt.screen_contains(40, "Hello!"),
        "emoji line missing, got: {:?}",
        text
    );
    // The text after \n\n should start at column 0, not offset.
    assert!(
        text.iter().any(|r| r.starts_with("How can I help")),
        "text after emoji should start at column 0, got: {:?}",
        text
    );
    // Prompt must be visible.
    assert!(
        vt.screen_contains(40, "> "),
        "prompt missing, got: {:?}",
        text
    );
}

/// Multiple emoji in a single line must not cause column drift.
#[test]
fn multiple_emoji_no_column_drift() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));

    // 3 emoji = 6 columns + "end" = 9 columns total.
    let response = "🎉🎊🎈end\nnext line here";
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some(response.into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);

    let text = vt.screen_text(40);
    // "next line here" should start at column 0.
    assert!(
        text.iter().any(|r| r.starts_with("next line here")),
        "line after emoji should start at col 0, got: {:?}",
        text
    );
}

/// Replacing a long streaming block with its final settled output
/// must not leave stale partial lines behind, even when the live
/// block overflowed the viewport while streaming.
#[test]
fn overflowing_stream_replaced_cleanly_on_finish() {
    let (_term, handle, vt) = setup(40, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "overflow please".into(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        session_prompt_id: "sp-0".into(),
        session_id: "s1".into(),
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        previous_response: None,
    }));

    let partial = "stream 0\nstream 1\nstream 2\nstream 3\nPARTIAL ONLY";
    renderer.handle(&Event::AgentResponseUpdated(AgentResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: partial.into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(40, "PARTIAL ONLY"),
        "partial overflowed response should be visible before finish, got: {:?}",
        vt.screen_text(40)
    );

    let final_text = "final 0\nfinal 1\nfinal 2";
    renderer.handle(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-0".into(),
        text: Some(final_text.into()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        response_id: None,
        phase: None,
        reasoning_items: Vec::new(),
    }));
    sync(&handle);

    let text = vt.screen_text(40);
    assert!(
        vt.screen_contains(40, "final 0"),
        "final response missing, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "final 2"),
        "final response tail missing, got: {:?}",
        text
    );
    assert!(
        !vt.screen_contains(40, "PARTIAL ONLY"),
        "stale partial content should be gone, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "> "),
        "prompt should remain visible, got: {:?}",
        text
    );
}
