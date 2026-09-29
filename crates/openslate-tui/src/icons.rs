//! Icons — the three-tier glyph vocabulary (theme-1).
//!
//! A single [`Icons`] enum carries the tier (the App picks it from
//! `--icons`/`OPENSLATE_ICONS`, default unicode) and rides INSIDE
//! [`crate::theme::Theme`] — the theme is the single injection source
//! (`ctx.theme.icons`). [`Icons::set`] resolves the tier to a const
//! [`IconSet`] table of `&'static str` glyphs (multi-char allowed,
//! e.g. the ascii `git:` branch sigil).
//!
//! Tiers:
//!
//! * `unicode` (default) — the restyle-1 geometric vocabulary
//!   (bullet/circle/diamond coverage without any font pack);
//! * `nerd` — FULL Nerd-ization: every MARKER slot (prompt/anchor,
//!   circles, gear, status shapes, arrows, branch, brand …) carries a
//!   Nerd Font PUA glyph; the STRUCTURAL slots (line-drawing
//!   connectors, panel corners, meter blocks, braille spinner,
//!   `…`/`·`) stay shared with unicode on purpose — the official
//!   Nerd Fonts (v3.5.1, font cmap dissected) ship NO standard-plane
//!   glyphs for line-drawing/block elements/braille/`…`/`·` — the
//!   base font renders those, so a PUA swap there would only lose
//!   portability. All PUA codepoints are written as `\u{Fxxx}` /
//!   `\u{Exxx}` ESCAPES — the source stays pure ASCII bytes (raw PUA
//!   literals get corrupted through edit channels); a contract test
//!   pins the escapes against `char::from_u32`;
//! * `ascii` — every glyph is width-1 printable ASCII (0x21..=0x7E):
//!   the dumb-terminal/CI/serial-console floor. Mapping table
//!   (theme-1 addendum, micro-tuned only where one context would lose
//!   distinctness — the tool/tree connectors `├`→`|` vs `└`→`` ` ``):
//!
//!   ```text
//!   ›>  ●*  •*  ○o  ✓+  ×x  ◆#  ◇-  ◌.  ◉@  ◐%  ■#  ✦*  ⎇y  ⚡~
//!   ├|  └`  │|  ─-  ╭╮╰╯+  ├┤++ (panel)  █#  ░-  ▕[  ▏]  …...  ↑^  ↓v
//!   ```
//!
//!   The middle-dot separator `·`→`.` and the em-dash `—`→`-` (plus
//!   the dimension/cross `×`→`x` and the cached sigil `⎓`→`c`) join
//!   via [`IconSet::ascii`] + [`localize`] so an ascii frame contains
//!   no stray non-ASCII chrome (user-facing Chinese copy is content,
//!   not chrome — it stays).
//!
//! A fourth, program-only tier exists: [`Icons::Custom`] — a base
//! table with user-level per-slot overrides (`[tui.icons] overrides`
//! in openslate.toml, icons-4) applied through
//! [`IconSet::patch_field`]. The `--icons` flag still picks the BASE
//! tier; the overrides stack on top of it (see `patch_field` for the
//! pinned semantics).

/// The icon tier. Carried by [`crate::theme::Theme`] (single source);
/// `clap::ValueEnum` backs `--icons <unicode|nerd|ascii>` and the
/// `OPENSLATE_ICONS` env fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Icons {
    /// Geometric Unicode (default) — the restyle-1 vocabulary.
    #[default]
    Unicode,
    /// Nerd Font PUA markers — full table, every marker slot.
    Nerd,
    /// Pure printable-ASCII glyphs (width 1, zero font dependency).
    Ascii,
    /// Program-built tier (icons-4): a base table with user-level
    /// per-slot overrides from `[tui.icons] overrides` applied on
    /// top. NOT a CLI value — `#[value(skip)]` keeps it out of
    /// `--icons`; the binary constructs it once at startup
    /// (`Box::leak`ed, process lifetime).
    #[value(skip)]
    Custom(&'static IconSet),
}

