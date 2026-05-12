//! Conversation prompt-queue dispatch.
//!
//! Each conversation owns a per-conversation `pending_prompts` queue.
//! The harness has no global agent slot — the agent extension serializes
//! its own consumption of `SessionPromptCreated` from the event log —
//! so the dispatch logic here just drains one prompt per *runnable*
//! conversation (Idle turn state, non-empty queue) and lets the agent
//! interleave them on its side.
//!
//! [`Harness::dispatch_user_prompt`] is the direct entry point for
//! interactive submission on the default conversation;
//! [`Harness::dispatch_prompt_for_conversation`] is the shared
//! per-conversation primitive (also used by side queries spawned via
//! `ExtAgentQuery`). [`Harness::try_advance_queue`] is the
//! react-to-state-change drain that picks the next runnable
//! conversation and dispatches one prompt from its queue.
//!
//! [`Harness::dispatch_blocked_for`] is the predicate the rest of the
//! harness uses to decide whether to dispatch immediately or queue.

use tau_proto::{Event, SessionId};

use crate::conversation::{ConversationId, ConversationTurnState};
use crate::error::HarnessError;
use crate::harness::Harness;

impl Harness {
    pub(crate) fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        debug_assert_eq!(
            self.conversations[&self.default_conversation_id].session_id, session_id,
            "dispatch_user_prompt only valid for the default conversation",
        );
        let cid = self.default_conversation_id.clone();
        self.dispatch_prompt_for_conversation(&cid, text)
    }

    /// Dispatches one prompt for `cid`: publishes the
    /// `UiPromptSubmitted` event (head-bounced via
    /// `publish_for_conversation` so it lands on the conversation's
    /// branch), enters `AgentThinking`, and asks the agent for a
    /// completion.
    ///
    /// Used for both interactive user prompts on the default
    /// conversation and side-query prompts spawned by extensions.
    pub(crate) fn dispatch_prompt_for_conversation(
        &mut self,
        cid: &ConversationId,
        text: String,
    ) -> Result<(), HarnessError> {
        let (session_id, originator) = match self.conversations.get(cid) {
            Some(c) => (c.session_id.clone(), c.originator.clone()),
            None => {
                return Err(HarnessError::Participant(format!(
                    "dispatch_prompt_for_conversation: unknown conversation `{cid}`"
                )));
            }
        };
        self.publish_for_conversation(
            cid,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id: session_id.clone(),
                text,
                originator,
                ctx_id: None,
            }),
        );
        if self.pending_intercept.is_some() || !self.deferred_publishes.is_empty() {
            // Publish parked in interception (or queued behind one
            // that is). Defer the agent dispatch until the user-
            // prompt event actually commits — see
            // `react_to_committed_event` for the drain.
            self.pending_user_prompt_dispatches.push_back(cid.clone());
        } else {
            // Publish committed inline. Safe to dispatch the agent
            // prompt now: the SessionTree already reflects the new
            // user message, so the message list assembled inside
            // `send_prompt_to_agent_for` will include it.
            self.send_prompt_to_agent_for(cid);
        }
        Ok(())
    }

    /// Drains every runnable conversation's pending prompt queue.
    ///
    /// There is no global agent slot — the agent extension serializes
    /// its own consumption of `SessionPromptCreated`. The harness emits
    /// one prompt per runnable conversation (Idle turn state, non-empty
    /// queue) and routes responses back via `prompt_conversations`.
    ///
    /// Session initialization still happens before prompt dispatch, so
    /// a fresh `chat-*` session can discover AGENTS.md and skills before
    /// the agent sees the first user message.
    pub(crate) fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || self.selected_model.is_none()
        {
            return;
        }

        while let Some(cid) = self.next_runnable_conversation() {
            let session_id = self
                .conversations
                .get(&cid)
                .map(|c| c.session_id.clone())
                .expect("runnable conversation exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let text = self
                .conversations
                .get_mut(&cid)
                .and_then(|c| c.pending_prompts.pop_front())
                .expect("runnable conversation has a prompt");
            if let Err(error) = self.dispatch_prompt_for_conversation(&cid, text) {
                self.emit_info(&format!("failed to dispatch queued prompt: {error}"));
                // Reset the conversation so it doesn't wedge as
                // AgentThinking with no in-flight prompt.
                if let Some(conv) = self.conversations.get_mut(&cid) {
                    conv.in_flight_prompt = None;
                    conv.turn_state = ConversationTurnState::Idle;
                }
            }
        }
    }

    pub(crate) fn next_runnable_conversation(&self) -> Option<ConversationId> {
        self.conversations
            .iter()
            .find(|(_, conv)| {
                !conv.pending_prompts.is_empty()
                    && matches!(conv.turn_state, ConversationTurnState::Idle)
            })
            .map(|(cid, _)| cid.clone())
    }

    /// True when a fresh prompt for `cid` should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: no model selected, harness mid-init, extensions not yet
    ///   `Ready`;
    /// - per-conversation: that conversation already has a prompt in flight or
    ///   is waiting on tool results.
    pub(crate) fn dispatch_blocked_for(&self, cid: &ConversationId) -> bool {
        if self.selected_model.is_none()
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            return true;
        }
        match self.conversations.get(cid) {
            Some(conv) => !matches!(conv.turn_state, ConversationTurnState::Idle),
            None => true,
        }
    }
}
