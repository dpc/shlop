//! Shared infrastructure for tau extensions.
//!
//! This crate is meant to be a thin support layer for extension
//! processes — anything that every extension wants but is too
//! mechanical to copy-paste into each one. Today that's exactly one
//! thing: a `tracing_subscriber` setup that writes to stderr (which
//! the harness captures into a per-extension log file) and is
//! filtered via the `TAU_EXT_LOG` environment variable.
//!
//! ## Per-extension log targets
//!
//! Each extension is expected to declare a short target string and
//! pass it as `target:` to the `tracing` macros. The convention is:
//!
//! ```ignore
//! pub const LOG_TARGET: &str = "dpc_notifications";
//!
//! tracing::info!(target: LOG_TARGET, "idle deadline armed");
//! ```
//!
//! Then `TAU_EXT_LOG=dpc_notifications=trace,info` filters that
//! extension at trace level while leaving everything else at info.
//! Targets are arbitrary `&'static str` — any name an extension
//! likes — so use one short, distinctive identifier per extension
//! and document it next to the const.
//!
//! ## Why a separate env var
//!
//! `RUST_LOG` is reserved for the wider host environment (the
//! harness, embedded crates, third-party libraries). Extensions
//! deserve their own knob so users can crank one extension to trace
//! without flooding stderr with everything else, and so users who
//! happen to have `RUST_LOG=debug` set globally don't suddenly get
//! verbose extension output by accident.

use tracing_subscriber::EnvFilter;

/// Environment variable controlling extension log filtering. Same
/// syntax as `RUST_LOG` (per-target levels, with a default level).
pub const ENV_VAR: &str = "TAU_EXT_LOG";

/// Default filter applied when `TAU_EXT_LOG` is unset or fails to
/// parse: every target at `info` and above.
pub const DEFAULT_FILTER: &str = "info";

/// Initialize the global `tracing` subscriber for this extension
/// process. Writes to stderr (no ANSI codes — the harness captures
/// stderr into a file), formats events with timestamp, level, and
/// target, and applies the [`ENV_VAR`] filter.
///
/// Safe to call once per process. If a subscriber is already
/// installed (e.g. by another `init_logging` call, or tests), the
/// duplicate-init error is silently ignored so the program keeps
/// running with whatever subscriber was set first.
pub fn init_logging() {
    let filter =
        EnvFilter::try_from_env(ENV_VAR).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        // The harness already records the wall-clock time the
        // extension wrote each line via the file's mtime / log
        // sentinels; including a per-event timestamp is still useful
        // for sub-second ordering.
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Decode the harness-provided `LifecycleConfigure.config` value into
/// the extension's typed configuration struct.
///
/// `Err` carries a human-readable message suitable for stuffing
/// straight into `LifecycleConfigError { message }` — the harness
/// surfaces it verbatim to the user. Avoids the noisy
/// `Semantic(None, "…")` shape `ciborium::de::Error` produces from
/// its derived `Debug` (which is what its `Display` falls back to).
///
/// Errors are flattened to one of:
/// - `<message>` for a plain semantic error (the most common case — missing
///   field, unknown field, type mismatch);
/// - `<message> (at offset N)` if `ciborium` reported one;
/// - prefixed with `CBOR syntax error` / `IO error` for the rarer
///   transport-level cases.
pub fn parse_config<C: serde::de::DeserializeOwned>(
    value: &tau_proto::CborValue,
) -> Result<C, String> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes)
        .map_err(|e| format!("failed to re-encode config payload: {e}"))?;
    ciborium::de::from_reader::<C, _>(&bytes[..]).map_err(format_de_error)
}

fn format_de_error(error: ciborium::de::Error<std::io::Error>) -> String {
    use ciborium::de::Error;
    match error {
        Error::Io(e) => format!("IO error reading config: {e}"),
        Error::Syntax(offset) => format!("CBOR syntax error at offset {offset}"),
        Error::Semantic(offset, msg) => match offset {
            Some(o) => format!("{msg} (at offset {o})"),
            None => msg,
        },
        Error::RecursionLimitExceeded => "config nesting too deep".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    #[serde(default, deny_unknown_fields)]
    struct Demo {
        n: u32,
    }

    impl Default for Demo {
        fn default() -> Self {
            Self { n: 1 }
        }
    }

    fn cbor_from_json(json: serde_json::Value) -> tau_proto::CborValue {
        tau_proto::json_to_cbor(&json)
    }

    #[test]
    fn parse_config_returns_typed_struct() {
        let value = cbor_from_json(serde_json::json!({ "n": 7 }));
        let cfg: Demo = parse_config(&value).expect("parse");
        assert_eq!(cfg, Demo { n: 7 });
    }

    #[test]
    fn parse_config_error_strips_debug_wrapper() {
        let value = cbor_from_json(serde_json::json!({ "wrong": 7 }));
        let err = parse_config::<Demo>(&value).expect_err("should fail");
        // No `Semantic(None, "…")` wrapping: just the message.
        assert!(!err.starts_with("Semantic"), "got: {err}");
        assert!(err.contains("unknown field"), "got: {err}");
        assert!(err.contains("wrong"), "got: {err}");
    }
}
