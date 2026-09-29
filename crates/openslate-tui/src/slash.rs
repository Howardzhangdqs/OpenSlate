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
//! |                  |                | Ctrl+M): on = wheel scrolls the chat;      |
//! |                  |                | off = native text selection works (wheel   |
//! |                  |                | degrades to ↑/↓ keys); a transient notice  |
//! |                  |                | reports the new state and the trade-off     |
//! | `/copy [all|tool]`| `Copy`        | copies through the fallback chain (OSC 52 |
//! |                  |                | → file → clipboard tool, same as Ctrl+Y).  |
//! |                  |                | No arg = the last assistant message's raw  |
//! |                  |                | markdown; `all` = the whole transcript as  |
//! |                  |                | plain text; `tool` = the last tool call's  |
//! |                  |                | retained output; other args → a usage      |
//! |                  |                | notice (`usage: /copy [all|tool]`)         |
//! | `/provider`      | `Provider`     | opens the model-management overlay
//! |                  |                | (model-mgmt-2): providers / model library
//! |                  |                | / level mapping over the layered persist
//! |                  |                | writers (hot-swap on save)
//! | anything else `/`| `Unknown`      | error state `unknown command … (try        |
//! |                  |                | /help)`                                     |
//!
//! Notes:
//! * `//text` escapes to a literal `/text` prompt (handled upstream in
//!   `App::handle_start_turn`, REPL semantics — not a command).
//! * `/model` is fully wired in app.rs (`handle_slash`): the switch
//!   itself, error feedback for unknown aliases, and a transient
//!   success notice (`model → <alias>`) on the status bar.

use crate::components::ConfigSummary;

/// One registry entry — the SINGLE truth for a command's name,
/// argument hint and description (slash-1). The completion list,
/// the help overlay's slash lines and (indirectly) this module's
/// `parse` all draw from [`registry`].
pub struct SlashCommandSpec {
    /// Bare command name (`"model"` — no slash).
    pub name: &'static str,
    /// Argument hint rendered in the completion's description column
    /// (`"<alias>"` / `"[all|tool]"` / `""`).
    pub args_hint: &'static str,
    /// One-line Chinese description (completion list + help overlay).
    pub description: &'static str,
}

/// The command registry in RANKING order — the completion filter's
/// stable tiebreak walks this order, so it doubles as the ranking
/// (slash-1). `model` deliberately precedes `mouse`: the spec's
/// dispatch contract pins `/mo` + Tab → `/model `, which the fuzzy
/// prefix class cannot deliver if `mouse` ranked first.
pub fn registry() -> &'static [SlashCommandSpec] {
    const REGISTRY: &[SlashCommandSpec] = &[
        SlashCommandSpec {
            name: "help",
            args_hint: "",
            description: "帮助浮层",
        },
        SlashCommandSpec {
            name: "exit",
            args_hint: "",
            description: "退出（免确认）",
        },
        SlashCommandSpec {
            name: "new",
            args_hint: "",
            description: "新会话·清空转录",
        },
        SlashCommandSpec {
            name: "status",
            args_hint: "",
            description: "会话统计浮层",
        },
        SlashCommandSpec {
            name: "agents",
            args_hint: "",
            description: "agents 浮层",
        },
        SlashCommandSpec {
            name: "model",
            args_hint: "<alias>",
            description: "切换模型别名",
        },
        SlashCommandSpec {
            name: "copy",
            args_hint: "[all|tool]",
            description: "复制输出·兜底存文件",
        },
        SlashCommandSpec {
            name: "mouse",
            args_hint: "",
            description: "鼠标捕获开关",
        },
        // model-mgmt-2: LAST on purpose (historical ranking note: the
        // entry used to be `models`, whose `/mo` prefix class demanded
        // the model-before-mouse ordering above; renamed to `/provider`
        // it owns the `/p…` prefix class alone, so the `/mo`+Tab →
        // `/model ` contract now rests purely on `model` before
        // `mouse`).
        SlashCommandSpec {
            name: "provider",
            args_hint: "",
            description: "模型管理·provider/模型库/级别",
        },
    ];
    REGISTRY
}

