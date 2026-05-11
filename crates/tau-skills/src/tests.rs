use super::*;

// -- Frontmatter parsing ------------------------------------------------

#[test]
fn parse_frontmatter_basic() {
    let content = "---\nname: my-skill\ndescription: Does things\n---\n# Body\n";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("Does things")
    );
    assert_eq!(body, "# Body\n");
}

#[test]
fn parse_frontmatter_quoted_values() {
    let content = "---\nname: \"my-skill\"\ndescription: 'A quoted description'\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("A quoted description")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_boolean_field() {
    let content = "---\nname: shown\ndescription: An advertised skill\nadvertise: true\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("advertise").map(String::as_str), Some("true"));
}

#[test]
fn parse_frontmatter_none_when_missing() {
    let content = "# No frontmatter\nJust body content.";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_unclosed() {
    let content = "---\nname: broken\nno closing fence";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_bom() {
    let content = "\u{feff}---\nname: bom-skill\ndescription: Has BOM\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("bom-skill"));
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_comments_and_blanks() {
    let content = "---\n# comment\n\nname: foo\ndescription: bar\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("foo"));
    assert_eq!(fm.get("description").map(String::as_str), Some("bar"));
}

// -- Skill loading from content -----------------------------------------

#[test]
fn load_skill_valid_defaults_to_not_advertised() {
    let content = "---\nname: my-skill\ndescription: Does useful things\n---\n# Instructions";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "my-skill");
    assert_eq!(skill.description, "Does useful things");
    assert!(
        !skill.add_to_prompt,
        "skills must opt into auto-advertising via `advertise: true`"
    );
    assert!(diags.is_empty());
}

#[test]
fn load_skill_advertise_true_opts_into_prompt() {
    let content = "---\nname: shown\ndescription: visible\nadvertise: true\n---\nBody";
    let path = Path::new("/skills/shown/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(skill.add_to_prompt);
}

#[test]
fn load_skill_missing_description() {
    let content = "---\nname: no-desc\n---\n# Body";
    let path = Path::new("/skills/no-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_empty_description() {
    let content = "---\nname: empty\ndescription:\n---\nBody";
    let path = Path::new("/skills/empty/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_advertise_false_or_missing_keeps_default() {
    let content = "---\nname: hidden\ndescription: A hidden skill\nadvertise: false\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(!skill.add_to_prompt);
}

#[test]
fn load_skill_name_fallback_to_parent_dir() {
    let content = "---\ndescription: Inferred name\n---\n";
    let path = Path::new("/skills/inferred-name/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "inferred-name");
}

#[test]
fn load_skill_name_mismatch_warning() {
    let content = "---\nname: wrong-name\ndescription: Mismatch test\n---\n";
    let path = Path::new("/skills/actual-dir/SKILL.md");
    let (_skill, diags) = load_skill_from_content(content, path);
    assert!(diags.iter().any(|d| d.message.contains("does not match")));
}

#[test]
fn load_skill_invalid_name_chars() {
    let content = "---\nname: Bad_Name\ndescription: Invalid chars\n---\n";
    let path = Path::new("/skills/Bad_Name/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "invalid name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("invalid characters"))
    );
}

#[test]
fn load_skill_hyphen_edges_are_skipped() {
    let content = "---\nname: -bad-\ndescription: bad\n---\n";
    let path = Path::new("/skills/-bad-/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("hyphen"))
    );
}

#[test]
fn load_skill_consecutive_hyphens_are_skipped() {
    let content = "---\nname: a--b\ndescription: bad\n---\n";
    let path = Path::new("/skills/a--b/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("consecutive"))
    );
}

#[test]
fn load_skill_empty_name_is_skipped() {
    // No `name:` field and a parent file_name that won't yield one either
    // (root path has no `file_name()`).
    let content = "---\ndescription: nameless\n---\n";
    let path = Path::new("/");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "empty name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("name is empty"))
    );
}

#[test]
fn load_skill_advertise_accepts_case_and_one() {
    for value in ["true", "True", "TRUE", "1"] {
        let content =
            format!("---\nname: shown\ndescription: visible\nadvertise: {value}\n---\nBody");
        let path = Path::new("/skills/shown/SKILL.md");
        let (skill, _diags) = load_skill_from_content(&content, path);
        let skill = skill.expect("should load");
        assert!(skill.add_to_prompt, "advertise: {value} should be truthy");
    }
}

#[test]
fn load_skill_advertise_rejects_other_truthy_words() {
    // `yes` / `on` are not in the accepted set — they stay false silently
    // (documented behavior).
    let content = "---\nname: hidden\ndescription: visible\nadvertise: yes\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    assert!(!skill.expect("should load").add_to_prompt);
}

#[test]
fn parse_frontmatter_crlf() {
    let content = "---\r\nname: crlf\r\ndescription: Has CRLF\r\n---\r\nBody line";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("crlf"));
    assert_eq!(fm.get("description").map(String::as_str), Some("Has CRLF"));
    assert_eq!(body, "Body line");
}

#[test]
fn parse_frontmatter_crlf_mixed_with_multibyte() {
    // Regression for the off-by-one in find_closing_fence with CRLF: any
    // byte-level offset slip would land inside a UTF-8 multibyte char and
    // panic on slice. With correct offsets it just returns the body.
    let content = "---\r\nname: mb\r\ndescription: café ☕\r\n---\r\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("description").map(String::as_str), Some("café ☕"));
    assert_eq!(body, "Body");
}

#[test]
fn skill_base_dir_matches_parent() {
    let content = "---\nname: my-skill\ndescription: x\n---\n";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, _) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.base_dir(), Path::new("/skills/my-skill"));
}

// -- Directory scanning -------------------------------------------------

#[test]
fn discover_skill_md_in_subdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("my-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Test\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("my-skill/SKILL.md"));
}

#[test]
fn discover_root_md_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("standalone.md"),
        "---\nname: standalone\ndescription: A standalone skill\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("standalone.md"));
}

