//! Provider management: authentication, OAuth flows, and model listing.
//!
//! Supports multiple named provider instances with API key or OAuth
//! credentials stored in `~/.local/share/tau/auth.json`.

pub mod oauth;
pub mod resolver;
pub mod storage;

pub use resolver::resolve;
