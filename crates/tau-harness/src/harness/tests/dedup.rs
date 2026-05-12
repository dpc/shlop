//! End-to-end tests for tool-result deduplication.
//!
//! Each test drives `Harness::handle_extension_event` with synthetic
//! `ToolResult` / `ToolError` frames and inspects the persisted
//! session tree to verify that the recorded entry is either the
//! original content or a `[tau-dedup]` pointer back to the first
//! occurrence on the conversation's branch.

use super::*;
use crate::dedup::{DEDUP_MARKER, DEFAULT_THRESHOLD_BYTES};

/// Drive a single `ToolResult` through the harness's normal intake
/// path (registers the call_id with `tool_conversations`,
/// `pending_tool_names`, and a `ToolsRunning` turn state, then sends
/// the result via `handle_extension_event`). Returns the recorded
/// `ToolActivityOutcome` for the call from the session tree.
fn run_tool_result(
    h: &mut Harness,
    session_id: &str,
    cid: &crate::conversation::ConversationId,
    call_id: &str,
    tool_name: &str,
    result: CborValue,
) -> ToolActivityOutcome {
    let call_id_typed: ToolCallId = call_id.into();
    let name = ToolName::new(tool_name);
    h.tool_conversations
        .insert(call_id_typed.clone(), cid.clone());
    h.pending_tool_names
        .insert(call_id_typed.clone(), name.clone());
    if let Some(conv) = h.conversations.get_mut(cid) {
        conv.turn_state = ConversationTurnState::ToolsRunning {
            remaining_calls: vec![call_id_typed.clone()],
        };
    }
    h.handle_extension_event(
        "shell",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: call_id_typed.clone(),
            tool_name: name,
            result,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("tool result");

    let tree = h.store.session(session_id).expect("session tree");
    tree.nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            SessionEntry::ToolActivity(rec) if rec.call_id.as_str() == call_id => {
                Some(rec.outcome.clone())
            }
            _ => None,
        })
        .expect("recorded outcome for call_id")
}

/// Like [`run_tool_result`] but for `ToolError`.
fn run_tool_error(
    h: &mut Harness,
    session_id: &str,
    cid: &crate::conversation::ConversationId,
    call_id: &str,
    tool_name: &str,
    message: String,
    details: Option<CborValue>,
) -> ToolActivityOutcome {
    let call_id_typed: ToolCallId = call_id.into();
    let name = ToolName::new(tool_name);
    h.tool_conversations
        .insert(call_id_typed.clone(), cid.clone());
    h.pending_tool_names
        .insert(call_id_typed.clone(), name.clone());
    if let Some(conv) = h.conversations.get_mut(cid) {
        conv.turn_state = ConversationTurnState::ToolsRunning {
            remaining_calls: vec![call_id_typed.clone()],
        };
    }
    h.handle_extension_event(
        "shell",
        Frame::Event(Event::ToolError(tau_proto::ToolError {
            call_id: call_id_typed.clone(),
            tool_name: name,
            message,
            details,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("tool error");

    let tree = h.store.session(session_id).expect("session tree");
    tree.nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            SessionEntry::ToolActivity(rec) if rec.call_id.as_str() == call_id => {
                Some(rec.outcome.clone())
            }
            _ => None,
        })
        .expect("recorded outcome for call_id")
}

/// Two large identical results land on the same conversation's
/// branch in sequence. The first is recorded verbatim; the second's
/// content is replaced with a pointer back to the first call_id.
#[test]
fn cross_turn_identical_result_collapses_to_pointer() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    let big = CborValue::Text("a".repeat(2048));

    let first = run_tool_result(&mut h, "s1", &cid, "call_first", "read", big.clone());
    assert!(
        matches!(&first, ToolActivityOutcome::Result { result } if result == &big),
        "first occurrence is recorded verbatim, got: {first:?}"
    );

    let second = run_tool_result(&mut h, "s1", &cid, "call_second", "read", big.clone());
    let ToolActivityOutcome::Result {
        result: dedup_result,
    } = second
    else {
        panic!("second occurrence should still be a Result outcome");
    };
    let CborValue::Text(text) = &dedup_result else {
        panic!("deduped result should be a CborValue::Text pointer; got: {dedup_result:?}");
    };
    assert!(
        text.starts_with(DEDUP_MARKER),
        "deduped text must start with the dedup marker; got: {text:?}",
    );
    assert!(
        text.contains("call_first"),
        "pointer must reference the first call_id; got: {text:?}",
    );
    // Lock in the terse pointer budget. The format is
    // `[tau-dedup] same as <tool> <call_id>` — currently ~50 B for
    // a typical tool name + OpenAI-style call_id. Cap at 100 B so a
    // future format change that grows the pointer significantly
    // (and erodes the dedup win) trips this test instead of slipping
    // through silently.
    assert!(
        text.len() < 100,
        "pointer text should stay terse (<100 B); got {} bytes: {text:?}",
        text.len(),
    );

    h.shutdown().expect("shutdown");
}

/// Results below the dedup threshold pass through unchanged even when
/// byte-identical. The pointer text would be comparable in size to
/// the content, so dedup costs more than it saves.
#[test]
fn small_results_below_threshold_are_not_deduped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    // Stay well clear of the threshold even after CBOR framing
    // overhead — 50 raw bytes of text encodes to ~52 B of CBOR.
    let small = CborValue::Text("ok".repeat(25));
    assert!("ok".repeat(25).len() < DEFAULT_THRESHOLD_BYTES);

    let first = run_tool_result(&mut h, "s1", &cid, "call_a", "shell", small.clone());
    let second = run_tool_result(&mut h, "s1", &cid, "call_b", "shell", small.clone());

    let ToolActivityOutcome::Result { result: r1 } = first else {
        unreachable!()
    };
    let ToolActivityOutcome::Result { result: r2 } = second else {
        unreachable!()
    };
    assert_eq!(r1, small);
    assert_eq!(
        r2, small,
        "below-threshold results must not be deduped — pointer would be the same size or larger"
    );

    h.shutdown().expect("shutdown");
}

