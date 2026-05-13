//! Helpers tools use to attach a [`ToolDisplay`] descriptor to their
//! result/error.
//!
//! Tools return `Result<ToolOutput, ToolFailure>`; both carry a
//! `ToolDisplay` next to the existing CBOR payload / error message.
//! The dispatcher in [`crate::tools`] unpacks these into `ToolResult`
//! / `ToolError` events with the descriptor attached.

use tau_proto::{CborValue, ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

/// Success bundle: the CBOR result the agent consumes and the display
/// descriptor the UI consumes.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub result: CborValue,
    pub display: ToolDisplay,
}

/// Error bundle: the message the agent sees, optional structured
/// details (e.g. shell stdout/stderr), and the display descriptor.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    pub message: String,
    pub details: Option<Box<CborValue>>,
    pub display: Box<ToolDisplay>,
}

impl ToolFailure {
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let status_text = error_chip_text(&message);
        Self {
            message,
            details: None,
            display: Box::new(ToolDisplay {
                status: ToolDisplayStatus::Error,
                status_text,
                ..Default::default()
            }),
        }
    }

    pub fn with_args(mut self, args: impl Into<String>) -> Self {
        self.display.args = args.into();
        self
    }

    pub fn with_details(mut self, details: CborValue) -> Self {
        self.details = Some(Box::new(details));
        self
    }
}

impl From<String> for ToolFailure {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

/// Short single-line chip text for an error.  Multi-line / very long
/// messages get collapsed so the inline chip stays readable.
fn error_chip_text(message: &str) -> String {
    let first = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return "err".to_owned();
    }
    const MAX: usize = 64;
    let label = if first.chars().count() <= MAX {
        first.to_owned()
    } else {
        let mut s: String = first.chars().take(MAX.saturating_sub(1)).collect();
        s.push('…');
        s
    };
    format!("err: {label}")
}

/// Build a `ToolDisplayStats` from textual output: lines + bytes.
/// Empty input yields an empty stats block (nothing renders).
pub(crate) fn text_stats(text: &str) -> ToolDisplayStats {
    if text.is_empty() {
        return ToolDisplayStats::default();
    }
    ToolDisplayStats {
        matches: None,
        lines: Some(text.lines().count() as u64),
        bytes: Some(text.len() as u64),
    }
}

/// A standard `Success` display with `args` label and `"ok"` chip.
pub(crate) fn ok_display(args: impl Into<String>) -> ToolDisplay {
    ToolDisplay {
        args: args.into(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}