#[test]
fn discover_skips_dot_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let hidden = tmp.path().join(".hidden");
    fs::create_dir_all(&hidden).expect("mkdir");
    fs::write(
        hidden.join("SKILL.md"),
        "---\nname: hidden\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_skips_node_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nm = tmp.path().join("node_modules").join("some-skill");
    fs::create_dir_all(&nm).expect("mkdir");
    fs::write(
        nm.join("SKILL.md"),
        "---\nname: some-skill\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_does_not_recurse_past_skill_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("mkdir");
    fs::write(
        parent.join("SKILL.md"),
        "---\nname: parent\ndescription: Parent skill\n---\n",
    )
    .expect("write");
    fs::write(
        child.join("SKILL.md"),
        "---\nname: child\ndescription: Should not be found\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("parent/SKILL.md"));
}

#[test]
fn discover_nonexistent_dir() {
    let paths = discover_skill_paths(Path::new("/nonexistent/path"));
    assert!(paths.is_empty());
}

// -- Multi-directory loading --------------------------------------------

#[test]
fn load_from_dirs_dedup() {
    let dir1 = tempfile::tempdir().expect("tempdir");
    let dir2 = tempfile::tempdir().expect("tempdir");

    let s1 = dir1.path().join("my-skill");
    fs::create_dir_all(&s1).expect("mkdir");
    fs::write(
        s1.join("SKILL.md"),
        "---\nname: my-skill\ndescription: First\n---\n",
    )
    .expect("write");

    let s2 = dir2.path().join("my-skill");
    fs::create_dir_all(&s2).expect("mkdir");
    fs::write(
        s2.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Second\n---\n",
    )
    .expect("write");

    let result = load_skills_from_dirs(&[dir1.path().to_owned(), dir2.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "First");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision)
    );
}

#[test]
fn load_from_empty_dirs() {
    let result = load_skills_from_dirs(&[]);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.is_empty());
}

#[test]
fn load_from_dirs_is_sorted_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["zebra", "alpha", "mango", "bravo"] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: x\n---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "bravo", "mango", "zebra"]);
}

#[test]
fn discover_does_not_follow_symlinks() {
    // Symlinks would otherwise let a cycle (or a stray link into the
    // user's home) recurse forever. Make sure we just skip them.
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    fs::create_dir_all(&real).expect("mkdir");
    fs::write(
        real.join("SKILL.md"),
        "---\nname: real\ndescription: real skill\n---\n",
    )
    .expect("write");

    let link = tmp.path().join("link");
    symlink(&real, &link).expect("symlink");

    let paths = discover_skill_paths(tmp.path());
    // Only the non-symlinked copy is discovered.
    assert_eq!(paths.len(), 1);
    assert!(paths[0].starts_with(&real));
}

// -- strip_frontmatter --------------------------------------------------

#[test]
fn strip_frontmatter_returns_body() {
    let content = "---\nname: x\n---\nThe body.";
    assert_eq!(strip_frontmatter(content), "The body.");
}

#[test]
fn strip_frontmatter_no_frontmatter() {
    let content = "Just content.";
    assert_eq!(strip_frontmatter(content), "Just content.");
}
