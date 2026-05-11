//! `find` tool: glob-based file search rooted at a directory.

use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use tau_proto::CborValue;

use crate::argument::{argument_text, optional_argument_int, optional_argument_text};
use crate::truncate::truncate_head_plain;

pub(crate) const DEFAULT_FIND_LIMIT: usize = 1000;

pub(crate) fn run_find(arguments: &CborValue) -> Result<CborValue, String> {
    let pattern = argument_text(arguments, "pattern")?;
    let path = optional_argument_text(arguments, "path").unwrap_or_else(|| ".".to_owned());
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_FIND_LIMIT);
    let search_path = PathBuf::from(&path);

    let metadata = fs::metadata(&search_path)
        .map_err(|e| format!("failed to access {}: {e}", search_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {}", search_path.display()));
    }

    let glob = compile_find_glob(&pattern)?;
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(&search_path)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry.map_err(|e| format!("failed to walk {}: {e}", search_path.display()))?;
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let Ok(relative_path) = entry.path().strip_prefix(&search_path) else {
            continue;
        };
        if glob.is_match(relative_path) {
            matches.push(path_to_slash(relative_path));
        }
    }
    matches.sort_by_key(|entry| entry.to_lowercase());

    if matches.is_empty() {
        return Ok(CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(search_path.display().to_string()),
            ),
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text(pattern),
            ),
            (
                CborValue::Text("matches".to_owned()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("no files found matching pattern".to_owned()),
            ),
        ]));
    }

    let total_matches = matches.len();
    let displayed: Vec<String> = matches.into_iter().take(limit).collect();
    let limit_reached = total_matches > displayed.len();
    let mut output_text = displayed.join("\n");
    let truncated = truncate_head_plain(&output_text);
    if truncated.was_truncated {
        output_text = truncated.content;
    }

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern.",
            limit * 2
        ));
    }
    if truncated.was_truncated {
        notices.push("50KB/2000 line output limit reached.".to_owned());
    }
    if !notices.is_empty() {
        output_text.push_str("\n\n[");
        output_text.push_str(&notices.join(" "));
        output_text.push(']');
    }

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(search_path.display().to_string()),
        ),
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((total_matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ]))
}

fn compile_find_glob(pattern: &str) -> Result<GlobSet, String> {
    let glob = Glob::new(pattern).map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|e| format!("failed to compile glob pattern {pattern:?}: {e}"))
}

fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
