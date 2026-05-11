//! `read` tool: read a file (optionally a line slice).

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use tau_proto::CborValue;

use crate::argument::{argument_text, optional_argument_int};
use crate::truncate::truncate_head_with_notice;

pub(crate) fn read_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let start_line = parse_read_start_line(arguments)?;
    let line_count = parse_read_line_count(arguments)?;
    let path_buf = PathBuf::from(&path);

    let sliced =
        stream_slice_lines(&path_buf, start_line, line_count).map_err(|error| error.to_string())?;
    let total_lines = sliced.total_lines;
    let truncated = truncate_head_with_notice(
        &sliced.content,
        "Use start_line and line_count to continue reading.",
    );
    let mut entries = vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text(truncated.content),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer((sliced.start_line as i64).into()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer((sliced.line_count as i64).into()),
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ),
    ];
    if truncated.was_truncated {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((truncated.total_bytes as i64).into()),
        ));
    }
    Ok(CborValue::Map(entries))
}

pub(crate) struct ReadSlice {
    pub(crate) content: String,
    pub(crate) start_line: usize,
    pub(crate) line_count: usize,
    /// Total lines in the source. For [`stream_slice_lines`] this is
    /// computed by scanning the rest of the file after the slice ends.
    pub(crate) total_lines: usize,
}

/// Stream `[start_line, start_line+count)` from `path` without
/// slurping the whole file. Continues reading past the slice end only
/// to count remaining lines (so the caller can report `total_lines`
/// for a "showing N of M" hint).
fn stream_slice_lines(
    path: &Path,
    start_line: usize,
    line_count: Option<usize>,
) -> std::io::Result<ReadSlice> {
    use std::io::BufRead as _;

    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut content = String::new();
    let mut kept = 0usize;
    let mut total_lines = 0usize;
    let mut buf = String::new();
    let take = line_count.unwrap_or(usize::MAX);

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        total_lines += 1;
        // 1-based index of this line is `total_lines`. Inside slice
        // window if it's >= start_line and we haven't kept enough yet.
        if total_lines >= start_line && kept < take {
            // Strip a single trailing newline so the join shape
            // matches `slice_lines` (which used `lines().join("\n")`).
            let trimmed = buf.strip_suffix('\n').unwrap_or(&buf);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            if kept > 0 {
                content.push('\n');
            }
            content.push_str(trimmed);
            kept += 1;
        }
    }

    Ok(ReadSlice {
        content,
        start_line,
        line_count: kept,
        total_lines,
    })
}

fn parse_read_start_line(arguments: &CborValue) -> Result<usize, String> {
    match optional_argument_int(arguments, "start_line") {
        None => Ok(1),
        Some(value) if value < 1 => Err("start_line must be >= 1".to_owned()),
        Some(value) => Ok(value as usize),
    }
}

fn parse_read_line_count(arguments: &CborValue) -> Result<Option<usize>, String> {
    match optional_argument_int(arguments, "line_count") {
        None => Ok(None),
        Some(value) if value < 1 => Err("line_count must be >= 1".to_owned()),
        Some(value) => Ok(Some(value as usize)),
    }
}

/// In-memory equivalent of [`stream_slice_lines`], retained for tests
/// that exercise the slicing logic on a string rather than a file.
#[cfg(test)]
pub(crate) fn slice_lines(input: &str, start_line: usize, line_count: Option<usize>) -> ReadSlice {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let start_idx = start_line.saturating_sub(1).min(total_lines);
    let end_idx = match line_count {
        Some(count) => start_idx.saturating_add(count).min(total_lines),
        None => total_lines,
    };
    ReadSlice {
        content: all_lines[start_idx..end_idx].join("\n"),
        start_line,
        line_count: end_idx.saturating_sub(start_idx),
        total_lines,
    }
}
