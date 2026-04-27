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