/// Argument choices for a command's second-level completion list
/// (slash-1). `copy` is static; `model` derives from the live
/// config's model aliases (sorted — `HashMap` iteration order is
/// unstable and the completion must rank deterministically). Any
/// other command gets an empty list (no argument completion).
pub fn arg_choices(name: &str, config: &ConfigSummary) -> Vec<String> {
    match name {
        "copy" => vec!["all".to_owned(), "tool".to_owned()],
        "model" => config.model_aliases.clone(),
        _ => Vec::new(),
    }
}

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
    /// `/provider` — open the model-management overlay (model-mgmt-2):
    /// providers / model library / level mapping, written back through
    /// the layered `persist` writers with a hot config swap.
    Provider,
    /// `/copy [all|tool]` — copy through the fallback chain (OSC 52 →
    /// file → clipboard tool, Ctrl+Y parity). `arg` is the verbatim
    /// trimmed remainder: `None` = last assistant output (Ctrl+Y
    /// parity), `Some("all")` = the whole transcript as plain text,
    /// `Some("tool")` = the last tool call's retained output; anything
    /// else is validated in `app.rs` (usage notice).
    Copy { arg: Option<String> },
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
        "/provider" => SlashCommand::Provider,
        "/copy" => SlashCommand::Copy {
            arg: arg.map(str::to_owned),
        },
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
        assert_eq!(parse("/copy"), SlashCommand::Copy { arg: None });
        assert_eq!(
            parse("/copy all"),
            SlashCommand::Copy {
                arg: Some("all".into())
            }
        );
        assert_eq!(
            parse("/copy tool"),
            SlashCommand::Copy {
                arg: Some("tool".into())
            }
        );
        // The arg rides verbatim (trimmed) — app.rs validates it.
        assert_eq!(
            parse("/copy  everything "),
            SlashCommand::Copy {
                arg: Some("everything".into())
            }
        );
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

    #[test]
    fn copy_keeps_unknown_args_for_app_validation() {
        // An invalid arg is NOT an unknown command — it stays a Copy so
        // app.rs can answer with the usage notice.
        assert_eq!(
            parse("/copy everything"),
            SlashCommand::Copy {
                arg: Some("everything".into())
            }
        );
        // Trailing whitespace toleration keeps the arg None (no arg).
        assert_eq!(parse("/copy "), SlashCommand::Copy { arg: None });
        assert_eq!(parse("/copy\t"), SlashCommand::Copy { arg: None });
    }

    // ── slash-1: the completion registry ──────────────────────────────

    fn config(aliases: &[&str]) -> ConfigSummary {
        ConfigSummary {
            model_alias: "main".into(),
            model_id: "m".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
            model_aliases: aliases.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn registry_covers_every_parseable_command() {
        let names: Vec<&str> = registry().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec!["help", "exit", "new", "status", "agents", "model", "copy", "mouse", "provider"]
        );
        // Unique names, non-empty descriptions, no stray slashes.
        for spec in registry() {
            assert!(!spec.name.contains('/'), "bare name: {}", spec.name);
            assert!(!spec.description.is_empty(), "desc for {}", spec.name);
        }
    }

    #[test]
    fn model_ranks_before_mouse() {
        // The spec's dispatch contract: `/mo` + Tab completes to
        // `/model `. With `model` earlier in the registry the fuzzy
        // prefix class ranks it first.
        let names: Vec<&str> = registry().iter().map(|s| s.name).collect();
        assert!(
            names.iter().position(|n| *n == "model").unwrap()
                < names.iter().position(|n| *n == "mouse").unwrap()
        );
    }

    #[test]
    fn provider_parses_distinctly_and_keeps_model_as_the_mo_head() {
        // model-mgmt-2 (renamed to `/provider`): the command no longer
        // joins the `/mo` prefix class — that class is {model, mouse}
        // and the contract holds as long as `model` ranks first. The
        // parse arm keeps `/provider` distinct from `/model`.
        assert_eq!(parse("/provider"), SlashCommand::Provider);
        assert_eq!(
            parse("/model fast"),
            SlashCommand::Model {
                alias: "fast".into()
            }
        );
        let names: Vec<&str> = registry().iter().map(|s| s.name).collect();
        let model = names.iter().position(|n| *n == "model").unwrap();
        let mouse = names.iter().position(|n| *n == "mouse").unwrap();
        let provider = names.iter().position(|n| *n == "provider").unwrap();
        assert!(model < mouse, "model stays the /mo head");
        assert!(provider > mouse, "provider stays last (registry tail)");
    }

    #[test]
    fn arg_choices_static_and_dynamic() {
        assert_eq!(
            arg_choices("copy", &config(&[])),
            vec!["all".to_owned(), "tool".to_owned()]
        );
        assert_eq!(
            arg_choices("model", &config(&["fast", "main"])),
            vec!["fast".to_owned(), "main".to_owned()]
        );
        // Commands without argument completion get an empty list.
        assert!(arg_choices("help", &config(&["fast"])).is_empty());
        assert!(arg_choices("nope", &config(&["fast"])).is_empty());
    }
}