/// A result that hashes to the same value as a *previously emitted
/// pointer* on the branch must not dedup against that pointer. The
/// rebuild-time skip on `[tau-dedup]`-prefixed entries is what
/// guarantees this; without it, a real result whose bytes happened to
/// match the pointer text would be redirected to the pointer's
/// (wrong) call_id.
#[test]
fn pointer_entries_are_not_themselves_dedup_anchors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    let big = CborValue::Text("z".repeat(2048));
    let _ = run_tool_result(&mut h, "s1", &cid, "call_orig", "read", big.clone());
    let _ = run_tool_result(&mut h, "s1", &cid, "call_dup", "read", big.clone());

    // Force a rebuild on the next intake by clearing the cached
    // dedup map. The next result will rebuild from the branch (which
    // now contains [Request_orig, Result_orig (real), Request_dup,
    // Result_dup (pointer)]) and we want to verify the pointer was
    // skipped.
    h.conversations
        .get_mut(&cid)
        .expect("default conv")
        .result_dedup = crate::dedup::ResultDedupMap::new();

    let third = run_tool_result(&mut h, "s1", &cid, "call_third", "read", big.clone());
    let ToolActivityOutcome::Result { result } = third else {
        unreachable!()
    };
    let CborValue::Text(text) = &result else {
        panic!("third occurrence should still dedup; got: {result:?}");
    };
    assert!(
        text.contains("call_orig"),
        "third occurrence must point at call_orig (the only real entry on the branch), \
         not at the pointer-bearing call_dup; got: {text:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Errors with the same message and the same details collapse into a
/// pointer; errors that share a message but differ in details stay
/// distinct (distinct details are usually what the model needs to
/// react to).
#[test]
fn identical_errors_collapse_but_distinct_details_stay() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    // Above the threshold: a 300-char "compile failed" message with
    // a long hex digest dump as details.
    let long_msg = "compile failed: ".to_owned() + &"E0277 ".repeat(50);

    let first = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e1",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block X".to_owned())),
    );
    let ToolActivityOutcome::Error { message: m1, .. } = first else {
        unreachable!()
    };
    assert_eq!(m1, long_msg, "first error recorded verbatim");

    let second = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e2",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block X".to_owned())),
    );
    let ToolActivityOutcome::Error {
        message: m2,
        details: d2,
    } = second
    else {
        unreachable!()
    };
    assert!(
        m2.starts_with(DEDUP_MARKER),
        "identical second error must dedup to a pointer; got message: {m2:?}",
    );
    assert!(
        d2.is_none(),
        "deduped error should drop the details payload"
    );

    let third = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e3",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block Y — different".to_owned())),
    );
    let ToolActivityOutcome::Error { message: m3, .. } = third else {
        unreachable!()
    };
    assert_eq!(
        m3, long_msg,
        "different details means the model needs the full content; must NOT dedup",
    );

    h.shutdown().expect("shutdown");
}