/// One tier's resolved glyph table. `Copy`; every field is a
/// `&'static str` (multi-char allowed). `spinner` is a string whose
/// CHARS are the animation frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconSet {
    /// Input prompt / user-message band anchor (`›`).
    pub prompt: &'static str,
    /// Assistant block anchor (`●`; nf-fa-circle in nerd).
    pub anchor: &'static str,
    /// Delegation marker (`●`; nf-fa-link in nerd).
    pub delegate: &'static str,
    /// Running/reasoning bullet (`•`).
    pub bullet: &'static str,
    /// Chain-of-thought row marker (the collapsed reasoning summary
    /// row and the expanded reasoning block's first row). Split from
    /// `bullet` (icons-5) so the two can diverge: `bullet` stays the
    /// TOOL-running marker; nerd makes this the brain (nf-fae-brain)
    /// while unicode/ascii keep the original `•`/`*` — same glyphs as
    /// their `bullet`, zero visual change.
    pub reasoning: &'static str,
    /// Pending / interrupted circle (`○`).
    pub pending: &'static str,
    /// Success check (`✓`).
    pub check: &'static str,
    /// Failure cross (`×`).
    pub cross: &'static str,
    /// Warning bang (`!`).
    pub warn: &'static str,
    /// Agents-tree root anchor (`◆`; nf-fa-diamond in nerd — migrated
    /// off the historical U+F111, whose official name is actually
    /// nf-fa-circle, i.e. the anchor slot's glyph).
    pub diamond: &'static str,
    /// Status phase diamond (`◇`).
    pub phase: &'static str,
    /// Starting phase (`◌`).
    pub starting: &'static str,
    /// Waiting (`◉`).
    pub waiting: &'static str,
    /// Agents running (`◐`).
    pub agents_running: &'static str,
    /// Stopped (`■`).
    pub stopped: &'static str,
    /// Steer (`↳`).
    pub steer: &'static str,
    /// Rate sigil (`⚡`).
    pub zap: &'static str,
    /// Brand/model star (`✦`; nf-fa-star in nerd).
    pub brand: &'static str,
    /// Git branch sigil (`⎇`; ascii spells `git:`; nerd uses
    /// nf-oct-git_branch for the node-branch topology).
    pub branch: &'static str,
    /// Tree/tool connector, continues (`├`).
    pub tee: &'static str,
    /// Tree/tool connector, closes (`└`).
    pub elbow: &'static str,
    /// Vertical stroke (`│`).
    pub vertical: &'static str,
    /// Horizontal stroke / rule (`─`).
    pub horizontal: &'static str,
    /// Streaming tail cursor (`▍`).
    pub cursor: &'static str,
    /// Ellipsis (`…`).
    pub ellipsis: &'static str,
    /// Arrows (`↑ ↓ → ←`).
    pub up: &'static str,
    pub down: &'static str,
    pub right: &'static str,
    pub left: &'static str,
    /// Context meter (`▕ █ ░ ▏`).
    pub meter_left: &'static str,
    pub meter_fill: &'static str,
    pub meter_ground: &'static str,
    pub meter_right: &'static str,
    /// The middle-dot sub-separator (`·`).
    pub dot: &'static str,
    /// Panel frame corners (`╭ ╮ ╰ ╯`) and divider tees (`├ ┤`).
    pub panel_top_left: &'static str,
    pub panel_top_right: &'static str,
    pub panel_bottom_left: &'static str,
    pub panel_bottom_right: &'static str,
    pub panel_tee_left: &'static str,
    pub panel_tee_right: &'static str,
    /// Spinner frames, one char each (braille ×10 / ascii `|/-\`).
    pub spinner: &'static str,
    /// Optional STATIC thinking glyph for the status line's Thinking
    /// state (icons-5: every built-in tier ships `None` — the spinner
    /// frames render, the ordinary path). The Option slot exists so a
    /// user can configure one via `[tui.icons] overrides`
    /// (`thinking = "\uF0EB"`); a terminal cannot rotate a glyph and
    /// fonts carry no frame pair, so an override renders STATIC while
    /// the streaming reasoning content itself supplies the motion.
    pub thinking: Option<&'static str>,
    /// Whether this tier is the pure-ASCII one (gates [`localize`]).
    pub ascii: bool,
    /// Whether this tier is the full-PUA nerd one (gates the
    /// arrow/cross remaps in [`localize`]).
    pub nerd: bool,
}

impl Icons {
    /// Resolve the tier to its glyph table.
    pub fn set(self) -> IconSet {
        match self {
            Icons::Unicode => UNICODE_SET,
            Icons::Nerd => NERD_SET,
            Icons::Ascii => ASCII_SET,
            // The leaked custom table resolves verbatim (`IconSet`
            // is `Copy`).
            Icons::Custom(s) => *s,
        }
    }
}

/// Leak a config-time override value into the `'static` lifetime the
/// table fields require (bounded: once at startup, ≤ the override
/// count).
fn leak_str(value: &str) -> &'static str {
    Box::leak(value.to_string().into_boxed_str())
}

