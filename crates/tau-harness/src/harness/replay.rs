//! Late-subscriber replay.
//!
//! When a UI client subscribes after the harness has already emitted
//! events, two replay paths catch it up:
//!
//! - [`Harness::replay_session_events`] pulls durable transcript facts from the
//!   per-session log via [`crate::SessionStore`], filtering on the new
//!   subscriber's selectors and on
//!   [`should_replay_session_event_to_late_subscriber`].
//! - [`Harness::replay_harness_info`] re-emits harness/extension lifecycle
//!   events from the in-memory [`crate::event_log::EventLog`], plus the current
//!   model / effort / context-usage state, so a UI that just joined sees the
//!   same banners and indicators as one that was here from the start.

use tau_proto::{
    Event, EventSelector, Frame, HarnessContextUsageChanged, HarnessModelSelected,
    HarnessModelsAvailable, Message,
};

use crate::harness::{Harness, selector_matches_event};
use crate::model::{efforts_for_model, model_context_window};

impl Harness {
    pub(crate) fn replay_session_events(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let Ok(events) = self.store.session_events(self.current_session_id.as_str()) else {
            return;
        };
        for entry in events {
            if selector_matches_event(selectors, &entry.event)
                && should_replay_session_event_to_late_subscriber(&entry.event)
            {
                let frame = Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                    id: entry.id,
                    event: Box::new(entry.event),
                }));
                let _ = self.bus.send_to(client_id, entry.source.as_deref(), frame);
            }
        }
    }

    /// Replays harness info and extension lifecycle events to a
    /// late-joining client.
    ///
    /// Events that are persisted to the durable per-session log
    /// (`ExtAgentsMdAvailable`, `ExtensionContextReady`, …) are
    /// intentionally NOT replayed here — `replay_session_events`
    /// already delivers them from the durable log on the same
    /// subscribe. Including them here too caused the CLI to render
    /// each "loaded: …" / "session context ready" line twice.
    pub(crate) fn replay_harness_info(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let mut cursor = 0;
        while let Some(entry) = self.event_log.get_next_from(cursor) {
            cursor = entry.seq + 1;
            let dominated = matches!(
                entry.event,
                Event::HarnessInfo(_)
                    | Event::HarnessSessionDir(_)
                    | Event::HarnessUiDir(_)
                    | Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
            );
            if dominated && selector_matches_event(selectors, &entry.event) {
                let _ = self.bus.send_to(
                    client_id,
                    entry.source.as_deref(),
                    Frame::Event(entry.event),
                );
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(models_event));
        }
        let selected_event = Event::HarnessModelSelected(HarnessModelSelected {
            model: self.selected_model.clone(),
            context_window: self
                .selected_model
                .as_ref()
                .and_then(|m| model_context_window(&self.model_registry, m)),
        });
        if selector_matches_event(selectors, &selected_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(selected_event));
        }
        let context_event = Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: self.context_input_tokens,
            cached_tokens: self.context_cached_tokens,
            percent_used: self.context_percent_used,
        });
        if selector_matches_event(selectors, &context_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(context_event));
        }
        let effort_event = Event::HarnessEffortChanged(tau_proto::HarnessEffortChanged {
            level: self.selected_effort,
        });
        if selector_matches_event(selectors, &effort_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(effort_event));
        }
        let levels = self
            .selected_model
            .as_ref()
            .map(|m| efforts_for_model(&self.model_registry, m))
            .unwrap_or_default();
        let levels_event =
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable { levels });
        if selector_matches_event(selectors, &levels_event) {
            let _ = self
                .bus
                .send_to(client_id, None, Frame::Event(levels_event));
        }
    }
}

fn should_replay_session_event_to_late_subscriber(event: &Event) -> bool {
    // Replay final, durable transcript facts, not progress. In
    // particular, skip `AgentResponseUpdated` streaming chunks and
    // `SessionPromptCreated` pending markers, but keep
    // `UiPromptSubmitted` and `AgentResponseFinished` so a resumed UI
    // can reconstruct completed turns.
    match event {
        Event::UiPromptSubmitted(_)
        | Event::SessionPromptSteered(_)
        | Event::SessionUserMessageInjected(_)
        | Event::ToolRequest(_)
        | Event::ToolResult(_)
        | Event::ToolError(_)
        | Event::ShellCommandFinished(_)
        | Event::SessionStarted(_)
        | Event::SessionShutdown(_)
        | Event::ExtAgentsMdAvailable(_)
        | Event::ExtensionContextReady(_)
        | Event::ExtensionEvent(_) => true,
        Event::AgentResponseFinished(response) => response.text.is_some(),
        _ => false,
    }
}
