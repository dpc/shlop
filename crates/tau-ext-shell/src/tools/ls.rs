//! `ls` tool: directory listing with truncation.

use std::fs;
use std::path::PathBuf;

use tau_proto::CborValue;

use crate::argument::{optional_argument_int, optional_argument_text};
use crate::truncate::truncate_head_plain;

pub(crate) const DEFAULT_LS_LIMIT: usize = 500;

pub(crate) fn run_ls(arguments: &CborValue) -> Result<CborValue, String> {
    let path = optional_argument_text(arguments, "path").unwrap_or_else(|| ".".to_owned());
    let limit = optional_argument_int(arguments, "limit")
        .map(|v| v.max(1) as usize)
        .unwrap_or(DEFAULT_LS_LIMIT);
    let dir_path = PathBuf::from(&path);

    let metadata = fs::metadata(&dir_path)
        .map_err(|e| format!("failed to access {}: {e}", dir_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {}", dir_path.display()));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir_path)
        .map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?
    {
        let entry = entry.map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?;
        let name = entry.file_name();
        let mut display = name.to_string_lossy().into_owned();
        if entry
            .file_type()
            .map_err(|e| format!("failed to read {}: {e}", dir_path.display()))?
            .is_dir()
        {
            display.push('/');
        }
        entries.push(display);
    }
    entries.sort_by_key(|entry| entry.to_lowercase());

    if entries.is_empty() {
        return Ok(CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(dir_path.display().to_string()),
            ),
            (
                CborValue::Text("entries".to_owned()),
                CborValue::Integer(0.into()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("(empty directory)".to_owned()),
            ),
        ]));
    }

    let total_entries = entries.len();
    let displayed: Vec<String> = entries.into_iter().take(limit).collect();
    let limit_reached = total_entries > displayed.len();
    let mut output_text = displayed.join("\n");
    let truncated = truncate_head_plain(&output_text);
    if truncated.was_truncated {
        output_text = truncated.content;
    }

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} entries limit reached. Use limit={} for more.",
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
            CborValue::Text(dir_path.display().to_string()),
        ),
        (
            CborValue::Text("entries".to_owned()),
            CborValue::Integer((total_entries as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ]))
}
