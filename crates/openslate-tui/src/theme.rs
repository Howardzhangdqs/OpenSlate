//! Semantic theme — palette + icon tier, the single UI truth source
//! (theme-1).
//!
//! * Colors live in ONE place: the [`Palette`] semantic-key struct
//!   with the `DARK`/`LIGHT` consts. Structural slots trace to the
//!   minimax-code truth source (`.ref/minimax-code/.../theme/
//!   palettes.ts`); theme-cyan-1 re-hued the brand family blue →
//!   cyan. [`Theme`] composes its `Style` slots from a palette — the
//!   rest of the crate never sees a literal hex.
//! * The icon tier ([`crate::icons::Icons`]) rides INSIDE the Theme
//!   (`theme.icons`) — one injection channel through
//!   [`crate::components::AppCtx`].
//! * `Theme::new()`/`dark()` is the default appearance; `light()` is
//!   the light board; either can be re-tiered with
//!   [`Theme::with_icons`].
//! * Principles carried over from restyle-1: truecolor `Color::Rgb`,
//!   body text paints the explicit `text` color, and no `REVERSED`
//!   rows anywhere.
//! * [`ANSI`] is the degraded board for terminals without truecolor:
//!   every slot is a 256-color `Color::Indexed` index (ratatui 0.30's
//!   `AnsiValue`; emits `38;5;n`) picked for visual proximity to the
//!   DARK hex (xterm cube/grayscale anchors, each documented at the
//!   definition). Note the three BACKGROUND slots
//!   (`user_message_bg`/`diff_*`) also ride indexed colors, so the
//!   board assumes 256-color support; a pure 16-color terminal tier
//!   is future work (would want named `Color` variants + no bands).

use ratatui::style::{Color, Modifier, Style};

use crate::icons::Icons;

/// `#RRGGBB` → [`Color::Rgb`].
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// 256-color index → [`Color::Indexed`] (ratatui 0.30's name for the
/// classic `AnsiValue`; emits `38;5;n`) — the ANSI board's only
/// constructor, keeping it free of `Rgb` literals.
const fn ansi(n: u8) -> Color {
    Color::Indexed(n)
}

/// The semantic color board — single-point literal definitions
/// (structural slots from the minimax-code `palettes.ts` dark/light
/// boards; the brand family is cyan since theme-cyan-1). `Theme`
/// composes every Style slot from one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// brand = signal = accent.
    pub signal: Color,
    /// Wordmark gradient edge tones (brand tone in the middle).
    pub wordmark_highlight: Color,
    pub wordmark_shadow: Color,
    /// Running-phase accent (spinner).
    pub orbit: Color,
    pub md_heading: Color,
    pub md_code: Color,
    pub md_link: Color,
    pub user_message_bg: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub text: Color,
    pub muted: Color,
    /// dim == line (rules, connectors, frame strokes).
    pub dim: Color,
    pub border: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    /// Editor-style text-selection background (select-1's
    /// drag-to-select highlight; bg-only slot like the bands).
    pub selection_bg: Color,
}

/// The dark palette (default). Structural slots (neutrals, semantic
/// colors, bands) trace to the minimax-code `palettes.ts` dark board;
/// theme-cyan-1 re-hued the brand family blue → cyan (signal /
/// wordmark gradient / md_link / selection).
pub const DARK: Palette = Palette {
    signal: rgb(0x67E8F9),
    wordmark_highlight: rgb(0xA5F3FC),
    wordmark_shadow: rgb(0x22D3EE),
    orbit: rgb(0x1CCDD2),
    md_heading: rgb(0xCBA6F7),
    md_code: rgb(0xA6E3A1),
    md_link: rgb(0x67E8F9),
    user_message_bg: rgb(0x262626),
    diff_added_bg: rgb(0x213A2B),
    diff_removed_bg: rgb(0x4A221D),
    text: rgb(0xD6D6D6),
    muted: rgb(0xADADAD),
    dim: rgb(0x666666),
    border: rgb(0x303030),
    success: rgb(0x28C567),
    warning: rgb(0xFFC340),
    error: rgb(0xFF5E6C),
    // select-1 + theme-cyan-1: editor-style selection cyan
    // (hsl(192,45%,31%) dark-board pick).
    selection_bg: rgb(0x2B6473),
};