impl IconSet {
    /// Patch ONE glyph slot by its field name (icons-4 user overrides
    /// from `[tui.icons] overrides`). Returns `false` for an unknown
    /// name — nothing changes.
    ///
    /// Pinned semantics:
    ///
    /// * the 41 glyph fields + `thinking` are patchable, in field
    ///   declaration order below — **keep this match in sync with the
    ///   field table**;
    /// * `thinking` patches to `Some` (there is deliberately no
    ///   un-patch back to `None`);
    /// * the `ascii`/`nerd` behavior flags are NOT patchable — they
    ///   stay with the base tier, so `localize` follows the base
    ///   (`Custom` over a nerd base keeps the PUA arrow remaps);
    /// * `blocked()` is not a slot either — a `Custom` tier resolves
    ///   it through the patched `diamond` (wildcard arm);
    /// * values may be multi-char; each value is leaked once for the
    ///   process lifetime.
    pub fn patch_field(&mut self, name: &str, value: &str) -> bool {
        match name {
            "prompt" => self.prompt = leak_str(value),
            "anchor" => self.anchor = leak_str(value),
            "delegate" => self.delegate = leak_str(value),
            "bullet" => self.bullet = leak_str(value),
            "reasoning" => self.reasoning = leak_str(value),
            "pending" => self.pending = leak_str(value),
            "check" => self.check = leak_str(value),
            "cross" => self.cross = leak_str(value),
            "warn" => self.warn = leak_str(value),
            "diamond" => self.diamond = leak_str(value),
            "phase" => self.phase = leak_str(value),
            "starting" => self.starting = leak_str(value),
            "waiting" => self.waiting = leak_str(value),
            "agents_running" => self.agents_running = leak_str(value),
            "stopped" => self.stopped = leak_str(value),
            "steer" => self.steer = leak_str(value),
            "zap" => self.zap = leak_str(value),
            "brand" => self.brand = leak_str(value),
            "branch" => self.branch = leak_str(value),
            "tee" => self.tee = leak_str(value),
            "elbow" => self.elbow = leak_str(value),
            "vertical" => self.vertical = leak_str(value),
            "horizontal" => self.horizontal = leak_str(value),
            "cursor" => self.cursor = leak_str(value),
            "ellipsis" => self.ellipsis = leak_str(value),
            "up" => self.up = leak_str(value),
            "down" => self.down = leak_str(value),
            "right" => self.right = leak_str(value),
            "left" => self.left = leak_str(value),
            "meter_left" => self.meter_left = leak_str(value),
            "meter_fill" => self.meter_fill = leak_str(value),
            "meter_ground" => self.meter_ground = leak_str(value),
            "meter_right" => self.meter_right = leak_str(value),
            "dot" => self.dot = leak_str(value),
            "panel_top_left" => self.panel_top_left = leak_str(value),
            "panel_top_right" => self.panel_top_right = leak_str(value),
            "panel_bottom_left" => self.panel_bottom_left = leak_str(value),
            "panel_bottom_right" => self.panel_bottom_right = leak_str(value),
            "panel_tee_left" => self.panel_tee_left = leak_str(value),
            "panel_tee_right" => self.panel_tee_right = leak_str(value),
            "spinner" => self.spinner = leak_str(value),
            "thinking" => self.thinking = Some(leak_str(value)),
            _ => return false,
        }
        true
    }
}

/// The unicode (default = restyle-1) table.
const UNICODE_SET: IconSet = IconSet {
    prompt: "›",
    anchor: "●",
    delegate: "●",
    bullet: "•",
    reasoning: "•",
    pending: "○",
    check: "✓",
    cross: "×",
    warn: "!",
    diamond: "◆",
    phase: "◇",
    starting: "◌",
    waiting: "◉",
    agents_running: "◐",
    stopped: "■",
    steer: "↳",
    zap: "⚡",
    brand: "✦",
    branch: "⎇",
    tee: "├",
    elbow: "└",
    vertical: "│",
    horizontal: "─",
    cursor: "▍",
    ellipsis: "…",
    up: "↑",
    down: "↓",
    right: "→",
    left: "←",
    meter_left: "▕",
    meter_fill: "█",
    meter_ground: "░",
    meter_right: "▏",
    dot: "·",
    panel_top_left: "╭",
    panel_top_right: "╮",
    panel_bottom_left: "╰",
    panel_bottom_right: "╯",
    panel_tee_left: "├",
    panel_tee_right: "┤",
    spinner: "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏",
    thinking: None,
    ascii: false,
    nerd: false,
};