/// On session resume / a new harness binding to an existing session
/// tree, the dedup map is rebuilt lazily from the branch the first
/// time a tool result intake needs it. A new identical result must
/// dedup against the pre-existing entry from before the restore.
#[test]
fn dedup_map_rebuilds_on_session_restore() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let big = CborValue::Text("q".repeat(2048));

    {
        let mut h = echo_harness(&sp).expect("start");
        let cid = h.default_conversation_id.clone();
        let _ = run_tool_result(&mut h, "s1", &cid, "call_pre_restore", "read", big.clone());
        h.shutdown().expect("shutdown");
    }

    // New harness pointing at the same state dir + session id —
    // simulates daemon restart / session resume. The default conv
    // starts with `result_dedup` empty and `head=Some(N)` from the
    // resumed tree; the first intake triggers a rebuild.
    let mut h = echo_harness(&sp).expect("re-start");
    let cid = h.default_conversation_id.clone();
    assert!(
        h.conversations
            .get(&cid)
            .expect("default conv")
            .head
            .is_some(),
        "resumed default conversation must have a non-empty branch head",
    );

    let post = run_tool_result(&mut h, "s1", &cid, "call_post_restore", "read", big.clone());
    let ToolActivityOutcome::Result { result } = post else {
        unreachable!()
    };
    let CborValue::Text(text) = &result else {
        panic!("post-restore identical result should dedup; got: {result:?}");
    };
    assert!(
        text.contains("call_pre_restore"),
        "post-restore dedup must point at the pre-restore call_id; got: {text:?}",
    );

    h.shutdown().expect("shutdown");
}

/// A conversation should only see its OWN branch's prior entries —
/// it must not dedup against content that exists in the tree but
/// only on a different conversation's branch. Modeled here by
/// pinning a side conversation with its own `head` and verifying its
/// dedup map starts empty (no entries from the default conv leak in).
#[test]
fn dedup_is_scoped_to_a_single_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let default_cid = h.default_conversation_id.clone();
    let big = CborValue::Text("p".repeat(2048));

    // Land an entry on the default conversation's branch.
    let _ = run_tool_result(
        &mut h,
        "s1",
        &default_cid,
        "call_default",
        "read",
        big.clone(),
    );

    // Spawn a side conversation whose head is None (a fresh root —
    // not parented under the default conv's last node). Its dedup
    // map starts empty; an identical result on its branch must NOT
    // dedup against the default conv's call_default entry, because
    // the side conv's model has no visibility into the default
    // conv's history.
    let side_cid = crate::conversation::ConversationId::new("side-test");
    h.conversations.insert(
        side_cid.clone(),
        crate::conversation::Conversation::new(
            side_cid.clone(),
            "s1".into(),
            tau_proto::PromptOriginator::Extension {
                name: "core-delegate".into(),
                query_id: "q-test".to_owned(),
            },
            None, // explicit-root: no inherited head
            None,
        ),
    );

    let side_outcome = run_tool_result(&mut h, "s1", &side_cid, "call_side", "read", big.clone());
    let ToolActivityOutcome::Result { result } = side_outcome else {
        unreachable!()
    };
    assert_eq!(
        result, big,
        "side conversation's first identical result must NOT dedup against the default \
         conv's prior result — the model on the side conversation can't see that earlier \
         output in its assembled history",
    );

    h.shutdown().expect("shutdown");
}

/// A self-pointer is a defensive no-op: if the same call_id somehow
/// reaches the dedup intake twice (a tracking-map bug, not a model
/// behavior), the second pass must NOT replace the result with a
/// pointer to itself — that would be unrecoverable for the model.
#[test]
fn dedup_refuses_to_self_point() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    let big = CborValue::Text("s".repeat(2048));

    let _first = run_tool_result(&mut h, "s1", &cid, "call_solo", "read", big.clone());

    // Manually run the dedup intake again on a result with the same
    // call_id and same content. Without the self-pointer guard this
    // would produce `[tau-dedup] identical to ... call_solo ...` —
    // a pointer to itself.
    let mut replay = ToolResult {
        call_id: "call_solo".into(),
        tool_name: ToolName::new("read"),
        result: big.clone(),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };
    h.dedup_tool_result(&cid, &mut replay);
    assert_eq!(
        replay.result, big,
        "self-pointer guard must leave content untouched when the existing \
         dedup-map entry already points at the same call_id; got: {:?}",
        replay.result,
    );

    h.shutdown().expect("shutdown");
}
