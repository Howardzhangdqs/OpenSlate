//! openslate-tui library surface.
//!
//! The binary (`src/main.rs`) is a thin shell: CLI args, file logging,
//! panic hook, server attach, terminal init/restore. Everything
//! testable lives in this lib so `tests/` can link against it
//! (integration tests cannot import from a bin target).
//!
//! web-1: the TUI is a pure client — [`client`] owns the server link
//! (transport + `ServerMsg → TuiEvent` conversion), the engine-era
//! bridges are gone.
//!
//! Frozen interfaces (P2b): [`action::Action`] + [`action::map_event`],
//! [`event::TuiEvent`] / [`event::TurnSummary`],
//! [`components::Component`] / [`components::AppCtx`], [`app::App`].

pub mod action;
pub mod app;
pub mod client;
pub mod clipboard;
pub mod complete;
pub mod components;
pub mod discovery;
pub mod event;
pub mod icons;
pub mod md;
pub mod panel;
pub mod slash;
pub mod theme;