/// The nerd table — FULL Nerd-ization: every marker slot carries a
/// Nerd Font PUA glyph (codepoints calibrated against the official
/// Nerd Fonts v3.5.1 glyphnames.json + CSS + font cmap; FA region
/// unless noted). The 2026-09 intuitiveness pass tried a cod-region
/// (U+EA60..=U+ECFF, NF v3-only) trio for brand/branch/thinking, but
/// the user's terminal font has NO cod coverage (`scripts/nf_probe.py`
/// — the glyphs rendered as stray CJK); all three settled on the old
/// FA/OCT regions instead. The structural slots (tee/elbow/vertical/
/// horizontal/cursor/ellipsis/meter_*/dot/panel_*/spinner)
/// intentionally stay `..UNICODE_SET`: the official Nerd Fonts ship
/// NO standard-plane glyphs for line-drawing, block elements,
/// braille or `…`/`·` (v3.5.1 font cmap dissected) — the base font
/// renders those, so both non-ascii tiers sharing them is correct.
/// PUA codepoints appear exclusively as `\u{Fxxx}`/`\u{Exxx}` ESCAPES
/// (the source stays pure-ASCII bytes — raw PUA literals get
/// corrupted through edit channels); the contract test pins every
/// escape against `char::from_u32`.
const NERD_SET: IconSet = IconSet {
    // nf-fa-angle_right U+F105 -> the `›` input prompt / user band.
    prompt: "\u{F105}",
    // nf-fa-circle U+F111 -> the assistant `●` block anchor.
    anchor: "\u{F111}",
    // nf-fa-link U+F0C1 -> the delegation marker.
    delegate: "\u{F0C1}",
    // nf-fa-gear U+F013 -> the TOOL-running `•` marker (the reasoning
    // rows split off to `reasoning` below, icons-5).
    bullet: "\u{F013}",
    // nf-fae-brain U+E28C -> the chain-of-thought row marker. FAE
    // old region = maximum font compatibility (all three user-tested
    // candidates rendered); the FA6-region U+EE9C nf-fa-brain is left
    // for users to pick via [tui.icons] overrides.
    reasoning: "\u{E28C}",
    // nf-fa-circle_o U+F10C -> the pending/interrupted `○`.
    pending: "\u{F10C}",
    // nf-fa-check U+F00C -> the ✓ markers.
    check: "\u{F00C}",
    // nf-fa-times U+F00D -> the × markers.
    cross: "\u{F00D}",
    // nf-fa-exclamation_triangle U+F071 -> the warning `!`.
    warn: "\u{F071}",
    // nf-fa-diamond U+F29F -> the agents-tree root `◆`. Migrated off
    // the historical U+F111 — its official name is nf-fa-circle (the
    // anchor slot now); U+F219 is nf-fa-gem (a filled gem, not the
    // root diamond) — do not "fix" this onto it.
    diamond: "\u{F29F}",
    // nf-oct-diamond U+F4BF -> the status phase `◇` (the FA region
    // has no hollow diamond, hence the cross-region octicon).
    phase: "\u{F4BF}",
    // nf-fa-circle_o_notch U+F1CE -> the starting `◌`.
    starting: "\u{F1CE}",
    // nf-fa-bullseye U+F140 -> the waiting `◉`.
    waiting: "\u{F140}",
    // nf-fa-play U+F04B -> the running-agent `◐`.
    agents_running: "\u{F04B}",
    // nf-fa-stop U+F04D -> the stopped `■`.
    stopped: "\u{F04D}",
    // nf-fa-level_down U+F149 -> the steer `↳`.
    steer: "\u{F149}",
    // nf-fa-bolt U+F0E7 -> the rate `⚡` sigil.
    zap: "\u{F0E7}",
    // nf-fa-star U+F005 -> the brand/model `✦`. Back from the
    // cod-region chat_sparkle U+EC4F: the user's font has no cod
    // coverage (NF v3-only region); the star stays clearest at small
    // sizes (nf-fa-magic U+F0D0 reads as a pencil — rejected).
    brand: "\u{F005}",
    // nf-oct-git_branch U+F418 -> the git `⎇` sigil. User-probed as
    // covered and pinned (nicer than the E0A0 vertical bar; the
    // cod-region git_branch U+EC6F has no coverage on this font).
    branch: "\u{F418}",
    // nf-fa-arrow_up U+F062 / _down U+F063 / _right U+F061 /
    // _left U+F060 -> the `↑↓→←` chrome (token stats, summaries).
    up: "\u{F062}",
    down: "\u{F063}",
    right: "\u{F061}",
    left: "\u{F060}",
    // icons-5: no built-in static thinking glyph — the Thinking state
    // renders the spinner frames like every tier. The Option SLOT
    // stays for [tui.icons] overrides (e.g. the nf-fa-lightbulb
    // U+F0EB that shipped here before icons-5).
    thinking: None,
    nerd: true,
    ..UNICODE_SET
};

