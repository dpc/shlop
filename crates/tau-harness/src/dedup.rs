//! Per-conversation deduplication of large, byte-identical tool
//! results.
//!
//! Models occasionally re-issue identical reads, repeat the same
//! probing shell command (`jj status`, `cargo check` after a no-op
//! edit), or emit the same parallel tool call twice in one batch.
//! Each repetition pins a copy of the tool output into the prompt
//! prefix forever, both bloating the steady-state context and
//! defeating the prompt cache for every subsequent turn that has to
//! re-anchor on the larger prefix.
//!
//! This module replaces the *content* of any tool result whose CBOR
//! encoding hashes to the same value as a result already on the
//! conversation's branch with a short pointer
//! (`[tau-dedup] same as <tool_name> <call_id>`). The first
//! occurrence is kept verbatim — only the duplicates are collapsed.
//! The model can cross-reference the pointer to the original
//! `call_id` which is still present earlier in its own context.
//!
//! Three invariants protect correctness:
//!
//! 1. **Branch isolation.** The map is per-conversation and rebuilt from the
//!    conversation's branch when the cursor moves non-linearly (e.g.
//!    `UiNavigateTree` to a sibling tip). A pointer can never reference a
//!    `call_id` the model can't see in its own assembled history.
//!
//! 2. **First-write-only.** Replacement happens at result-intake time, before
//!    the result is folded into the session tree. Once recorded the entry is
//!    frozen for the rest of the session, preserving the harness's
//!    linear-prefix invariant for the upstream prompt cache.
//!
//! 3. **Threshold gated.** Results whose serialized form is below
//!    [`DEFAULT_THRESHOLD_BYTES`] are not deduped at all — the pointer text
//!    itself runs ~50 B, so single-digit savings aren't worth the extra hop the
//!    model has to make to recover the original.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use tau_core::{SessionEntry, ToolActivityOutcome};
use tau_proto::{CborValue, NodeId, ToolCallId};

/// Sentinel prefix on dedup-pointer text. Picked so it is exceedingly
/// unlikely to appear at the start of a real tool output. Used both
/// as a marker for the model ("this is a synthesized pointer, not
/// real content") and as the rebuild-time skip predicate so a pointer
/// is never re-hashed and re-inserted into the map (which would let a
/// later real result accidentally dedup against the *pointer* text).
pub(crate) const DEDUP_MARKER: &str = "[tau-dedup]";

/// Minimum CBOR-serialized size of a tool result to consider
/// deduping. Below this, the pointer text is comparable to the
/// original content and the model cost of the redirect outweighs the
/// savings. 256 B leaves a healthy margin over the ~50-B pointer
/// text and covers most "empty-success" tool outputs without
/// burning cycles hashing them.
pub(crate) const DEFAULT_THRESHOLD_BYTES: usize = 256;

/// 16-byte truncated SHA-256 digest of the CBOR-serialized result
/// content. Truncation gives a ~10⁻¹⁹ collision probability per
/// pair, which is fine — a collision would only mean two unrelated
/// outputs get aliased, and the model would see a pointer to the
/// "wrong" call. In practice the failure mode is so rare that
/// chasing it isn't worth the wider hash.
pub(crate) type ResultHash = [u8; 16];

/// Per-conversation dedup state. Tracks the hash of every full-fat
/// tool result (and tool error message) seen on the current branch,
/// keyed back to the first `call_id` that produced that content.
///
/// `built_for` records the [`NodeId`] the map was last synchronized
/// with. When the conversation's cursor moves non-linearly (a
/// navigation), [`Self::needs_rebuild`] returns true and the harness
/// rebuilds from the new branch before the next dedup decision.
#[derive(Debug, Default, Clone)]
pub(crate) struct ResultDedupMap {
    map: HashMap<ResultHash, ToolCallId>,
    built_for: Option<NodeId>,
}

