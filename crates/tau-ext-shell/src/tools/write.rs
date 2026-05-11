//! `write` tool: overwrite (or create) a file and report a diff.

use std::fs;
use std::path::PathBuf;

use tau_proto::CborValue;

use crate::argument::argument_text;
use crate::diff::{compute_diff, encode_diff};

pub(crate) fn write_file(arguments: &CborValue) -> Result<CborValue, String> {
    let path = argument_text(arguments, "path")?;
    let content = argument_text(arguments, "content")?;
    let path_buf = PathBuf::from(&path);

    if let Some(parent) = path_buf.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }

    // Best-effort read of the existing file so we can diff. If the
    // file doesn't exist (or can't be decoded as utf-8), treat the
    // baseline as empty — every line of `content` becomes an add.
    let original = fs::read_to_string(&path_buf).unwrap_or_default();

    let bytes_written = content.len();
    fs::write(&path_buf, &content).map_err(|error| error.to_string())?;

    let diff = compute_diff(&original, &content);

    Ok(CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path_buf.display().to_string()),
        ),
        (
            CborValue::Text("bytes_written".to_owned()),
            CborValue::Integer((bytes_written as i64).into()),
        ),
        (CborValue::Text("diff".to_owned()), encode_diff(&diff)),
    ]))
}