/// The light palette — the message-band and diff backgrounds are
/// LIGHT tints here; only the values change, the rendering logic
/// does not. theme-cyan-1 re-hued the brand family blue → cyan
/// (structural slots unchanged from the palettes.ts light board).
pub const LIGHT: Palette = Palette {
    signal: rgb(0x06B6D4),
    wordmark_highlight: rgb(0x22D3EE),
    wordmark_shadow: rgb(0x0E7490),
    orbit: rgb(0x00767D),
    md_heading: rgb(0x8839EF),
    md_code: rgb(0x267A3F),
    md_link: rgb(0x155E75),
    user_message_bg: rgb(0xF5F5F5),
    diff_added_bg: rgb(0xDAFBE1),
    diff_removed_bg: rgb(0xFFEBE9),
    text: rgb(0x303030),
    muted: rgb(0x666666),
    dim: rgb(0x949494),
    border: rgb(0xEDEDED),
    success: rgb(0x008635),
    warning: rgb(0x916300),
    error: rgb(0xE31937),
    // select-1 + theme-cyan-1: light-board selection cyan — dark
    // text keeps contrast without an fg swap.
    selection_bg: rgb(0xA5F3FC),
};

/// The ANSI degraded board (256-color fallback for terminals without
/// truecolor; selected via `--theme ansi`). Each slot approximates the
/// DARK hex hue by xterm-256 cube (idx 16-231 = 16+36r+6g+b over
/// levels 0/95/135/175/215/255) or grayscale (idx 232-255 = 8+10·i)
/// proximity, via `Color::Indexed` (`38;5;n`); the anchor hex is
/// documented per slot for traceability. The three background slots
/// also need 256-color support (see the module doc).
pub const ANSI: Palette = Palette {
    // #67E8F9 → nearest cube (95,215,255).
    signal: ansi(81),
    // #A5F3FC → nearest cube (175,255,255).
    wordmark_highlight: ansi(159),
    // #22D3EE → nearest cube (0,215,255) — adjacent to but distinct
    // from orbit 44 (0,215,215); keeps the 3-step wordmark gradient
    // 159→81→45.
    wordmark_shadow: ansi(45),
    // #1CCDD2 → nearest cube (0,215,215); anchor tier 80/44.
    orbit: ansi(44),
    // #CBA6F7 → anchor tier 140/141; saturated purple keeps the
    // heading identity vs the pastel exact-nearest 183 (215,175,255)
    // which washes toward text.
    md_heading: ansi(141),
    // #A6E3A1 → anchor tier 114/71; pale-green slot for inline code.
    md_code: ansi(114),
    // #67E8F9 → same hue as signal → same index.
    md_link: ansi(81),
    // #262626 → grayscale exact match (232+3 → 8+10·3 = 38).
    user_message_bg: ansi(235),
    // #213A2B → nearest cube (0,95,0) — dark green band.
    diff_added_bg: ansi(22),
    // #4A221D → nearest cube (95,0,0) — dark red band.
    diff_removed_bg: ansi(52),
    // #D6D6D6 → grayscale 208 (anchor tier 252).
    text: ansi(252),
    // #ADADAD → grayscale 138; deliberately below the exact-nearest
    // 248/249 for clearer text/muted separation in degraded envs.
    muted: ansi(245),
    // #666666 → grayscale nearest 98 (anchor tier 240-241).
    dim: ansi(240),
    // #303030 → grayscale exact match (8+10·4 = 48).
    border: ansi(236),
    // #28C567 → anchor tier 114/71; deeper green, distinct from
    // md_code 114 (exact-nearest 41 (0,215,95) reads neon).
    success: ansi(71),
    // #FFC340 → anchor tier 214/220; classic gold/amber.
    warning: ansi(220),
    // #FF5E6C → nearest cube (255,95,95); anchor tier 203/196.
    error: ansi(203),
    // #2B6473 → nearest cube still (0,95,135) — the classic dark
    // cyan selection (editor selection color in 256-color terminals).
    selection_bg: ansi(24),
};

