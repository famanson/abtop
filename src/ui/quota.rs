use crate::app::App;
use crate::collector::kiro_quota::KiroQuotaFetcher;
use crate::locale::t;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex", "kiro"];

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_quota_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", "²", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Bottom summary: total tokens + rate
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();

    // Split into side-by-side columns: one per known source (CLAUDE | CODEX | KIRO).
    // Columns are always rendered so the panel layout stays stable even when a
    // source has no data yet.
    let num_sources = SOURCES.len() as u16;
    let col_w = inner.width / num_sources;
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals

    for (i, source) in SOURCES.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect {
            x: col_x,
            y: inner.y,
            width: this_w,
            height: content_h,
        };

        let rl = app
            .rate_limits
            .iter()
            .find(|r| r.source.eq_ignore_ascii_case(source));
        let hint = match *source {
            "claude" => t("quota.abtop_setup"),
            "kiro" if app.kiro_quota.is_none() => String::new(),
            "kiro"
                if app
                    .kiro_quota
                    .as_ref()
                    .is_some_and(KiroQuotaFetcher::cache_failed) =>
            {
                t("quota.kiro_cache_failed")
            }
            "kiro" => t("quota.kiro_setup"),
            _ => t("quota.run_codex"),
        };
        draw_source_column(f, col_area, source, &hint, rl, &cpu_grad, theme);
    }

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    let total_label = t("quota.total");
    f.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(
                format!(" {} {}", total_label, fmt_tokens(total_tokens)),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(
                format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(theme.graph_text),
            ),
        ])]),
        bottom_area,
    );
}

fn draw_source_column(
    f: &mut Frame,
    area: Rect,
    source: &str,
    hint: &str,
    rl: Option<&RateLimitInfo>,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let col_w_usize = area.width as usize;
    // Under 12 cells the bar and the countdown's second unit clip the percent and the hours.
    let compact = area.width < 12;
    let (pad, bar_w, pct_w) = if compact {
        ("", 0, 0)
    } else {
        (" ", col_w_usize.saturating_sub(10).clamp(2, 8), 3)
    };

    let Some(rl) = rl else {
        let mut lines = vec![Line::from(Span::styled(
            format!("{pad}{}", source.to_uppercase()),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ))];
        // A clipped "— no" or hint reads as an answer, so compact shows the dash alone.
        if compact {
            lines.push(Line::from(Span::styled(
                "—",
                Style::default().fg(theme.inactive_fg),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("  — {}", t("quota.no_data")),
                Style::default().fg(theme.inactive_fg),
            )));
            if hint.width() < col_w_usize {
                lines.push(Line::from(Span::styled(
                    format!(" {}", hint),
                    Style::default().fg(theme.graph_text),
                )));
            }
        }
        f.render_widget(Paragraph::new(lines), area);
        return;
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let is_stale = rl
        .updated_at
        .is_some_and(|ts| now.saturating_sub(ts) > STALE_SECS);

    // Stale data → dim the source name. Drops the unlabeled "Xs ago" row
    // we used to render here; keeping a distinct freshness color but no
    // explicit number is enough to signal "values may be out of date"
    // without competing with the reset countdown for the user's attention.
    let source_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.title
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{pad}{}", rl.source.to_uppercase()),
        Style::default()
            .fg(source_color)
            .add_modifier(Modifier::BOLD),
    )));

    // Reset countdown is only meaningful when the data is fresh enough
    // that the reported `resets_at` is still in the future. Stale sources
    // get the bar (the % is approximately correct) but no countdown row.
    let show_reset = !is_stale;

    // A window's bar row and reset row. The reset row is always reserved so the
    // columns line up, blank when the source is stale or the reset is past.
    let window_rows = |label: String, used_pct: f64, reset: String| {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let pct = format!("{:>pct_w$.0}%", remaining);
        let mut s = Vec::new();
        // A row exactly the column's width runs into the next column; a clipped percent misreads.
        if label.width() + pct.width() <= col_w_usize {
            s.push(styled_label(&format!("{pad}{label}"), theme.graph_text));
            s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        }
        let sep = if compact && label.width() + 1 + pct.width() >= col_w_usize {
            ""
        } else {
            " "
        };
        let c = grad_at(cpu_grad, used_pct);
        s.push(Span::styled(format!("{sep}{pct}"), Style::default().fg(c)));
        // Each fallback keeps whole units: the prefix and largest unit, then that unit alone.
        let tokens = reset.split(' ').take(2).collect::<Vec<_>>().join(" ");
        let wide = format!("  {reset}");
        let row = if reset.is_empty() {
            String::new()
        } else if !compact {
            if wide.width() > col_w_usize {
                format!("  {tokens}")
            } else {
                wide
            }
        } else if tokens.width() > col_w_usize {
            reset.split(' ').nth(1).unwrap_or_default().to_string()
        } else {
            tokens
        };
        [
            Line::from(s),
            Line::from(Span::styled(row, Style::default().fg(theme.graph_text))),
        ]
    };
    if let Some(used_pct) = rl.five_hour_pct {
        let label = format_window_label(rl.five_hour_window_minutes, t("quota.5h"));
        let reset = rl.five_hour_resets_at.filter(|_| show_reset);
        lines.extend(window_rows(
            label,
            used_pct,
            reset.map(format_reset_time).unwrap_or_default(),
        ));
    }
    if let Some(used_pct) = rl.seven_day_pct {
        let label = format_window_label(rl.seven_day_window_minutes, t("quota.7d"));
        let reset = rl.seven_day_resets_at.filter(|_| show_reset);
        lines.extend(window_rows(
            label,
            used_pct,
            reset.map(format_reset_time).unwrap_or_default(),
        ));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn format_window_label(window_minutes: Option<u64>, fallback: String) -> String {
    let Some(minutes) = window_minutes else {
        return fallback;
    };

    if minutes == 0 {
        fallback
    } else if minutes % (24 * 60) == 0 {
        format!("{}{}", minutes / (24 * 60), t("time.d"))
    } else if minutes % 60 == 0 {
        format!("{}{}", minutes / 60, t("time.h"))
    } else {
        format!("{}{}", minutes, t("time.m"))
    }
}

/// Format a reset timestamp as a human countdown labeled "in X" so the
/// row reads as a time-until-reset. Returns an empty string when the
/// reset is already in the past — the actual next reset depends on the
/// window length which the caller doesn't track, and showing a "now"
/// sentinel was misleading on stale sources where the window had
/// already rolled over multiple times. Callers skip the row when this
/// returns empty.
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if reset_ts <= now {
        return String::new();
    }
    let diff = reset_ts - now;
    let prefix = t("quota.in");
    if diff < 60 {
        format!("{} {}{}", prefix, diff, t("time.s"))
    } else if diff < 3600 {
        format!("{} {}{}", prefix, diff / 60, t("time.m"))
    } else if diff < 86400 {
        let h = diff / 3600;
        let m = (diff % 3600) / 60;
        format!("{} {}{} {}{}", prefix, h, t("time.h"), m, t("time.m"))
    } else {
        let d = diff / 86400;
        let h = (diff % 86400) / 3600;
        format!("{} {}{} {}{}", prefix, d, t("time.d"), h, t("time.h"))
    }
}
