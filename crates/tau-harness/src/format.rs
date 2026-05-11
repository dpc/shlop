//! Pretty-printing helpers for harness lifecycle and tool-progress
//! events. Session-entry rendering lives in `tau-session-inspect`; the
//! harness pulls in [`tau_session_inspect::format_session_entry`] for
//! its tree-preview helper.

use tau_core::SessionEntry;
use tau_proto::{Event, ProgressUpdate, ToolProgress};

/// Formats a tool progress event for display.
#[must_use]
pub fn format_tool_progress(progress: &ToolProgress) -> String {
    let mut text = progress.tool_name.to_string();
    if let Some(message) = &progress.message {
        text.push_str(": ");
        text.push_str(message);
    }
    if let Some(ProgressUpdate {
        current: Some(current),
        total: Some(total),
    }) = &progress.progress
    {
        text.push_str(&format!(" ({current}/{total})"));
    }
    text
}

/// Formats an extension lifecycle event for display.
#[must_use]
pub fn format_extension_event(event: &Event) -> String {
    match event {
        Event::ExtensionStarting(s) => format!("extension {} starting", s.extension_name),
        Event::ExtensionReady(r) => format!("extension {} ready", r.extension_name),
        Event::ExtensionExited(e) => format!("extension {} exited", e.extension_name),
        Event::ExtensionRestarting(r) => format!("extension {} restarting", r.extension_name),
        _ => event.name().to_string(),
    }
}

/// One-line preview of a session entry for `/tree` output.
pub(crate) fn render_entry_preview(entry: &SessionEntry) -> String {
    let raw = tau_session_inspect::format_session_entry(entry);
    let single_line: String = raw
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if single_line.chars().count() > 60 {
        let truncated: String = single_line.chars().take(60).collect();
        format!("{truncated}…")
    } else {
        single_line
    }
}
