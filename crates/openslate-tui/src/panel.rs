//! Panel frame — the minimax-code rounded overlay frame (restyle-1).
//!
//! Reference: `.ref/minimax-code/packages/tui/src/tui/widgets/panel-frame.ts`.
//!
//! ```text
//! ╭─ Title ── meta ─╮
//! │ content…        │
//! ├─── (divider) ───┤
//! │ footer          │
//! ╰─────────────────╯
//! ```
//!
//! * border strokes carry the `line` color (#666666);
//! * the title is BOLD in the caller's tone color;
//! * an optional meta (panel-meta-1) closes the header's right slot
//!   (`╭─ Title ─ meta ─╮`) in the plain muted style — the close-key
//!   hint; the title budget yields room first and a still-too-wide
//!   meta drops, keeping the `╮` closing (panel-frame.ts meta math);
//! * content sits 2 columns inside each edge (content width = width − 4);
//! * degenerate areas (width < 12 or height < 6, the panel-frame.ts
//!   budget) render a bare truncated bold title instead of a frame
//!   (the meta never rides the degraded path);
//! * rows never wrap — callers clip/truncate their own content.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Theme;

/// Minimum size below which the frame degrades to a bare bold title
/// (panel-frame.ts `framed = width >= 12 && height >= 6`).
const MIN_FRAME_WIDTH: u16 = 12;
const MIN_FRAME_HEIGHT: u16 = 6;

/// Display width of a string (unicode-width via ratatui).
fn str_width(s: &str) -> usize {
    Span::from(s).width()
}

/// Render the rounded frame around `area` and return the CONTENT rect
/// (`x+2, y+1, width-4, height-2`). `meta`, when given, rides the
/// header's right slot (`╭─ Title ─ meta ─╮`) in the plain muted
/// style — the close-key hint (panel-frame.ts `renderPanelHeader`).
/// Degenerate areas render a bare truncated bold-tone title line and
/// return the area below it.
pub fn render_frame(
    f: &mut Frame,
    area: Rect,
    title: &str,
    tone: Style,
    theme: &Theme,
    meta: Option<&str>,
) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    // theme-1: the frame strokes come from the theme's icon tier
    // (`╭╮╰╯├┤─│` unicode; `+++++-|` ascii).
    let g = theme.icons.set();
    if area.width < MIN_FRAME_WIDTH || area.height < MIN_FRAME_HEIGHT {
        // Degenerate: bold tone title, clipped to the width (panel-
        // frame.ts `truncateToWidth(..., '')`). No meta on this path.
        let clipped: String = title.chars().take(area.width as usize).collect();
        f.render_widget(
            Paragraph::new(Line::styled(clipped, tone)),
            Rect { height: 1, ..area },
        );
        return Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
    }

    let inner_w = area.width as usize;
    // Title budget (panel-frame.ts renderPanelHeader math): with a
    // meta, the title cedes `meta + 8` columns (` {meta} ─╮` plus a
    // separation dash) while ≥6 columns remain for it; a too-wide
    // meta falls back to the wider no-meta budget and the fit check
    // below drops it — title truncates first, meta drops second, the
    // `╮` always closes the row (never overflow, never panic).
    let meta = meta.filter(|m| !m.is_empty());
    let title_budget = match meta {
        Some(m) => {
            let with_meta = inner_w.saturating_sub(str_width(m) + 8);
            if with_meta >= 6 {
                with_meta
            } else {
                inner_w - 6
            }
        }
        // tl+h+sp + sp + h+h + tr
        None => inner_w - 7,
    };
    let title_clipped = clip(title, title_budget.max(1));
    let title_w = str_width(&title_clipped);
    let start_w = title_w + 4; // tl+h+sp+title+sp

    // The meta stays only when `start + {meta} ─╮` fits inside the
    // width (always true on the with-meta budget path; the fallback
    // path re-checks — a short title may still fit a wide meta).
    let meta_fit = meta.filter(|m| start_w + str_width(m) + 4 <= inner_w);
    let fill = match meta_fit {
        Some(m) => inner_w - start_w - str_width(m) - 4,
        None => inner_w - start_w - 1,
    };

    let mut header: Vec<Span<'static>> = vec![
        Span::styled(format!("{}{}", g.panel_top_left, g.horizontal), theme.line),
        Span::styled(" ".to_owned(), theme.line),
        Span::styled(title_clipped, tone),
        Span::styled(" ".to_owned(), theme.line),
    ];
    match meta_fit {
        Some(m) => {
            header.push(Span::styled(g.horizontal.repeat(fill), theme.line));
            header.push(Span::styled(" ".to_owned(), theme.line));
            header.push(Span::styled(m.to_owned(), theme.muted));
            header.push(Span::styled(
                format!(" {}{}", g.horizontal, g.panel_top_right),
                theme.line,
            ));
        }
        None => {
            header.push(Span::styled(
                format!("{}{}", g.horizontal.repeat(fill), g.panel_top_right),
                theme.line,
            ));
        }
    }

    let mut rows: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    rows.push(Line::from(header));
    let body_rows = area.height as usize - 2;
    for _ in 0..body_rows {
        rows.push(Line::from(vec![
            Span::styled(g.vertical.to_owned(), theme.line),
            Span::raw(" ".repeat(inner_w - 2)),
            Span::styled(g.vertical.to_owned(), theme.line),
        ]));
    }
    rows.push(Line::styled(
        format!(
            "{}{}{}",
            g.panel_bottom_left,
            g.horizontal.repeat(inner_w - 2),
            g.panel_bottom_right
        ),
        theme.line,
    ));
    f.render_widget(
        Paragraph::new(rows),
        Rect {
            height: area.height,
            ..area
        },
    );

    Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width - 4,
        height: area.height - 2,
    }
}

