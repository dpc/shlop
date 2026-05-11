//! `edit` tool: targeted exact-string replacements on a file.

use std::fs;
use std::path::PathBuf;

use tau_proto::CborValue;

use crate::argument::{argument_array, argument_text, cbor_map_int, cbor_map_text};
use crate::diff::{compute_diff, encode_diff};

pub(crate) fn edit_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let path_buf = PathBuf::from(&path);

    let original = fs::read_to_string(&path_buf).map_err(|e| e.to_string())?;

    let edits = argument_array(arguments, "edits")?;
    if edits.is_empty() {
        return Err("edits array must not be empty".to_owned());
    }

    // Collect all replacements and validate against the original.
    let mut replacements: Vec<(usize, usize, &str)> = Vec::new();
    for edit in edits {
        let old_text = cbor_map_text(edit, "oldText")
            .ok_or_else(|| "each edit must have a string oldText".to_owned())?;
        let new_text = cbor_map_text(edit, "newText")
            .ok_or_else(|| "each edit must have a string newText".to_owned())?;
        let expected_matches = match cbor_map_int(edit, "expected_matches") {
            Some(n) if n < 0 => {
                return Err("expected_matches must not be negative".to_owned());
            }
            Some(n) => {
                usize::try_from(n).map_err(|_| "expected_matches is too large".to_owned())?
            }
            None => 1,
        };

        if old_text.is_empty() {
            return Err("oldText must not be empty".to_owned());
        }

        let matches: Vec<(usize, &str)> = original.match_indices(old_text).collect();
        let actual_matches = matches.len();
        if actual_matches != expected_matches {
            return Err(format!(
                "matches: expected {expected_matches}, found {actual_matches}"
            ));
        }

        for (start, matched) in matches {
            let end = start + matched.len();
            replacements.push((start, end, new_text));
        }
    }

    // Sort by start position (descending) so we can apply from end to start
    // without invalidating earlier offsets.
    replacements.sort_by(|a, b| b.0.cmp(&a.0));

    // Check for overlapping ranges.
    for pair in replacements.windows(2) {
        // After descending sort: pair[0].start >= pair[1].start.
        // Overlap if pair[1].end > pair[0].start (pair[1] is earlier in file).
        if pair[1].1 > pair[0].0 {
            return Err("overlapping edits".to_owned());
        }
    }

    // Apply replacements from end to start.
    let mut result = original.clone();
    for (start, end, new_text) in &replacements {
        result.replace_range(*start..*end, new_text);
    }

    fs::write(&path_buf, &result).map_err(|e| e.to_string())?;

    let diff = compute_diff(&original, &result);

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("edits_applied".to_owned()),
            CborValue::Integer((replacements.len() as i64).into()),
        ),
        (CborValue::Text("diff".to_owned()), encode_diff(&diff)),
    ]))
}