/// The ascii table — every glyph printable width-1 ASCII (theme-1
/// addendum mapping, `└`→`` ` `` micro-tuned so tool/tree rows keep
/// connector distinctness).
const ASCII_SET: IconSet = IconSet {
    prompt: ">",
    anchor: "*",
    delegate: "*",
    bullet: "*",
    reasoning: "*",
    pending: "o",
    check: "+",
    cross: "x",
    warn: "!",
    diamond: "#",
    phase: "-",
    starting: ".",
    waiting: "@",
    agents_running: "%",
    stopped: "#",
    steer: ">",
    zap: "~",
    brand: "*",
    branch: "git:",
    tee: "|",
    elbow: "`",
    vertical: "|",
    horizontal: "-",
    cursor: "|",
    ellipsis: "...",
    up: "^",
    down: "v",
    right: ">",
    left: "<",
    meter_left: "[",
    meter_fill: "#",
    meter_ground: "-",
    meter_right: "]",
    dot: ".",
    panel_top_left: "+",
    panel_top_right: "+",
    panel_bottom_left: "+",
    panel_bottom_right: "+",
    panel_tee_left: "+",
    panel_tee_right: "+",
    spinner: "|/-\\",
    thinking: None,
    ascii: true,
    nerd: false,
};

/// The approval "blocked" marker — a distinct accessor because the
/// agents-tree root (`diamond`, nf-fa-diamond U+F29F) and the
/// approval banner (nf-fa-ban U+F05E) use different nerd glyphs while
/// sharing the unicode `◆`.
impl Icons {
    /// The approval "blocked" marker (`◆` / nf-fa-ban / `#`).
    pub fn blocked(self) -> &'static str {
        match self {
            // nf-fa-ban U+F05E — FA-region for style unity with the
            // full nerd table (escape-only source; the contract test
            // pins it to char::from_u32). The historical U+F256 is
            // officially nf-fa-hand_stop_o, not an octicon hand.
            Icons::Nerd => "\u{F05E}",
            _ => self.set().diamond,
        }
    }
}

