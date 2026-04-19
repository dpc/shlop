//! Styled text types for terminal rendering.
//!
//! Content is represented as sequences of [`Span`]s, each pairing a
//! plain-text string with a [`Style`]. Display width is always
//! computable from the text alone — no ANSI escape codes are stored
//! in the data model.

pub use crossterm::style::Color;

/// Visual attributes for a single character cell.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
}

/// A character cell: one character plus its visual style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        Self { ch, style }
    }

    pub fn plain(ch: char) -> Self {
        Self {
            ch,
            style: Style::default(),
        }
    }
}

/// A run of text with a uniform style.
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }
}

/// A sequence of styled spans representing rich text.
///
/// Can be constructed from plain `&str` / `String` (unstyled),
/// a single [`Span`], or a `Vec<Span>`.
#[derive(Clone, Debug, Default)]
pub struct StyledText {
    spans: Vec<Span>,
}

impl StyledText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Total display width in characters.
    pub fn char_count(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }

    /// Returns `true` if there is no text content.
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Converts to a flat sequence of [`Cell`]s (newlines excluded).
    pub fn to_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::new();
        for span in &self.spans {
            for ch in span.text.chars() {
                if ch != '\n' {
                    cells.push(Cell::new(ch, span.style));
                }
            }
        }
        cells
    }
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<Span> for StyledText {
    fn from(span: Span) -> Self {
        Self { spans: vec![span] }
    }
}

impl From<Vec<Span>> for StyledText {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}