/// The theme selector for `--theme <dark|light|ansi>` (clap ValueEnum;
/// env `OPENSLATE_THEME` fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ThemeMode {
    /// The dark palette (default).
    #[default]
    Dark,
    /// The light palette.
    Light,
    /// The 256-color degraded palette (no-truecolor terminals).
    Ansi,
}

impl ThemeMode {
    /// The palette board for this mode.
    pub const fn palette(self) -> &'static Palette {
        match self {
            ThemeMode::Dark => &DARK,
            ThemeMode::Light => &LIGHT,
            ThemeMode::Ansi => &ANSI,
        }
    }
}

/// Semantic style constants composed from a [`Palette`]. `Copy` so it
/// can be embedded in [`crate::components::AppCtx`] cheaply.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// The icon tier riding along (single injection channel).
    pub icons: Icons,
    /// User `›` anchor (message band first row, input prompt) —
    /// signal + BOLD.
    pub user_label: Style,
    /// Assistant/body text — explicit `text` foreground.
    pub assistant: Style,
    /// Reasoning/thinking text — muted; collapsed summary rows add
    /// BOLD at the render site.
    pub reasoning: Style,
    /// Tool running — accent (`•` marker, live tail cursor).
    pub tool_running: Style,
    /// Tool completed — success (`✓` marker).
    pub tool_success: Style,
    /// Tool failed — error (`×` marker).
    pub tool_failure: Style,
    /// Delegation marker `● agent` — accent.
    pub delegate: Style,
    /// Agent-tree root — signal + BOLD `◆` (root anchor).
    pub agent_active: Style,
    /// Agent-tree running node — accent `◐`.
    pub agent_running: Style,
    /// Agent-tree finished node — muted `✓`.
    pub agent_done: Style,
    /// Agent-tree failed node — error `×`.
    pub agent_failed: Style,
    /// Fine print (session detail rows, `…` placeholders) — dim.
    pub fine: Style,
    /// Panel borders — border.
    pub border: Style,
    /// Status line base — PLAIN (no REVERSED rows anywhere).
    pub status_bar: Style,
    /// Approval banner key hints / titles — warning + BOLD.
    pub approval: Style,
    /// Error state text — error.
    pub error: Style,
    /// Muted/hint text — muted.
    pub muted: Style,
    /// Rules (`─` separators), tree/tool connectors, panel frame
    /// strokes — line (dim).
    pub bar_divider: Style,
    /// Panel headers (`agents` / `session`) — BOLD on text.
    pub header: Style,
    /// Panel header while focused — signal + BOLD.
    pub header_focused: Style,
    /// Overlay panel titles — signal + BOLD.
    pub overlay_title: Style,
    /// Turn-end marker `└ model · Ns · …` — muted (the `⚡` span
    /// uses signal).
    pub turn_marker: Style,
    /// Markdown headings — mdHeading + BOLD.
    pub md_heading: Style,
    /// Markdown inline/fenced code — mdCode.
    pub md_code: Style,
    /// Markdown link text — mdLink.
    pub md_link: Style,
    /// Plain warning — banner body / notices / `!` prefixes.
    pub warning: Style,
    /// Orbit — the running-phase accent: spinner, `◌` starting phase.
    pub orbit: Style,
    /// Background-only user-message band.
    pub user_message_bg: Style,
    /// Background-only diff-added band.
    pub diff_added_bg: Style,
    /// Background-only diff-removed band.
    pub diff_removed_bg: Style,
    /// Background-only selection highlight (select-1's
    /// drag-to-select overlay; text and its fg stay untouched).
    pub selection_bg: Style,
    /// Wordmark gradient edge tone (bright end).
    pub wordmark_highlight: Style,
    /// Wordmark gradient edge tone (dark end).
    pub wordmark_shadow: Style,
    /// Synonym of [`Theme::bar_divider`]: the `line` color for panel
    /// frames, rules and connectors.
    pub line: Style,
    /// interactive-1 hover highlight for clickable UI segments (the
    /// hint-row `? help`, the approval banner's y/n/a buttons, a
    /// completion list row, the `+N ↓` jump hint) — signal +
    /// UNDERLINED + BOLD. Derived from the same `signal` slot as
    /// [`Theme::user_label`], so every board gets it for free and it
    /// can never drift from the brand color.
    pub hover: Style,
}