/// Localize a chrome string built at DATA time (meta lines, stored
/// args previews, failure summaries) into the tier's glyphs; user/
/// tool CONTENT must never pass through here.
///
/// * ascii — the full chrome downgrade: `…·—↑↓→←×⎓≥` through `set`
///   (`×`→`x`, `⎓`→`c` — the cached sigil's ascii mnemonic, `≥`
///   widens to `>=`);
/// * nerd — ONLY the glyphs with PUA table slots swap: `↑↓→←` and
///   the `×` cross (token stats `↑50 ↓10`, failure `×` summaries);
///   `…·—⎓≥` have no Nerd Font coverage and stay base-font unicode
///   (identity);
/// * unicode — identity mapping (each char to itself).
pub fn localize(s: &str, set: &IconSet) -> String {
    if !set.ascii && !set.nerd {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if set.nerd {
            // Nerd tier: arrows + cross resolve through the PUA
            // slots; everything else is identity.
            match ch {
                '↑' => out.push_str(set.up),
                '↓' => out.push_str(set.down),
                '→' => out.push_str(set.right),
                '←' => out.push_str(set.left),
                '×' => out.push_str(set.cross),
                other => out.push(other),
            }
        } else {
            // Ascii tier: full chrome downgrade to ASCII mnemonics.
            match ch {
                '…' => out.push_str(set.ellipsis),
                '·' => out.push_str(set.dot),
                '—' => out.push('-'),
                '↑' => out.push_str(set.up),
                '↓' => out.push_str(set.down),
                '→' => out.push_str(set.right),
                '←' => out.push_str(set.left),
                '×' => out.push('x'),
                '⎓' => out.push('c'),
                '≥' => out.push_str(">="),
                other => out.push(other),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ascii tier contract: EVERY glyph field is empty or pure
    /// printable ASCII (0x21..=0x7E) — width-1, zero font dependency.
    #[test]
    fn ascii_tier_is_printable_ascii_only() {
        let g = Icons::Ascii.set();
        let fields = [
            g.prompt,
            g.anchor,
            g.delegate,
            g.bullet,
            g.pending,
            g.check,
            g.cross,
            g.warn,
            g.diamond,
            g.phase,
            g.starting,
            g.waiting,
            g.agents_running,
            g.stopped,
            g.steer,
            g.zap,
            g.brand,
            g.branch,
            g.tee,
            g.elbow,
            g.vertical,
            g.horizontal,
            g.cursor,
            g.ellipsis,
            g.up,
            g.down,
            g.right,
            g.left,
            g.meter_left,
            g.meter_fill,
            g.meter_ground,
            g.meter_right,
            g.dot,
            g.panel_top_left,
            g.panel_top_right,
            g.panel_bottom_left,
            g.panel_bottom_right,
            g.panel_tee_left,
            g.panel_tee_right,
            g.spinner,
            Icons::Ascii.blocked(),
        ];
        assert!(g.ascii);
        assert_eq!(g.thinking, None, "ascii tier has no static thinking glyph");
        for f in fields {
            assert!(
                f.chars().all(|c| (0x21..=0x7E).contains(&(c as u32))),
                "ascii tier glyph {f:?} must be printable ASCII"
            );
        }
    }

    /// Unicode tier contract: no Nerd Font PUA (U+E000..=U+F8FF)
    /// anywhere; the spinner stays the frozen braille 10-frame table.
    #[test]
    fn unicode_tier_has_no_pua() {
        let g = Icons::Unicode.set();
        let fields = [
            g.prompt,
            g.anchor,
            g.delegate,
            g.bullet,
            g.pending,
            g.check,
            g.cross,
            g.warn,
            g.diamond,
            g.phase,
            g.starting,
            g.waiting,
            g.agents_running,
            g.stopped,
            g.steer,
            g.zap,
            g.brand,
            g.branch,
            g.tee,
            g.elbow,
            g.vertical,
            g.horizontal,
            g.cursor,
            g.ellipsis,
            g.up,
            g.down,
            g.right,
            g.left,
            g.meter_left,
            g.meter_fill,
            g.meter_ground,
            g.meter_right,
            g.dot,
            g.panel_top_left,
            g.panel_top_right,
            g.panel_bottom_left,
            g.panel_bottom_right,
            g.panel_tee_left,
            g.panel_tee_right,
            g.spinner,
        ];
        for f in fields {
            assert!(
                !f.chars().any(|c| (0xE000..=0xF8FF).contains(&(c as u32))),
                "unicode tier must not carry PUA: {f:?}"
            );
        }
        assert_eq!(g.spinner, "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
        assert_eq!(g.spinner.chars().count(), 10);
        assert_eq!(
            g.thinking, None,
            "unicode tier has no static thinking glyph"
        );
        assert_eq!(
            g.reasoning, "•",
            "reasoning rows keep the bullet glyph (zero visual change)"
        );
        assert_eq!(Icons::Unicode.blocked(), "◆");
    }

    /// Nerd tier contract: the FULL marker table — every slot pinned
    /// against `char::from_u32` (the only sanctioned PUA writer), so
    /// the `\u{Fxxx}`/`\u{Exxx}` source escapes can never drift.
    /// Codepoints calibrated against Nerd Fonts v3.5.1
    /// (glyphnames.json + CSS + font cmap).
    #[test]
    fn nerd_tier_pua_pins_to_from_u32() {
        let g = Icons::Nerd.set();
        let pua = |cp: u32| char::from_u32(cp).expect("valid PUA").to_string();
        assert_eq!(g.prompt, pua(0xF105), "nf-fa-angle_right");
        assert_eq!(g.anchor, pua(0xF111), "nf-fa-circle");
        assert_eq!(g.delegate, pua(0xF0C1), "nf-fa-link");
        assert_eq!(g.bullet, pua(0xF013), "nf-fa-gear");
        assert_eq!(
            g.reasoning,
            pua(0xE28C),
            "nf-fae-brain (FAE old region; FA6 U+EE9C left to user overrides)"
        );
        assert_eq!(g.pending, pua(0xF10C), "nf-fa-circle_o");
        assert_eq!(g.check, pua(0xF00C), "nf-fa-check");
        assert_eq!(g.cross, pua(0xF00D), "nf-fa-times");
        assert_eq!(g.warn, pua(0xF071), "nf-fa-exclamation_triangle");
        assert_eq!(
            g.diamond,
            pua(0xF29F),
            "nf-fa-diamond (migrated off U+F111 = nf-fa-circle)"
        );
        assert_eq!(
            g.phase,
            pua(0xF4BF),
            "nf-oct-diamond (no hollow diamond in the FA region)"
        );
        assert_eq!(g.starting, pua(0xF1CE), "nf-fa-circle_o_notch");
        assert_eq!(g.waiting, pua(0xF140), "nf-fa-bullseye");
        assert_eq!(g.agents_running, pua(0xF04B), "nf-fa-play");
        assert_eq!(g.stopped, pua(0xF04D), "nf-fa-stop");
        assert_eq!(g.steer, pua(0xF149), "nf-fa-level_down");
        assert_eq!(g.zap, pua(0xF0E7), "nf-fa-bolt");
        assert_eq!(
            g.brand,
            pua(0xF005),
            "nf-fa-star (no cod-region coverage on the user's font — fa fallback)"
        );
        assert_eq!(
            g.branch,
            pua(0xF418),
            "nf-oct-git_branch (old-region coverage, user-pinned)"
        );
        assert_eq!(g.up, pua(0xF062), "nf-fa-arrow_up");
        assert_eq!(g.down, pua(0xF063), "nf-fa-arrow_down");
        assert_eq!(g.right, pua(0xF061), "nf-fa-arrow_right");
        assert_eq!(g.left, pua(0xF060), "nf-fa-arrow_left");
        assert_eq!(Icons::Nerd.blocked(), pua(0xF05E), "nf-fa-ban (approval)");
        assert_eq!(
            g.thinking,
            None,
            "icons-5: no built-in static thinking glyph (spinner frames; Option slot for overrides)"
        );
        assert!(g.nerd);
        // The structural slots stay shared with unicode — Nerd Fonts
        // v3.5.1 ship no standard-plane line-drawing/block/braille
        // coverage (font cmap dissected), the base font renders them.
        assert_eq!(g.tee, "├");
        assert_eq!(g.elbow, "└");
        assert_eq!(g.vertical, "│");
        assert_eq!(g.horizontal, "─");
        assert_eq!(g.cursor, "▍");
        assert_eq!(g.ellipsis, "…");
        assert_eq!(g.dot, "·");
        assert_eq!(g.meter_fill, "█");
        assert_eq!(g.panel_top_left, "╭");
        assert_eq!(g.spinner, "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
        assert_eq!(g.spinner.chars().count(), 10);
    }

    /// icons-4 per-slot overrides: patching one slot leaves the rest
    /// of the base tier untouched; unknown slots fail without
    /// mutating; the behavior flags stay with the base.
    #[test]
    fn patch_field_overrides_single_slots() {
        let pua = |cp: u32| char::from_u32(cp).expect("valid PUA").to_string();
        let mut g = Icons::Nerd.set();
        assert!(g.patch_field("branch", "\u{F418}"));
        assert_eq!(g.branch, "\u{F418}", "patched slot takes the override");
        assert_eq!(g.brand, pua(0xF005), "unpatched slots keep the nerd values");
        assert_eq!(g.elbow, "└", "structural slots unpolluted");
        assert!(g.nerd, "behavior flags stay with the base");
        // reasoning patches independently of bullet (icons-5 split).
        assert!(g.patch_field("reasoning", "\u{E28C}"));
        assert_eq!(g.reasoning, "\u{E28C}");
        assert_eq!(g.bullet, pua(0xF013), "bullet keeps the gear");
        // `thinking` patches to Some; a unicode base starts at None.
        let mut u = Icons::Unicode.set();
        assert_eq!(u.thinking, None);
        assert!(u.patch_field("thinking", "\u{F0EB}"));
        assert_eq!(u.thinking, Some("\u{F0EB}"));
        // Unknown slot: false, nothing changes.
        let before = g;
        assert!(!g.patch_field("nope", "x"));
        assert_eq!(g, before, "unknown name mutates nothing");
        // ascii base keeps its ascii flag through a multi-char patch.
        let mut a = Icons::Ascii.set();
        assert!(a.patch_field("branch", "git::"));
        assert_eq!(a.branch, "git::", "multi-char values are legal");
        assert!(a.ascii);
        assert!(!a.nerd);
    }

    /// Custom tier contract: `set()` returns the embedded table
    /// verbatim; `blocked()` resolves through the patched diamond.
    #[test]
    fn custom_tier_resolves_to_its_table() {
        let mut g = Icons::Unicode.set();
        assert!(g.patch_field("prompt", "»"));
        assert!(g.patch_field("diamond", "◆!"));
        let table: &'static IconSet = Box::leak(Box::new(g));
        let icons = Icons::Custom(table);
        assert_eq!(icons.set(), *table);
        assert_eq!(icons.set().prompt, "»", "patched slot flows through");
        assert_eq!(icons.set().elbow, "└", "unpatched slots flow through");
        assert_eq!(
            icons.blocked(),
            "◆!",
            "blocked() is not a slot — it rides the patched diamond"
        );
    }

    /// The frozen ascii mapping (addendum table + the two micro-tunes:
    /// `└`→backtick for connector distinctness; panel tees `+`).
    #[test]
    fn ascii_mapping_table() {
        let g = Icons::Ascii.set();
        assert_eq!(g.prompt, ">");
        assert_eq!(g.anchor, "*");
        assert_eq!(g.delegate, "*");
        assert_eq!(g.bullet, "*");
        assert_eq!(
            g.reasoning, "*",
            "same glyph as bullet (zero visual change)"
        );
        assert_eq!(g.pending, "o");
        assert_eq!(g.check, "+");
        assert_eq!(g.cross, "x");
        assert_eq!(g.warn, "!");
        assert_eq!(g.diamond, "#");
        assert_eq!(g.phase, "-");
        assert_eq!(g.starting, ".");
        assert_eq!(g.waiting, "@");
        assert_eq!(g.agents_running, "%");
        assert_eq!(g.stopped, "#");
        assert_eq!(g.zap, "~");
        assert_eq!(g.brand, "*");
        assert_eq!(g.branch, "git:");
        assert_eq!(g.tee, "|");
        assert_eq!(g.elbow, "`");
        assert_eq!(g.vertical, "|");
        assert_eq!(g.horizontal, "-");
        assert_eq!(g.cursor, "|");
        assert_eq!(g.ellipsis, "...");
        assert_eq!(g.up, "^");
        assert_eq!(g.down, "v");
        assert_eq!(g.meter_left, "[");
        assert_eq!(g.meter_fill, "#");
        assert_eq!(g.meter_ground, "-");
        assert_eq!(g.meter_right, "]");
        assert_eq!(g.dot, ".");
        assert_eq!(g.panel_top_left, "+");
        assert_eq!(g.panel_bottom_right, "+");
        assert_eq!(g.panel_tee_left, "+");
        assert_eq!(g.spinner, "|/-\\");
        assert_eq!(Icons::Ascii.blocked(), "#");
    }

    /// Same-context uniqueness: the tool/tree connector pair and the
    /// status trio (brand/bullet/phase) stay distinguishable per tier.
    #[test]
    fn connectors_and_markers_stay_distinct() {
        for icons in [Icons::Unicode, Icons::Nerd, Icons::Ascii] {
            let g = icons.set();
            assert_ne!(g.tee, g.elbow, "tool-row connectors differ");
            assert_ne!(g.up, g.down);
            assert_ne!(g.check, g.cross);
        }
        // Within the status line: brand `*` vs bullet `*` collide in
        // ascii but never co-occur (the status line shows no bullet).
    }

    #[test]
    fn localize_maps_chrome_per_tier() {
        let ascii = Icons::Ascii.set();
        let unicode = Icons::Unicode.set();
        assert_eq!(localize("↑50 ↓10 · ttft …", &ascii), "^50 v10 . ttft ...");
        assert_eq!(localize("tool — decision", &ascii), "tool - decision");
        // The dimension/cross sign and the cached sigil (ascii mnemonics
        // `x` / `c`).
        assert_eq!(localize("60 列 × 12 行", &ascii), "60 列 x 12 行");
        assert_eq!(localize("↑50 ↓10 ⎓3", &ascii), "^50 v10 c3");
        // The min-size guard's `≥` widens to `>=` (1→2 cols, `…`→`...` precedent).
        assert_eq!(localize("≥ 60 列", &ascii), ">= 60 列");
        // Nerd tier: the arrows and the cross resolve through the PUA
        // table slots; `·`/`…` stay identity (no Nerd Font coverage —
        // the base font renders them).
        let nerd = Icons::Nerd.set();
        let pua = |cp: u32| char::from_u32(cp).expect("valid PUA").to_string();
        assert_eq!(
            localize("↑50 ↓10 · ttft …", &nerd),
            format!("{}50 {}10 · ttft …", pua(0xF062), pua(0xF063))
        );
        assert_eq!(
            localize("60 列 × 12 行", &nerd),
            format!("60 列 {} 12 行", pua(0xF00D))
        );
        // The ascii-only downgrades never fire in nerd: `—`/`⎓`/`≥`
        // stay base-font unicode (identity).
        assert_eq!(
            localize("tool — decision ⎓3 ≥ 5", &nerd),
            "tool — decision ⎓3 ≥ 5"
        );
        // Unicode tier: identity.
        assert_eq!(localize("↑50 ↓10 · ttft …", &unicode), "↑50 ↓10 · ttft …");
        assert_eq!(localize("60 列 × 12 行", &unicode), "60 列 × 12 行");
    }
}