/// The frame's mid divider row: `├───…───┤` in the line color.
/// Paints exactly one row; callers place it inside a framed panel.
pub fn render_divider(f: &mut Frame, row: Rect, theme: &Theme) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    let g = theme.icons.set();
    let w = row.width as usize;
    let line: String = format!(
        "{}{}{}",
        g.panel_tee_left,
        g.horizontal.repeat(w.saturating_sub(2)),
        g.panel_tee_right
    );
    f.render_widget(
        Paragraph::new(Line::styled(line, theme.line)),
        Rect { height: 1, ..row },
    );
}

/// Clip `s` to at most `max_cols` display columns (no ellipsis;
/// CJK-safe — mirrors panel-frame.ts `truncateToWidth(..., '')`).
pub fn clip(s: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = str_width(&ch.to_string());
        if used + cw > max_cols {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|f| paint(f)).expect("draw");
        terminal
            .backend()
            .to_string()
            .lines()
            .map(|l| l.split('"').nth(1).unwrap_or("").to_owned())
            .collect()
    }

    /// The frame: rounded corners, `╭─ Title ─╮` header, `│` body
    /// columns, `╰───╯` bottom, title in the tone style.
    #[test]
    fn frame_renders_rounded_border_and_title() {
        let theme = Theme::new();
        let rows = draw(24, 8, |f| {
            render_frame(f, f.area(), "Help", theme.overlay_title, &theme, None);
        });
        assert!(rows[0].starts_with("╭─ Help ─"), "header: {}", rows[0]);
        assert!(rows[0].ends_with('╮'), "header closes: {}", rows[0]);
        for row in &rows[1..7] {
            assert!(
                row.starts_with('│') && row.ends_with('│'),
                "body row: {row}"
            );
        }
        assert_eq!(rows[7], format!("╰{}╯", "─".repeat(22)));
    }

    /// Content rect geometry: 2 columns inside each edge, 2 rows total.
    #[test]
    fn frame_content_rect_is_inset() {
        let theme = Theme::new();
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).expect("terminal");
        let mut content = None;
        terminal
            .draw(|f| {
                content = Some(render_frame(
                    f,
                    f.area(),
                    "X",
                    theme.overlay_title,
                    &theme,
                    None,
                ));
            })
            .expect("draw");
        assert_eq!(
            content.unwrap(),
            Rect::new(2, 1, 26, 8),
            "content = (x+2, y+1, w-4, h-2)"
        );
    }

    /// Degenerate sizes (< 12 cols or < 6 rows): a bare bold-tone
    /// title line only — never a broken frame, never a panic.
    #[test]
    fn frame_degrades_to_bare_title_on_small_areas() {
        let theme = Theme::new();
        for (w, h) in [(1, 1), (2, 2), (10, 10), (24, 5), (5, 24)] {
            draw(w, h, |f| {
                render_frame(f, f.area(), "Title", theme.overlay_title, &theme, None);
            });
        }
        let rows = draw(10, 3, |f| {
            render_frame(f, f.area(), "Title", theme.overlay_title, &theme, None);
        });
        assert!(rows[0].starts_with("Title"), "bare title: {}", rows[0]);
        assert!(!rows.iter().any(|r| r.contains('╭')));
    }

    /// Over-long titles truncate (width-aware, CJK-safe).
    #[test]
    fn frame_truncates_long_titles() {
        let theme = Theme::new();
        let rows = draw(20, 7, |f| {
            render_frame(
                f,
                f.area(),
                "a-very-long-panel-title",
                theme.overlay_title,
                &theme,
                None,
            );
        });
        assert!(rows[0].starts_with("╭─ a-very-lon"), "{}", rows[0]);
        assert!(rows[0].ends_with('╮'), "{}", rows[0]);
        // CJK: each wide char clips whole.
        let rows = draw(16, 7, |f| {
            render_frame(
                f,
                f.area(),
                "标题标题标题",
                theme.overlay_title,
                &theme,
                None,
            );
        });
        assert!(rows[0].contains("标题"), "{}", rows[0]);
        assert!(Span::from(rows[0].trim()).width() <= 16);
    }

    /// None (and an empty-string meta) keep the historical header
    /// bytes: `╭─ Title ─...─╮`, no right slot.
    #[test]
    fn frame_without_meta_is_unchanged() {
        let theme = Theme::new();
        let expected = format!("╭─ Title {}╮", "─".repeat(14));
        let rows = draw(24, 8, |f| {
            render_frame(f, f.area(), "Title", theme.overlay_title, &theme, None);
        });
        assert_eq!(rows[0], expected);
        // Some("") is a falsy meta (minimax-style) → same bytes.
        let rows = draw(24, 8, |f| {
            render_frame(f, f.area(), "Title", theme.overlay_title, &theme, Some(""));
        });
        assert_eq!(rows[0], expected);
    }

    /// panel-meta-1: the meta right slot — `╭─ Title ─ meta ─╮`
    /// closes the header, the meta cells in the plain muted style
    /// (panel-frame.ts renderPanelHeader's muted meta, no BOLD).
    #[test]
    fn frame_meta_rides_the_header_right_slot_muted() {
        let theme = Theme::new();
        let rows = draw(24, 8, |f| {
            render_frame(
                f,
                f.area(),
                "Title",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert_eq!(rows[0], "╭─ Title ─── Esc 关闭 ─╮");
        // The meta text is exactly the muted-styled run on row 0.
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).expect("terminal");
        terminal
            .draw(|f| {
                render_frame(
                    f,
                    f.area(),
                    "Title",
                    theme.overlay_title,
                    &theme,
                    Some("Esc 关闭"),
                );
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        // Buffer cells carry Reset defaults (not None), so compare
        // slot-wise — the muted-fg run of the header row is exactly
        // the meta text (hidden cells under wide glyphs stay Reset).
        let meta_cells: Vec<_> = (0..buf.area.width)
            .map(|x| buf.cell((x, 0)).expect("header cell"))
            .filter(|c| c.style().fg == theme.muted.fg)
            .collect();
        let muted: String = meta_cells.iter().map(|c| c.symbol()).collect();
        assert_eq!(muted, "Esc 关闭");
        assert!(
            meta_cells.iter().all(|c| c.style().add_modifier.is_empty()),
            "the meta cells are plain (no BOLD)"
        );
    }

    /// The narrow ladder (panel-frame.ts math): the title truncates
    /// first (meta kept, budget ≥6), a still-narrower width drops the
    /// meta entirely (short titles may still fit it), and the row
    /// always closes with `╮` at exactly the width — no overflow, no
    /// panic.
    #[test]
    fn frame_meta_narrow_ladder_title_first_then_meta_dropped() {
        let theme = Theme::new();
        // 22 cols: budget = 22−16 = 6 → title clipped to 6, meta
        // kept, fill 0 (the seam shows the two slot spaces).
        let rows = draw(22, 8, |f| {
            render_frame(
                f,
                f.area(),
                "LongTitle",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert_eq!(rows[0], "╭─ LongTi  Esc 关闭 ─╮");
        assert_eq!(Span::from(rows[0].as_str()).width(), 22);
        // 20 cols, long title: budget falls back (4 < 6), start+end
        // overflows → meta dropped, dash fill closes with `╮`.
        let rows = draw(20, 8, |f| {
            render_frame(
                f,
                f.area(),
                "LongTitle",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert_eq!(rows[0], "╭─ LongTitle ──────╮");
        // 20 cols, SHORT title: the fallback budget does not clip it
        // and `meta ─╮` still fits → meta kept.
        let rows = draw(20, 8, |f| {
            render_frame(
                f,
                f.area(),
                "Hi",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert_eq!(rows[0], "╭─ Hi ── Esc 关闭 ─╮");
        // 12 cols (the frame minimum): budget 6, meta dropped, the
        // row closes at exactly 12 cols.
        let rows = draw(12, 6, |f| {
            render_frame(
                f,
                f.area(),
                "LongTitle",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert_eq!(rows[0], "╭─ LongTi ─╮");
        assert!(!rows[0].contains("Esc"), "meta dropped at the minimum");
    }

    /// The degraded path (<12×6) never carries the meta.
    #[test]
    fn frame_degenerate_path_ignores_meta() {
        let theme = Theme::new();
        let rows = draw(10, 3, |f| {
            render_frame(
                f,
                f.area(),
                "Title",
                theme.overlay_title,
                &theme,
                Some("Esc 关闭"),
            );
        });
        assert!(rows[0].starts_with("Title"), "bare title: {}", rows[0]);
        assert!(
            rows.iter().all(|r| !r.contains("Esc")),
            "no meta below the frame budget"
        );
        assert!(!rows.iter().any(|r| r.contains('╭')));
    }

    /// The divider row: `├───┤`.
    #[test]
    fn divider_renders_tee_rows() {
        let theme = Theme::new();
        let rows = draw(10, 1, |f| {
            render_divider(f, f.area(), &theme);
        });
        assert_eq!(rows[0], "├────────┤");
    }

    #[test]
    fn clip_truncates_by_display_width() {
        assert_eq!(clip("abcdef", 4), "abcd");
        assert_eq!(clip("世界世界", 5), "世界");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("", 10), "");
    }
}