impl Theme {
    /// Compose every Style slot from a palette board + icon tier.
    pub fn from_palette(p: &Palette, icons: Icons) -> Self {
        let signal = Style::new().fg(p.signal);
        let line = Style::new().fg(p.dim);
        Self {
            icons,
            user_label: signal.add_modifier(Modifier::BOLD),
            assistant: Style::new().fg(p.text),
            reasoning: Style::new().fg(p.muted),
            tool_running: signal,
            tool_success: Style::new().fg(p.success),
            tool_failure: Style::new().fg(p.error),
            delegate: signal,
            agent_active: signal.add_modifier(Modifier::BOLD),
            agent_running: signal,
            agent_done: Style::new().fg(p.muted),
            agent_failed: Style::new().fg(p.error),
            fine: line,
            border: Style::new().fg(p.border),
            status_bar: Style::new(),
            approval: Style::new().fg(p.warning).add_modifier(Modifier::BOLD),
            error: Style::new().fg(p.error),
            muted: Style::new().fg(p.muted),
            bar_divider: line,
            header: Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            header_focused: signal.add_modifier(Modifier::BOLD),
            overlay_title: signal.add_modifier(Modifier::BOLD),
            turn_marker: Style::new().fg(p.muted),
            md_heading: Style::new().fg(p.md_heading).add_modifier(Modifier::BOLD),
            md_code: Style::new().fg(p.md_code),
            md_link: Style::new().fg(p.md_link),
            warning: Style::new().fg(p.warning),
            orbit: Style::new().fg(p.orbit),
            user_message_bg: Style::new().bg(p.user_message_bg),
            diff_added_bg: Style::new().bg(p.diff_added_bg),
            diff_removed_bg: Style::new().bg(p.diff_removed_bg),
            selection_bg: Style::new().bg(p.selection_bg),
            wordmark_highlight: Style::new().fg(p.wordmark_highlight),
            wordmark_shadow: Style::new().fg(p.wordmark_shadow),
            hover: signal
                .add_modifier(Modifier::UNDERLINED)
                .add_modifier(Modifier::BOLD),
            line,
        }
    }

    /// The default dark theme (the cyan board since theme-cyan-1).
    pub fn dark() -> Self {
        Self::from_palette(&DARK, Icons::Unicode)
    }

    /// The light theme (the cyan light board since theme-cyan-1).
    pub fn light() -> Self {
        Self::from_palette(&LIGHT, Icons::Unicode)
    }

    /// The 256-color degraded theme (no-truecolor terminals).
    pub fn ansi() -> Self {
        Self::from_palette(&ANSI, Icons::Unicode)
    }

