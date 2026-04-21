//! Theme definition and resolution.
//!
//! A [`Theme`] maps [`StyleName`]s to [`ThemeStyle`]s. Resolution
//! takes a [`ThemedText`] and produces [`ResolvedSpan`]s with
//! concrete style attributes.

use std::collections::HashMap;
use std::path::Path;
use std::{fmt, io};

use crate::color::Color;
use crate::text::{StyleName, ThemedText};

/// Visual attributes for a style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct ThemeStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
}

/// A theme: a mapping from style names to visual attributes.
///
/// Styles not present in the map resolve to [`ThemeStyle::default()`]
/// (no formatting).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Theme {
    #[serde(default)]
    styles: HashMap<StyleName, ThemeStyle>,
}

const BUILTIN_THEME: &str = include_str!("../themes/tau.json5");

impl Theme {
    /// Creates an empty theme (everything uses default styling).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the built-in "tau" theme.
    ///
    /// This is the default theme used when no user theme is configured.
    pub fn builtin() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_THEME).expect("built-in theme is valid JSON5")
    }

    /// Loads a theme from a JSON5 file.
    pub fn load(path: &Path) -> Result<Self, ThemeLoadError> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| ThemeLoadError::Io(path.to_path_buf(), e))?;
        Self::parse(&contents).map_err(|e| ThemeLoadError::Parse(path.to_path_buf(), e))
    }

    /// Parses a theme from a JSON5 string.
    pub fn parse(s: &str) -> Result<Self, json5::Error> {
        json5::from_str(s)
    }

    /// Looks up the style for a name, falling back to the default.
    pub fn resolve_style(&self, name: &StyleName) -> ThemeStyle {
        self.styles.get(name).copied().unwrap_or_default()
    }

    /// Resolves a [`ThemedText`] into spans with concrete styles.
    pub fn resolve<'a>(&self, themed: &'a ThemedText) -> Vec<ResolvedSpan<'a>> {
        themed
            .spans()
            .iter()
            .map(|span| {
                let style = themed
                    .style_name(span.style_idx)
                    .map(|name| self.resolve_style(name))
                    .unwrap_or_default();
                ResolvedSpan {
                    text: &span.text,
                    style,
                }
            })
            .collect()
    }
}

/// A span of text with resolved style attributes.
pub struct ResolvedSpan<'a> {
    pub text: &'a str,
    pub style: ThemeStyle,
}

/// Errors that can occur when loading a theme file.
#[derive(Debug)]
pub enum ThemeLoadError {
    Io(std::path::PathBuf, io::Error),
    Parse(std::path::PathBuf, json5::Error),
}

impl fmt::Display for ThemeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "reading {}: {err}", path.display()),
            Self::Parse(path, err) => write!(f, "parsing {}: {err}", path.display()),
        }
    }
}

impl std::error::Error for ThemeLoadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_theme_resolves_to_defaults() {
        let theme = Theme::new();
        let mut text = ThemedText::new();
        let s = text.add_style("whatever");
        text.push(s, "hello");

        let resolved = theme.resolve(&text);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].text, "hello");
        assert_eq!(resolved[0].style, ThemeStyle::default());
    }

    #[test]
    fn named_style_resolves() {
        let theme: Theme = Theme::parse(
            r#"{
                styles: {
                    prompt: { fg: "green", bold: true },
                }
            }"#,
        )
        .expect("valid theme");

        let mut text = ThemedText::new();
        let prompt = text.add_style("prompt");
        text.push(prompt, ">");

        let resolved = theme.resolve(&text);
        assert_eq!(resolved[0].style.fg, Some(Color::Green));
        assert!(resolved[0].style.bold);
        assert!(!resolved[0].style.italic);
    }

    #[test]
    fn default_idx_resolves_to_default_style() {
        let theme: Theme = Theme::parse(
            r#"{
                styles: {
                    prompt: { fg: "red" },
                }
            }"#,
        )
        .expect("valid theme");

        let mut text = ThemedText::new();
        text.push_default("plain text");

        let resolved = theme.resolve(&text);
        assert_eq!(resolved[0].style, ThemeStyle::default());
    }

    #[test]
    fn hex_color_in_theme() {
        let theme: Theme = Theme::parse(
            r##"{
                styles: {
                    custom: { fg: "#ff8800", bg: "#001122" },
                }
            }"##,
        )
        .expect("valid theme");

        let mut text = ThemedText::new();
        let s = text.add_style("custom");
        text.push(s, "colored");

        let resolved = theme.resolve(&text);
        assert_eq!(
            resolved[0].style.fg,
            Some(Color::Rgb {
                r: 0xff,
                g: 0x88,
                b: 0x00
            })
        );
        assert_eq!(
            resolved[0].style.bg,
            Some(Color::Rgb {
                r: 0x00,
                g: 0x11,
                b: 0x22
            })
        );
    }

    #[test]
    fn multiple_spans_resolve_independently() {
        let theme: Theme = Theme::parse(
            r#"{
                styles: {
                    error: { fg: "red", bold: true },
                    muted: { fg: "dark_grey" },
                }
            }"#,
        )
        .expect("valid theme");

        let mut text = ThemedText::new();
        let error = text.add_style("error");
        let muted = text.add_style("muted");
        text.push(error, "ERROR: ");
        text.push(muted, "details here");
        text.push_default(" (ok)");

        let resolved = theme.resolve(&text);
        assert_eq!(resolved.len(), 3);

        assert_eq!(resolved[0].style.fg, Some(Color::Red));
        assert!(resolved[0].style.bold);

        assert_eq!(resolved[1].style.fg, Some(Color::DarkGrey));
        assert!(!resolved[1].style.bold);

        assert_eq!(resolved[2].style, ThemeStyle::default());
    }

    #[test]
    fn builtin_theme_parses() {
        let theme = Theme::builtin();

        // Spot-check a few expected styles.
        let prompt = theme.resolve_style(&StyleName::new("user.prompt"));
        assert!(prompt.bold);
        assert!(prompt.fg.is_none());

        let tool_err = theme.resolve_style(&StyleName::new("tool.error"));
        assert_eq!(tool_err.fg, Some(Color::DarkRed));

        let selected = theme.resolve_style(&StyleName::new("completion.selected"));
        assert!(selected.bold);
        assert_eq!(selected.fg, Some(Color::White));
        assert_eq!(selected.bg, Some(Color::DarkBlue));
    }

    #[test]
    fn builtin_theme_missing_style_is_default() {
        let theme = Theme::builtin();
        let style = theme.resolve_style(&StyleName::new("nonexistent.style"));
        assert_eq!(style, ThemeStyle::default());
    }
}
