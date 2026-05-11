//! Output-truncation helpers shared by every tool.

/// Maximum lines before truncation kicks in.
pub(crate) const MAX_OUTPUT_LINES: usize = 2000;
/// Maximum bytes before truncation kicks in.
pub(crate) const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Result of a truncation operation.
pub(crate) struct Truncated {
    pub(crate) content: String,
    pub(crate) was_truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
}

pub(crate) fn truncate_head_plain(input: &str) -> Truncated {
    let total_lines = input.lines().count();
    let total_bytes = input.len();

    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: input.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    let mut result = String::new();
    let mut bytes = 0;
    let mut kept_lines = 0;

    for (line_idx, line) in input.lines().enumerate() {
        if kept_lines >= MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        if line_idx > 0 {
            result.push('\n');
            bytes += 1;
        }
        result.push_str(line);
        bytes += line.len();
        kept_lines = line_idx + 1;
    }

    Truncated {
        content: result,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

/// Truncate from the head (keep first lines).  Used by `read`.
pub(crate) fn truncate_head(input: &str) -> Truncated {
    truncate_head_with_notice(input, "Use start_line and line_count to continue reading.")
}

pub(crate) fn truncate_head_with_notice(input: &str, continuation_hint: &str) -> Truncated {
    let mut truncated = truncate_head_plain(input);
    if !truncated.was_truncated {
        return truncated;
    }

    let kept_lines = truncated.content.lines().count();
    truncated.content.push_str(&format!(
        "\n\n[Showing lines 1-{kept_lines} of {} ({} bytes total). \
         {continuation_hint}]",
        truncated.total_lines, truncated.total_bytes
    ));
    truncated
}

/// Truncate from the tail (keep last lines).  Used by `shell`.
pub(crate) fn truncate_tail(input: &str) -> Truncated {
    let all_lines: Vec<&str> = input.lines().collect();
    let total_lines = all_lines.len();
    let total_bytes = input.len();

    if total_lines <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return Truncated {
            content: input.to_owned(),
            was_truncated: false,
            total_lines,
            total_bytes,
        };
    }

    // Walk backwards, accumulating lines until we hit a limit.
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;

    for &line in all_lines.iter().rev() {
        if kept.len() >= MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();

    let first_kept = total_lines - kept.len() + 1;
    let last_kept = total_lines;
    let mut result = format!(
        "[Showing lines {first_kept}-{last_kept} of {total_lines} ({total_bytes} bytes total)]\n\n"
    );
    result.push_str(&kept.join("\n"));

    Truncated {
        content: result,
        was_truncated: true,
        total_lines,
        total_bytes,
    }
}

/// Truncate a single line, appending a marker if truncated.
pub(crate) fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &line[..end])
}