    /// Re-tier the icons (CLI `--icons`): colors stay, glyphs swap.
    pub fn with_icons(mut self, icons: Icons) -> Self {
        self.icons = icons;
        self
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Historical alias: restyle-1 called the dark theme `new`. Kept so
/// the ~40 existing call sites stay untouched.
impl Theme {
    /// The default dark theme (alias of [`Theme::dark`]).
    pub fn new() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_match_the_cyan_boards() {
        // The ONLY literal-hex contract test in the crate, both boards
        // parameterized. theme-cyan-1 re-hued the brand family
        // blue → cyan (signal/wordmark/md_link/selection); the
        // structural slots still trace to the minimax-code
        // `palettes.ts` dark/light boards.
        for (
            board,
            signal,
            wm_hi,
            wm_sh,
            orbit,
            heading,
            code,
            link,
            band,
            added,
            removed,
            text,
            muted,
            dim,
            border,
            success,
            warning,
            error,
            selection,
        ) in [
            (
                &DARK,
                0x67E8F9u32,
                0xA5F3FC,
                0x22D3EE,
                0x1CCDD2,
                0xCBA6F7,
                0xA6E3A1,
                0x67E8F9,
                0x262626,
                0x213A2B,
                0x4A221D,
                0xD6D6D6,
                0xADADAD,
                0x666666,
                0x303030,
                0x28C567,
                0xFFC340,
                0xFF5E6C,
                // select-1's slot (editor-style selection cyan).
                0x2B6473,
            ),
            (
                &LIGHT, 0x06B6D4, 0x22D3EE, 0x0E7490, 0x00767D, 0x8839EF, 0x267A3F, 0x155E75,
                0xF5F5F5, 0xDAFBE1, 0xFFEBE9, 0x303030, 0x666666, 0x949494, 0xEDEDED, 0x008635,
                0x916300, 0xE31937, 0xA5F3FC,
            ),
        ] {
            assert_eq!(board.signal, rgb(signal));
            assert_eq!(board.wordmark_highlight, rgb(wm_hi));
            assert_eq!(board.wordmark_shadow, rgb(wm_sh));
            assert_eq!(board.orbit, rgb(orbit));
            assert_eq!(board.md_heading, rgb(heading));
            assert_eq!(board.md_code, rgb(code));
            assert_eq!(board.md_link, rgb(link));
            assert_eq!(board.user_message_bg, rgb(band));
            assert_eq!(board.diff_added_bg, rgb(added));
            assert_eq!(board.diff_removed_bg, rgb(removed));
            assert_eq!(board.text, rgb(text));
            assert_eq!(board.muted, rgb(muted));
            assert_eq!(board.dim, rgb(dim));
            assert_eq!(board.border, rgb(border));
            assert_eq!(board.success, rgb(success));
            assert_eq!(board.warning, rgb(warning));
            assert_eq!(board.error, rgb(error));
            assert_eq!(board.selection_bg, rgb(selection));
        }
    }

    /// `Theme::dark()` composes the cyan board (theme-cyan-1): the
    /// slots match the board values slot by slot (colors + the
    /// modifiers the spec names), and it is what `new()`/`default()`
    /// return.
    #[test]
    fn dark_theme_composes_the_cyan_slots() {
        let t = Theme::dark();
        assert_eq!(t.user_label.fg, Some(rgb(0x67E8F9)));
        assert!(t.user_label.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.assistant.fg, Some(rgb(0xD6D6D6)));
        assert_eq!(t.tool_running.fg, Some(rgb(0x67E8F9)));
        assert_eq!(t.tool_success.fg, Some(rgb(0x28C567)));
        assert_eq!(t.tool_failure.fg, Some(rgb(0xFF5E6C)));
        assert_eq!(t.delegate.fg, Some(rgb(0x67E8F9)));
        assert_eq!(t.orbit.fg, Some(rgb(0x1CCDD2)));
        assert_eq!(t.md_heading.fg, Some(rgb(0xCBA6F7)));
        assert!(t.md_heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.md_code.fg, Some(rgb(0xA6E3A1)));
        assert_eq!(t.md_link.fg, Some(rgb(0x67E8F9)));
        assert_eq!(t.line, t.bar_divider);
        assert_eq!(t.line.fg, Some(rgb(0x666666)));
        assert_eq!(t.approval.fg, Some(rgb(0xFFC340)));
        assert!(t.approval.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.user_message_bg.bg, Some(rgb(0x262626)));
        assert_eq!(t.selection_bg.bg, Some(rgb(0x2B6473)));
        assert_eq!(t.wordmark_highlight.fg, Some(rgb(0xA5F3FC)));
        assert_eq!(t.wordmark_shadow.fg, Some(rgb(0x22D3EE)));
        // Aliases.
        assert_eq!(Theme::new().icons, Icons::Unicode);
        assert_eq!(Theme::default().icons, Theme::dark().icons);
        assert_eq!(
            Theme::default().wordmark_highlight,
            Theme::dark().wordmark_highlight
        );
    }

    /// `Theme::light()`: the light board composed — spot-check the
    /// slots whose values CHANGE (light bands, text, signal) and the
    /// carried icon tier (unicode default, re-tierable).
    #[test]
    fn light_theme_composes_the_light_board() {
        let t = Theme::light();
        assert_eq!(t.icons, Icons::Unicode);
        assert_eq!(t.user_label.fg, Some(rgb(0x06B6D4)));
        assert!(t.user_label.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.assistant.fg, Some(rgb(0x303030)));
        assert_eq!(t.muted.fg, Some(rgb(0x666666)));
        assert_eq!(t.line.fg, Some(rgb(0x949494)));
        assert_eq!(t.orbit.fg, Some(rgb(0x00767D)));
        assert_eq!(t.md_heading.fg, Some(rgb(0x8839EF)));
        assert_eq!(t.md_code.fg, Some(rgb(0x267A3F)));
        assert_eq!(t.warning.fg, Some(rgb(0x916300)));
        assert_eq!(t.error.fg, Some(rgb(0xE31937)));
        assert_eq!(t.user_message_bg.bg, Some(rgb(0xF5F5F5)));
        assert_eq!(t.diff_added_bg.bg, Some(rgb(0xDAFBE1)));
        assert_eq!(t.diff_removed_bg.bg, Some(rgb(0xFFEBE9)));
        assert_eq!(t.selection_bg.bg, Some(rgb(0xA5F3FC)));
        assert_eq!(t.wordmark_highlight.fg, Some(rgb(0x22D3EE)));
        assert_eq!(t.wordmark_shadow.fg, Some(rgb(0x0E7490)));
        // Re-tiering keeps the board, swaps glyphs.
        let ascii = t.with_icons(Icons::Ascii);
        assert_eq!(ascii.icons, Icons::Ascii);
        assert_eq!(ascii.assistant, t.assistant, "colors untouched");
    }

    /// interactive-1: the `hover` slot is pinned on all three boards —
    /// signal foreground + UNDERLINED + BOLD (same-source with each
    /// board's `signal`, so the brand color can never drift).
    #[test]
    fn hover_slot_pinned_on_all_three_boards() {
        for (theme, fg) in [
            (Theme::dark(), Color::Rgb(0x67, 0xE8, 0xF9)),
            (Theme::light(), Color::Rgb(0x06, 0xB6, 0xD4)),
            (Theme::ansi(), Color::Indexed(81)),
        ] {
            assert_eq!(theme.hover.fg, Some(fg), "hover fg follows the board");
            assert!(
                theme.hover.add_modifier.contains(Modifier::UNDERLINED),
                "hover is underlined"
            );
            assert!(
                theme.hover.add_modifier.contains(Modifier::BOLD),
                "hover is bold"
            );
            assert_eq!(
                theme.hover.sub_modifier,
                Modifier::empty(),
                "hover subtracts nothing"
            );
        }
    }

    /// No REVERSED anywhere, on either board (parameterized).
    #[test]
    fn no_reversed_rows_on_any_board() {
        for theme in [Theme::dark(), Theme::light()] {
            for style in [
                theme.user_label,
                theme.assistant,
                theme.reasoning,
                theme.tool_running,
                theme.tool_success,
                theme.tool_failure,
                theme.delegate,
                theme.agent_active,
                theme.agent_running,
                theme.agent_done,
                theme.agent_failed,
                theme.fine,
                theme.border,
                theme.status_bar,
                theme.approval,
                theme.error,
                theme.muted,
                theme.bar_divider,
                theme.header,
                theme.header_focused,
                theme.overlay_title,
                theme.turn_marker,
                theme.md_heading,
                theme.md_code,
                theme.md_link,
                theme.warning,
                theme.orbit,
                theme.line,
            ] {
                assert!(
                    !style.add_modifier.contains(Modifier::REVERSED),
                    "REVERSED retired"
                );
            }
            assert_eq!(theme.status_bar, Style::new());
        }
    }

    /// ThemeMode maps to its board (CLI plumbing).
    #[test]
    fn theme_mode_resolves_its_palette() {
        assert_eq!(ThemeMode::Dark.palette(), &DARK);
        assert_eq!(ThemeMode::Light.palette(), &LIGHT);
        assert_eq!(
            Theme::from_palette(ThemeMode::Dark.palette(), Icons::Nerd).icons,
            Icons::Nerd
        );
    }

    /// The ANSI board, slot by slot, pinned to its 256-color index —
    /// each anchor traces to the DARK hex documented at the const
    /// (guards against silent drift while tuning).
    #[test]
    fn ansi_palette_pins_every_slot() {
        assert_eq!(ANSI.signal, ansi(81)); // #67E8F9
        assert_eq!(ANSI.wordmark_highlight, ansi(159)); // #A5F3FC
        assert_eq!(ANSI.wordmark_shadow, ansi(45)); // #22D3EE
        assert_eq!(ANSI.orbit, ansi(44)); // #1CCDD2
        assert_eq!(ANSI.md_heading, ansi(141)); // #CBA6F7
        assert_eq!(ANSI.md_code, ansi(114)); // #A6E3A1
        assert_eq!(ANSI.md_link, ansi(81)); // #67E8F9
        assert_eq!(ANSI.user_message_bg, ansi(235)); // #262626
        assert_eq!(ANSI.diff_added_bg, ansi(22)); // #213A2B
        assert_eq!(ANSI.diff_removed_bg, ansi(52)); // #4A221D
        assert_eq!(ANSI.text, ansi(252)); // #D6D6D6
        assert_eq!(ANSI.muted, ansi(245)); // #ADADAD
        assert_eq!(ANSI.dim, ansi(240)); // #666666
        assert_eq!(ANSI.border, ansi(236)); // #303030
        assert_eq!(ANSI.success, ansi(71)); // #28C567
        assert_eq!(ANSI.warning, ansi(220)); // #FFC340
        assert_eq!(ANSI.error, ansi(203)); // #FF5E6C
        assert_eq!(ANSI.selection_bg, ansi(24)); // #2B6473
                                                 // CLI plumbing: `--theme ansi` resolves this board, and the
                                                 // symmetric constructor composes it (spot-check bg + fg).
        assert_eq!(ThemeMode::Ansi.palette(), &ANSI);
        assert_eq!(Theme::ansi().user_message_bg.bg, Some(ansi(235)));
        assert_eq!(Theme::ansi().selection_bg.bg, Some(ansi(24)));
        assert_eq!(Theme::ansi().user_label.fg, Some(ansi(81)));
        assert!(Theme::ansi()
            .user_label
            .add_modifier
            .contains(Modifier::BOLD));
    }

    /// The ANSI board must not leak a single `Color::Rgb`: a
    /// no-truecolor terminal would mangle or collapse the truecolor
    /// escapes — that is the whole point of the degraded board.
    #[test]
    fn ansi_board_has_no_rgb_leak() {
        for (name, color) in [
            ("signal", ANSI.signal),
            ("wordmark_highlight", ANSI.wordmark_highlight),
            ("wordmark_shadow", ANSI.wordmark_shadow),
            ("orbit", ANSI.orbit),
            ("md_heading", ANSI.md_heading),
            ("md_code", ANSI.md_code),
            ("md_link", ANSI.md_link),
            ("user_message_bg", ANSI.user_message_bg),
            ("diff_added_bg", ANSI.diff_added_bg),
            ("diff_removed_bg", ANSI.diff_removed_bg),
            ("text", ANSI.text),
            ("muted", ANSI.muted),
            ("dim", ANSI.dim),
            ("border", ANSI.border),
            ("success", ANSI.success),
            ("warning", ANSI.warning),
            ("error", ANSI.error),
            ("selection_bg", ANSI.selection_bg),
        ] {
            assert!(
                !matches!(color, Color::Rgb(..)),
                "slot {name} leaked Color::Rgb — ANSI board must be \
                 AnsiValue-only"
            );
        }
    }
}
