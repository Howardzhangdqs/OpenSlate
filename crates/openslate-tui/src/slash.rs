//! Slash commands — the TUI subset aligned with the REPL
//! (`/new /status /agents /model <alias> /copy /mouse /help /exit`).
//!
//! This module owns PARSING only (frozen surface — the input component
//! and App routing never change); behavior is wired in
//! `app.rs::handle_slash`. Current wiring (P3 lane-b audit):
//!
//! | command          | parsed variant | behavior (app.rs)                           |
//! |------------------|----------------|---------------------------------------------|
//! | `/help`          | `Help`         | opens the help overlay (modal; closes on    |
//! |                  |                | `?`/`q`/Esc)                                |
//! | `/exit`          | `Exit`         | quits immediately (typed command = explicit |
//! |                  |                | intent, no confirmation modal)              |
//! | `/new`           | `New`          | clears history + transcript and finalizes   |
//! |                  |                | the backing run; rejected with a transient  |
//! |                  |                | notice while a turn runs                    |
//! | `/status`        | `Status`       | forces the sidebar visible and focuses it   |
//! |                  |                | (session panel = the status surface)        |
//! | `/agents`        | `Agents`       | same focus behavior (agents panel)          |
//! | `/model <alias>` | `Model`        | sets the model override (provider is        |
//! |                  |                | rebuilt per turn); unknown alias → error    |
//! |                  |                | state listing available aliases. The switch |
//! |                  |                | is complete: the effective alias shows in   |
//! |                  |                | the session panel and the status bar        |
//! | `/mouse`         | `Mouse`        | toggles terminal mouse capture (same as    |
//! |                  |                | Ctrl+M): on = wheel scrolls the chat;       |
//! |                  |                | off = native text selection works (wheel    |
//! |                  |                | degrades to ↑/↓ keys); a transient notice  |
//! |                  |                | reports the new state and the trade-off     |
//! | `/copy`          | `Copy`         | copies the last assistant message's raw     |
//! |                  |                | markdown to the system clipboard via OSC 52 |
//! |                  |                | (same as Ctrl+Y); `nothing to copy` notice  |
//! |                  |                | when no assistant output exists yet         |
//! | anything else `/`| `Unknown`      | error state `unknown command … (try        |
//! |                  |                | /help)`                                     |
//!
//! Notes:
//! * `//text` escapes to a literal `/text` prompt (handled upstream in
//!   `App::handle_start_turn`, REPL semantics — not a command).
//! * `/model` is fully wired in app.rs (`handle_slash`): the switch
//!   itself, error feedback for unknown aliases, and a transient
//!   success notice (`model → <alias>`) on the status bar.

/// One parsed slash command (TUI subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help` — open the help overlay.
    Help,
    /// `/exit` — quit immediately (typed command = explicit intent, no
    /// confirmation modal).
    Exit,
    /// `/new` — clear conversation history and close the backing run.
    New,
    /// `/status` — session statistics (sidebar session panel focus).
    Status,
    /// `/agents` — agent tree (sidebar agents panel focus).
    Agents,
    /// `/mouse` — toggle terminal mouse capture (Ctrl+M parity): on =
    /// wheel scrolls the chat; off = native text selection works (the
    /// wheel degrades to ↑/↓ keys).
    Mouse,
    /// `/copy` — copy the last assistant message's raw markdown to the
    /// system clipboard via OSC 52 (Ctrl+Y parity).
    Copy,
    /// `/model <alias>` — switch the active model alias. The remainder
    /// after `/model` is the alias verbatim (trimmed); an alias
    /// containing whitespace simply fails the app's known-alias lookup.
    Model { alias: String },
    /// Anything else starting with `/`.
    Unknown { raw: String },
}

/// Parse a submitted input line (already known to start with `/` and not
/// `//`). Mirrors the REPL's `splitn(2, whitespace)` shape.
pub fn parse(input: &str) -> SlashCommand {
    let parts: Vec<&str> = input.splitn(2, char::is_whitespace).collect();
    let command = parts[0];
    let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

    match command {
        "/help" => SlashCommand::Help,
        "/exit" => SlashCommand::Exit,
        "/new" => SlashCommand::New,
        "/status" => SlashCommand::Status,
        "/agents" => SlashCommand::Agents,
        "/mouse" => SlashCommand::Mouse,
        "/copy" => SlashCommand::Copy,
        "/model" => match arg {
            Some(alias) => SlashCommand::Model {
                alias: alias.to_owned(),
            },
            None => SlashCommand::Unknown {
                raw: input.to_owned(),
            },
        },
        _ => SlashCommand::Unknown {
            raw: input.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_tui_subset() {
        assert_eq!(parse("/help"), SlashCommand::Help);
        assert_eq!(parse("/exit"), SlashCommand::Exit);
        assert_eq!(parse("/new"), SlashCommand::New);
        assert_eq!(parse("/status"), SlashCommand::Status);
        assert_eq!(parse("/agents"), SlashCommand::Agents);
        assert_eq!(parse("/mouse"), SlashCommand::Mouse);
        assert_eq!(parse("/copy"), SlashCommand::Copy);
        assert_eq!(
            parse("/model fast"),
            SlashCommand::Model {
                alias: "fast".into()
            }
        );
        assert_eq!(
            parse("/model   main  "),
            SlashCommand::Model {
                alias: "main".into()
            }
        );
    }

    #[test]
    fn unknown_and_missing_arg() {
        assert_eq!(
            parse("/verbose on"),
            SlashCommand::Unknown {
                raw: "/verbose on".into()
            }
        );
        // /model without an alias is unknown (nothing to switch to).
        assert!(matches!(parse("/model"), SlashCommand::Unknown { .. }));
    }

    #[test]
    fn trailing_whitespace_is_tolerated() {
        assert_eq!(parse("/agents "), SlashCommand::Agents);
        assert_eq!(parse("/help\t"), SlashCommand::Help);
        assert_eq!(parse("/status  \n"), SlashCommand::Status);
    }

    #[test]
    fn commands_are_case_sensitive() {
        // REPL parity: lowercase only. Uppercase forms fall through to
        // the unknown-command error.
        assert_eq!(
            parse("/HELP"),
            SlashCommand::Unknown {
                raw: "/HELP".into()
            }
        );
        assert_eq!(
            parse("/Agents"),
            SlashCommand::Unknown {
                raw: "/Agents".into()
            }
        );
    }

    #[test]
    fn model_alias_keeps_remainder_verbatim() {
        // Documented shape: the full trimmed remainder is the alias; a
        // whitespace alias never matches the config, so the app's error
        // (with the available-alias list) fires.
        assert_eq!(
            parse("/model fast extra"),
            SlashCommand::Model {
                alias: "fast extra".into()
            }
        );
    }

    #[test]
    fn bare_slash_and_prefixes_are_unknown() {
        assert_eq!(parse("/"), SlashCommand::Unknown { raw: "/".into() });
        assert_eq!(
            parse("/helpers"),
            SlashCommand::Unknown {
                raw: "/helpers".into()
            }
        );
        assert_eq!(
            parse("/exitcode"),
            SlashCommand::Unknown {
                raw: "/exitcode".into()
            }
        );
        assert_eq!(
            parse("/copier"),
            SlashCommand::Unknown {
                raw: "/copier".into()
            }
        );
    }
}
