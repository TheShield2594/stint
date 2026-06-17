//! TUI rendering using ratatui.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;
use stint_core::duration::format_duration_human;
use time::OffsetDateTime;

use super::app::{App, Panel};

/// Renders the entire dashboard.
pub fn render(frame: &mut Frame, app: &App) {
    let extra_rows = if app.error_message.is_some() { 1 } else { 0 };

    // 4-panel layout: Header, (optional error bar), Main, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),          // Header
            Constraint::Length(extra_rows), // Error bar (0 or 1)
            Constraint::Min(5),             // Main area
            Constraint::Length(3),          // Footer
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    if extra_rows > 0 {
        render_error_bar(frame, app, chunks[1]);
    }
    render_main(frame, app, chunks[2]);
    render_footer(frame, chunks[3]);
}

/// Renders the header bar with current timer status.
fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let (status_text, style) = match &app.running_timer {
        Some((entry, project)) => {
            let elapsed = (OffsetDateTime::now_utc() - entry.start).whole_seconds();
            let text = format!(
                "  Tracking: {}  [{}]",
                project.name,
                format_duration_human(elapsed)
            );
            (
                text,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        }
        None => {
            let text = "  Idle — no timer running".to_string();
            (text, Style::default().fg(Color::DarkGray))
        }
    };

    let header = Paragraph::new(Line::from(vec![Span::styled(status_text, style)])).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" stint ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );
    frame.render_widget(header, area);
}

/// Renders a red error bar below the header when a database error occurs.
fn render_error_bar(frame: &mut Frame, app: &App, area: Rect) {
    let text = match &app.error_message {
        Some(msg) => format!("  ⚠ DB error: {msg}"),
        None => return,
    };

    let bar = Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
    )]));
    frame.render_widget(bar, area);
}

/// Renders the main content area with today's entries, timeline, and week totals.
fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(area);

    render_today(frame, app, chunks[0]);
    render_timeline(frame, app, chunks[1]);
    render_week(frame, app, chunks[2]);
}

/// Renders today's entries panel.
fn render_today(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.selected_panel == Panel::Today {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let items: Vec<ListItem> = app
        .today_entries
        .iter()
        .skip(app.today_scroll)
        .enumerate()
        .map(|(i, (entry, project))| {
            let duration = if entry.is_running() {
                let elapsed = (OffsetDateTime::now_utc() - entry.start).whole_seconds();
                format!("{} *", format_duration_human(elapsed))
            } else {
                format_duration_human(entry.computed_duration_secs().unwrap_or(0))
            };

            let time_str = entry
                .start
                .format(&time::format_description::well_known::Rfc3339)
                .map(|s| s[11..16].to_string()) // HH:MM
                .unwrap_or_else(|_| "??:??".to_string());

            let source = match entry.source.as_str() {
                "hook" => "auto",
                other => other,
            };

            let notes = entry.notes.as_deref().unwrap_or("");
            let line = format!(
                " {time_str}  {:<14} {:>8}  {:<6} {notes}",
                project.name, duration, source
            );

            let style = if entry.is_running() {
                Style::default().fg(Color::Green)
            } else if i % 2 == 0 {
                Style::default()
            } else {
                Style::default().fg(Color::Gray)
            };

            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();

    let title = format!(" Today ({}) ", app.today_entries.len());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style),
    );
    frame.render_widget(list, area);
}

/// Renders the timeline panel.
fn render_timeline(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.selected_panel == Panel::Timeline;
    super::timeline::render_timeline(
        frame,
        area,
        app.timeline_entries(),
        app.timeline_scroll,
        app.timeline_view,
        is_focused,
    );
}

/// Renders the weekly project totals panel.
fn render_week(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.selected_panel == Panel::Week {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let max_secs = app.week_totals.iter().map(|(_, s)| *s).max().unwrap_or(1);
    // Reserve space for the bar: area width minus borders, name column, time column, padding
    let bar_width = area.width.saturating_sub(30) as usize;

    let rows: Vec<Row> = app
        .week_totals
        .iter()
        .skip(app.week_scroll)
        .map(|(name, secs)| {
            let bar_len = if max_secs > 0 {
                (*secs as usize * bar_width) / max_secs as usize
            } else {
                0
            }
            .max(1);
            let bar = "\u{2588}".repeat(bar_len); // Full block character

            Row::new(vec![format!(" {name}"), bar, format_duration_human(*secs)])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Min(4),
            Constraint::Length(10),
        ],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" This Week ")
            .border_style(border_style),
    )
    .column_spacing(1);

    frame.render_widget(table, area);
}

/// Renders the footer with key binding hints.
fn render_footer(frame: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(":quit  "),
        Span::styled("tab", Style::default().fg(Color::Yellow)),
        Span::raw(":switch  "),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Yellow)),
        Span::raw(":scroll  "),
        Span::styled("y", Style::default().fg(Color::Yellow)),
        Span::raw(":yesterday  "),
        Span::styled("t", Style::default().fg(Color::Yellow)),
        Span::raw(":today"),
    ]));
    frame.render_widget(footer, area);
}