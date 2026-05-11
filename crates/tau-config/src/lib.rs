//! User configuration loading.
//!
//! Settings live in `~/.config/tau/` as JSON5: `cli.json5`,
//! `harness.json5`, and `models.json5`, each with an optional
//! `*.d/*.json5` drop-in directory for layered overrides. See
//! [`settings`] for the schema and loader entry points.
//!
//! Resolved-harness types and the user-vs-builtin extension resolver
//! live in `tau-harness` — this crate just owns the on-disk schema.

pub mod atomic;
pub mod settings;
