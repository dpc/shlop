//! Theme system for Tau.
//!
//! Provides a semantic styling layer: code describes *what* a piece of
//! text is (via [`StyleName`]s), and a user-provided [`Theme`] decides
//! *how* it looks (colors, bold, etc.).
//!
//! # Modules
//!
//! - [`text`] — [`ThemedText`], [`ThemedSpan`], [`StyleIdx`]
//! - [`color`] — [`Color`] enum with JSON5-friendly deserialization
//! - [`theme`] — [`ThemeStyle`], [`Theme`], resolution

pub mod color;
pub mod names;
pub mod text;
pub mod theme;

pub use color::Color;
pub use text::{StyleIdx, StyleName, ThemedSpan, ThemedText};
pub use theme::{ResolvedSpan, Theme, ThemeStyle};
