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
fn nested_spans_inherit_and_override_styles() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    outer: { fg: "red", bg: "dark_blue", bold: true },
                    inner: { fg: "green", italic: true },
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let outer = text.add_style("outer");
    let inner = text.add_style("inner");
    text.push_tree(SpanTree::span(
        outer,
        vec![
            SpanTree::text("outer "),
            SpanTree::span(inner, vec![SpanTree::text("inner")]),
        ],
    ));

    let resolved = theme.resolve(&text);
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].text, "outer ");
    assert_eq!(resolved[0].style.fg, Some(Color::Red));
    assert_eq!(resolved[0].style.bg, Some(Color::DarkBlue));
    assert!(resolved[0].style.bold);
    assert!(!resolved[0].style.italic);

    assert_eq!(resolved[1].text, "inner");
    assert_eq!(resolved[1].style.fg, Some(Color::Green));
    assert_eq!(resolved[1].style.bg, Some(Color::DarkBlue));
    assert!(resolved[1].style.bold);
    assert!(resolved[1].style.italic);
}

#[test]
fn builtin_theme_parses() {
    let theme = Theme::builtin();

    // Spot-check a few expected styles.
    let prompt = theme.resolve_style(&StyleName::new("user.prompt"));
    assert!(prompt.bold);
    assert!(prompt.fg.is_none());

    let tool_err = theme.resolve_style(&StyleName::new("tool.status.error"));
    assert_eq!(tool_err.fg, Some(Color::Red));

    let tool_ok = theme.resolve_style(&StyleName::new("tool.status.success"));
    assert_eq!(tool_ok.fg, Some(Color::Green));

    let extension_status = theme.resolve_style(&StyleName::new("extension.status"));
    assert_eq!(extension_status, ThemeStyle::default());

    let session_status = theme.resolve_style(&StyleName::new("session.status"));
    assert_eq!(session_status, ThemeStyle::default());

    let selected = theme.resolve_style(&StyleName::new("completion.selected"));
    assert!(selected.bold);
    assert_eq!(selected.fg, Some(Color::White));
    assert_eq!(selected.bg, Some(Color::DarkBlue));

    let token_stats = theme.resolve_style(&StyleName::new("token.stats"));
    assert_eq!(token_stats.fg, Some(Color::DarkGrey));

    let delta = theme.resolve_style(&StyleName::new("token.stats.symbol.delta"));
    assert!(delta.bold);

    let sigma = theme.resolve_style(&StyleName::new("token.stats.symbol.sigma"));
    assert!(sigma.bold);
}

#[test]
fn builtin_theme_missing_style_is_default() {
    let theme = Theme::builtin();
    let style = theme.resolve_style(&StyleName::new("nonexistent.style"));
    assert_eq!(style, ThemeStyle::default());
}