impl ResultDedupMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the cached map's notion of the conversation
    /// head differs from `current`, i.e. the conversation jumped to a
    /// branch that wasn't a linear extension of where the map was
    /// built. The harness clears and rebuilds in that case.
    pub(crate) fn needs_rebuild(&self, current: Option<NodeId>) -> bool {
        self.built_for != current
    }

    /// Replace contents from a freshly walked branch. Called after
    /// [`Self::needs_rebuild`] reports a mismatch, or eagerly on
    /// session resume / session switch.
    pub(crate) fn rebuild_from_branch<'a>(
        &mut self,
        branch: impl IntoIterator<Item = &'a SessionEntry>,
        new_head: Option<NodeId>,
        threshold: usize,
    ) {
        self.map.clear();
        for entry in branch {
            let SessionEntry::ToolActivity(activity) = entry else {
                continue;
            };
            let (content_hash, content_bytes) = match &activity.outcome {
                ToolActivityOutcome::Result { result } => {
                    if is_dedup_pointer_value(result) {
                        continue;
                    }
                    let bytes = encode_for_hash(result);
                    (sha256_truncated(&bytes), bytes.len())
                }
                ToolActivityOutcome::Error { message, details } => {
                    if message.starts_with(DEDUP_MARKER) {
                        continue;
                    }
                    let bytes = encode_error_for_hash(message, details.as_ref());
                    (sha256_truncated(&bytes), bytes.len())
                }
                ToolActivityOutcome::Requested { .. } => continue,
            };
            if content_bytes < threshold {
                continue;
            }
            // Earlier occurrences win — the pointer must reference the
            // first `call_id` in the assembled history so the model
            // sees the canonical content above the pointer.
            self.map
                .entry(content_hash)
                .or_insert_with(|| activity.call_id.clone());
        }
        self.built_for = new_head;
    }

    /// Look up `hash`. Returns the first `call_id` that produced this
    /// content on the current branch, or `None` if it's new.
    pub(crate) fn lookup(&self, hash: &ResultHash) -> Option<&ToolCallId> {
        self.map.get(hash)
    }

    /// Record a fresh `(hash, call_id)` pair. Caller must have
    /// confirmed `lookup(&hash).is_none()` first; an
    /// already-present hash is a programming error and triggers a
    /// debug-assertion-only panic so production sessions just keep
    /// the original mapping.
    pub(crate) fn insert(&mut self, hash: ResultHash, call_id: ToolCallId) {
        debug_assert!(
            !self.map.contains_key(&hash),
            "dedup map insert called on existing hash; lookup-before-insert was skipped"
        );
        self.map.insert(hash, call_id);
    }

    /// Advance the map's "built for" cursor without touching the
    /// table. Called after an event commits and the conversation head
    /// moves linearly to the just-folded node — the map is already
    /// in sync with that branch tip, so no rebuild is needed.
    ///
    /// **Skips when `built_for` is `None`.** That state means the map
    /// has never been populated for this conversation (fresh harness
    /// after session resume; map cleared after a navigation). A
    /// commit at this stage might be a non-dedup-eligible event (a
    /// `UserMessage` from session re-init, an `AgentMessage`) whose
    /// fold doesn't pass through `dedup_tool_result`. Advancing
    /// unconditionally would mark the map as "in sync with this new
    /// head" while still empty, making the next dedup intake skip
    /// the rebuild and miss every historical entry on the branch.
    /// The lazy rebuild on the next dedup intake is what populates
    /// the map; this method is only an optimization for the
    /// already-built case.
    pub(crate) fn note_head_advanced_to(&mut self, new_head: NodeId) {
        if self.built_for.is_some() {
            self.built_for = Some(new_head);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// CBOR-encode `value` for hashing. Encoding (rather than e.g. a
/// printable repr) keeps the hash stable across renderer changes
/// (`cbor_to_text` is for human / LLM consumption only and may evolve
/// formatting).
pub(crate) fn encode_for_hash(value: &CborValue) -> Vec<u8> {
    let mut buf = Vec::new();
    // ciborium's writer is infallible into a `Vec<u8>`; a serialize
    // failure here would mean a non-encodable CBOR value reached us,
    // which `tau_proto` rules out at the type level.
    ciborium::into_writer(value, &mut buf)
        .expect("CborValue from tau_proto should always serialize back to CBOR");
    buf
}

/// Companion to [`encode_for_hash`] for `ToolError` outcomes. Keys on
/// the message string and the optional details payload jointly so a
/// repeated error with the same message and same details collapses,
/// while two errors that share a message but carry different details
/// stay distinct.
pub(crate) fn encode_error_for_hash(message: &str, details: Option<&CborValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    // Tag-prefix to keep the error keyspace disjoint from the result
    // keyspace — without this, an error message text would collide
    // with a result whose CBOR-encoded form was the same byte sequence.
    buf.extend_from_slice(b"err\x00");
    buf.extend_from_slice(message.as_bytes());
    buf.push(0);
    if let Some(details) = details {
        ciborium::into_writer(details, &mut buf)
            .expect("CborValue from tau_proto should always serialize back to CBOR");
    }
    buf
}

/// SHA-256 of `bytes`, truncated to 16 bytes. See [`ResultHash`].
pub(crate) fn sha256_truncated(bytes: &[u8]) -> ResultHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Build the CBOR value that replaces a duplicate tool result.
/// Encodes as `CborValue::Text` so both the wire format and the
/// downstream [`crate::prompt::cbor_to_text`] path see identical
/// human-readable content with a stable, recognizable prefix.
///
/// Format kept terse on purpose: the model already knows from the
/// wrapping `function_call_output` that this is a tool-result
/// payload, so re-stating "result of …" is redundant. Tool-name and
/// call_id are enough for the model to locate the original output
/// earlier in its own context.
pub(crate) fn build_pointer_value(
    original_call_id: &ToolCallId,
    tool_name: &tau_proto::ToolName,
) -> CborValue {
    CborValue::Text(format!(
        "{DEDUP_MARKER} same as {} {}",
        tool_name.as_str(),
        original_call_id
    ))
}

/// Build the error-message string that replaces a duplicate tool
/// error. The full pointer goes into the `message` field with the
/// same marker prefix; `details` is dropped because it is what made
/// the original distinct and the pointer's job is to refer back, not
/// to reproduce it. The wrapping `function_call_output` is rendered
/// with an "ERROR:" prefix downstream, so the pointer doesn't need
/// to restate the kind here.
pub(crate) fn build_pointer_error_message(
    original_call_id: &ToolCallId,
    tool_name: &tau_proto::ToolName,
) -> String {
    format!(
        "{DEDUP_MARKER} same as {} {}",
        tool_name.as_str(),
        original_call_id
    )
}

/// True when `value` is a previously-emitted dedup pointer rather
/// than a real tool result. Recognized by the [`DEDUP_MARKER`] prefix
/// on a `CborValue::Text` payload; any other shape is real content.
pub(crate) fn is_dedup_pointer_value(value: &CborValue) -> bool {
    matches!(value, CborValue::Text(s) if s.starts_with(DEDUP_MARKER))
}

#[cfg(test)]
mod tests {
    use tau_core::{ToolActivityOutcome, ToolActivityRecord};
    use tau_proto::{CborValue, ToolCallId, ToolName};

    use super::*;

    fn cbor_text(s: &str) -> CborValue {
        CborValue::Text(s.to_owned())
    }

    fn result_entry(call_id: &str, content: &str) -> SessionEntry {
        SessionEntry::ToolActivity(ToolActivityRecord {
            call_id: ToolCallId::from(call_id),
            tool_name: ToolName::new("read"),
            outcome: ToolActivityOutcome::Result {
                result: cbor_text(content),
            },
        })
    }

    #[test]
    fn rebuild_records_only_above_threshold() {
        let small = "x".repeat(50);
        let big = "y".repeat(1024);
        let entries = vec![
            result_entry("call_small", &small),
            result_entry("call_big", &big),
        ];
        let mut map = ResultDedupMap::new();
        map.rebuild_from_branch(&entries, Some(NodeId::new(1)), DEFAULT_THRESHOLD_BYTES);
        // Only the big entry was over the threshold.
        assert_eq!(map.len(), 1);
        let big_hash = sha256_truncated(&encode_for_hash(&cbor_text(&big)));
        assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_big"),);
    }

    #[test]
    fn rebuild_skips_dedup_pointers() {
        let big = "z".repeat(1024);
        let pointer = format!(
            "{DEDUP_MARKER} identical to result of `read` call_id `call_x` — see that earlier output"
        );
        let entries = vec![
            result_entry("call_a", &big),
            // A previously-recorded dedup pointer that was already
            // serving as a stand-in for `call_a`'s content. On
            // rebuild it must NOT enter the map — otherwise a future
            // result whose CBOR happened to match the pointer text
            // would dedup against the wrong (pointer's) call_id.
            result_entry("call_b", &pointer),
        ];
        let mut map = ResultDedupMap::new();
        map.rebuild_from_branch(&entries, Some(NodeId::new(2)), DEFAULT_THRESHOLD_BYTES);
        // call_a entered, call_b's pointer text did not.
        assert_eq!(map.len(), 1);
        let big_hash = sha256_truncated(&encode_for_hash(&cbor_text(&big)));
        assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_a"),);
    }

    #[test]
    fn rebuild_keeps_first_call_id_on_duplicate() {
        let big = "q".repeat(1024);
        let entries = vec![
            result_entry("call_first", &big),
            result_entry("call_second", &big),
        ];
        let mut map = ResultDedupMap::new();
        map.rebuild_from_branch(&entries, Some(NodeId::new(2)), DEFAULT_THRESHOLD_BYTES);
        let h = sha256_truncated(&encode_for_hash(&cbor_text(&big)));
        assert_eq!(
            map.lookup(&h).map(|c| c.as_str()),
            Some("call_first"),
            "earliest occurrence on the branch must own the slot"
        );
    }

    #[test]
    fn needs_rebuild_detects_head_jump() {
        let mut map = ResultDedupMap::new();
        map.rebuild_from_branch(
            std::iter::empty(),
            Some(NodeId::new(5)),
            DEFAULT_THRESHOLD_BYTES,
        );
        assert!(!map.needs_rebuild(Some(NodeId::new(5))));
        // Linear advance still counts as a rebuild trigger from this
        // helper's POV — the harness handles linear advance via
        // `note_head_advanced_to`, not a rebuild.
        assert!(map.needs_rebuild(Some(NodeId::new(6))));
        assert!(map.needs_rebuild(None));
    }

    #[test]
    fn pointer_value_starts_with_marker() {
        let v = build_pointer_value(&ToolCallId::from("call_xyz"), &ToolName::new("read"));
        let CborValue::Text(s) = v else {
            panic!("pointer should always be CborValue::Text");
        };
        assert!(s.starts_with(DEDUP_MARKER), "got: {s}");
        assert!(is_dedup_pointer_value(&CborValue::Text(s)));
    }

    #[test]
    fn pointer_error_message_starts_with_marker() {
        let m = build_pointer_error_message(&ToolCallId::from("call_xyz"), &ToolName::new("shell"));
        assert!(m.starts_with(DEDUP_MARKER), "got: {m}");
    }

    #[test]
    fn error_hash_keyspace_is_disjoint_from_result_keyspace() {
        // An error message and a tool result whose CBOR-encoded form
        // is the same string must not collide. The "err\0" prefix on
        // error encoding guarantees this.
        let s = "abc".repeat(200);
        let result_bytes = encode_for_hash(&cbor_text(&s));
        let error_bytes = encode_error_for_hash(&s, None);
        assert_ne!(
            sha256_truncated(&result_bytes),
            sha256_truncated(&error_bytes),
        );
    }

    #[test]
    fn error_details_distinguish_otherwise_identical_messages() {
        let msg = "compile failed".to_owned();
        let h1 = sha256_truncated(&encode_error_for_hash(&msg, None));
        let h2 = sha256_truncated(&encode_error_for_hash(
            &msg,
            Some(&cbor_text("error: missing semicolon")),
        ));
        assert_ne!(h1, h2);
    }

    /// Regression guard: `note_head_advanced_to` must skip the
    /// advance when `built_for` is `None`. The harness calls this hook
    /// on *every* fold (including ones that don't pass through dedup
    /// intake — `UserMessage` from session re-init, `AgentMessage`,
    /// `ToolRequest`). On a freshly resumed session the map starts
    /// empty with `built_for == None`; if such a fold advanced the
    /// cursor unconditionally, `needs_rebuild(new_head)` would return
    /// `false` on the next dedup intake and the lazy rebuild would
    /// never run, silently losing every historical entry on the
    /// branch. A naive "just always set built_for" simplification
    /// would re-introduce that bug, which is the exact regression the
    /// `dedup_map_rebuilds_on_session_restore` integration test
    /// caught during development.
    #[test]
    fn note_head_advanced_skips_when_built_for_is_none() {
        let mut map = ResultDedupMap::new();
        assert!(map.needs_rebuild(Some(NodeId::new(7))));
        map.note_head_advanced_to(NodeId::new(7));
        assert!(
            map.needs_rebuild(Some(NodeId::new(7))),
            "advancing built_for from None would mark the map as in-sync \
             with a head it has never been populated for, masking the lazy \
             rebuild on the next intake",
        );
    }

    #[test]
    fn note_head_advanced_does_not_clear() {
        let big = "p".repeat(1024);
        let entries = vec![result_entry("call_a", &big)];
        let mut map = ResultDedupMap::new();
        map.rebuild_from_branch(&entries, Some(NodeId::new(1)), DEFAULT_THRESHOLD_BYTES);
        assert_eq!(map.len(), 1);
        map.note_head_advanced_to(NodeId::new(2));
        assert!(!map.needs_rebuild(Some(NodeId::new(2))));
        assert_eq!(map.len(), 1);
    }
}
