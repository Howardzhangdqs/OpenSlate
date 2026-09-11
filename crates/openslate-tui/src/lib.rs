//! openslate-tui library surface.
//!
//! The binary (`src/main.rs`) is a thin shell: CLI args, file logging,
//! panic hook, terminal init/restore. Everything testable lives in this
//! lib so `tests/` can link against it (integration tests cannot import
//! from a bin target).
//!
//! Frozen interfaces (P2b): [`action::Action`] + [`action::map_event`],
//! [`event::TuiEvent`] / [`event::TurnSummary`] /
//! [`event::spawn_turn`], [`components::Component`] /
//! [`components::AppCtx`], [`app::App`].

pub mod action;
pub mod app;
pub mod clipboard;
pub mod components;
pub mod event;
pub mod md;
pub mod slash;
pub mod theme;
