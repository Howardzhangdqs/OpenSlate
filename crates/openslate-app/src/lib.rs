//! Shared application wiring for OpenSlate frontends (CLI today, TUI in P2b).
//!
//! Extracted verbatim from the openslate-cli `wiring` module (P2a, no
//! behavior change): config loading → validation → SQLite store → agent
//! tree → tool registry → RunManager assembly ([`wiring`]), provider
//! construction ([`provider`]), and the approval policy derivation shared
//! by interactive and non-interactive sessions.

pub mod provider;
pub mod wiring;

// Root re-exports: the CLI keeps its historical `wiring::X` paths by aliasing
// this crate (`use openslate_app as wiring;`), while new frontends can use
// the explicit module paths (`openslate_app::wiring::X`).
pub use provider::build_provider_for_model;
pub use wiring::*;
