use std::time::Duration;

use anyhow::Context;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row as TableRow, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;

use crate::model::{Row, Update, apply_update, fail_unfinished};
use crate::render::{self, COLUMN_HEADERS, Tone};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PAGE_SIZE: usize = 10;

const SUBTLE: Color = Color::Indexed(241);
const OK: Color = Color::Indexed(42);
const WARNING: Color = Color::Indexed(214);
const CRITICAL: Color = Color::Indexed(196);
const HEADER: Color = Color::Indexed(205);

/// Shows the live results table until the user quits and returns the final rows.
///
/// Blocks the calling thread, so run it via `spawn_blocking`.
pub fn run(
    rows: Vec<Row>,
    updates: UnboundedReceiver<Update>,
    notice: Option<String>,
) -> anyhow::Result<Vec<Row>> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, rows, updates, notice.as_deref());
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    rows: Vec<Row>,
    mut updates: UnboundedReceiver<Update>,
    notice: Option<&str>,
) -> anyhow::Result<Vec<Row>> {
    let mut rows = rows;
    let mut table_state = TableState::default().with_selected(0);
    let mut checks_running = true;

    loop {
        while checks_running {
            match updates.try_recv() {
                Ok(update) => rows = apply_update(rows, update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    checks_running = false;
                    rows = fail_unfinished(rows, "timed out");
                }
            }
        }

        terminal
            .draw(|frame| draw(frame, &rows, &mut table_state, notice))
            .context("failed to draw the terminal UI")?;

        if event::poll(POLL_INTERVAL).context("failed to poll terminal events")? {
            let Event::Key(key) = event::read().context("failed to read terminal event")? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if is_quit(&key) {
                return Ok(rows);
            }
            let current = table_state.selected().unwrap_or(0);
            table_state.select(Some(next_selection(current, rows.len(), key.code)));
        }
    }
}

fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// The row to select after `key`, clamped to the table.
fn next_selection(current: usize, len: usize, key: KeyCode) -> usize {
    let last = len.saturating_sub(1);
    let target = match key {
        KeyCode::Down | KeyCode::Char('j') => current.saturating_add(1),
        KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
        KeyCode::PageDown => current.saturating_add(PAGE_SIZE),
        KeyCode::PageUp => current.saturating_sub(PAGE_SIZE),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => current,
    };
    target.min(last)
}

fn draw(frame: &mut Frame, rows: &[Row], table_state: &mut TableState, notice: Option<&str>) {
    let [table_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    frame.render_stateful_widget(build_table(rows), table_area, table_state);
    frame.render_widget(footer(rows, notice), footer_area);
}

fn build_table(rows: &[Row]) -> Table<'static> {
    let name_width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .chain(std::iter::once(COLUMN_HEADERS[0].len()))
        .max()
        .unwrap_or(0);

    let header_style = Style::new()
        .fg(HEADER)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let header = TableRow::new(COLUMN_HEADERS.map(|title| Cell::from(title).style(header_style)));

    let widths = [
        Constraint::Length(u16::try_from(name_width + 2).unwrap_or(u16::MAX)),
        Constraint::Length(13),
        Constraint::Length(24),
        Constraint::Fill(1),
    ];

    Table::new(rows.iter().map(table_row), widths)
        .header(header)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
}

fn table_row(row: &Row) -> TableRow<'static> {
    let version = render::version_cell(&row.stage);
    let maintenance = render::maintenance_cell(&row.stage);

    TableRow::new([
        Cell::from(Span::styled(
            row.name.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            row.requirement.clone(),
            Style::new().fg(SUBTLE),
        )),
        Cell::from(Span::styled(version.text, tone_style(version.tone))),
        Cell::from(Span::styled(maintenance.text, tone_style(maintenance.tone))),
    ])
}

fn tone_style(tone: Tone) -> Style {
    Style::new().fg(match tone {
        Tone::Subtle => SUBTLE,
        Tone::Ok => OK,
        Tone::Warning => WARNING,
        Tone::Critical => CRITICAL,
    })
}

fn footer(rows: &[Row], notice: Option<&str>) -> Line<'static> {
    let finished = rows.iter().filter(|row| row.stage.is_finished()).count();
    let issues = rows.iter().filter(|row| row.stage.has_issue()).count();

    let mut spans = vec![Span::styled(
        format!(
            "{finished}/{} checked · {issues} with findings · q quit · ↑/↓ PgUp/PgDn scroll",
            rows.len()
        ),
        Style::new().fg(SUBTLE),
    )];
    if let Some(notice) = notice {
        spans.push(Span::styled(
            format!(" · {notice}"),
            Style::new().fg(WARNING),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_moves_and_clamps() {
        assert_eq!(next_selection(0, 5, KeyCode::Down), 1);
        assert_eq!(next_selection(4, 5, KeyCode::Down), 4);
        assert_eq!(next_selection(0, 5, KeyCode::Up), 0);
        assert_eq!(next_selection(3, 5, KeyCode::Char('k')), 2);
        assert_eq!(next_selection(1, 50, KeyCode::PageDown), 11);
        assert_eq!(next_selection(1, 5, KeyCode::PageDown), 4);
        assert_eq!(next_selection(3, 50, KeyCode::PageUp), 0);
        assert_eq!(next_selection(3, 50, KeyCode::End), 49);
        assert_eq!(next_selection(3, 50, KeyCode::Home), 0);
        assert_eq!(next_selection(3, 50, KeyCode::Char('x')), 3);
    }

    #[test]
    fn selection_handles_an_empty_table() {
        assert_eq!(next_selection(0, 0, KeyCode::Down), 0);
        assert_eq!(next_selection(0, 0, KeyCode::End), 0);
    }

    #[test]
    fn quit_keys_are_recognised() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        assert!(is_quit(&plain(KeyCode::Char('q'))));
        assert!(is_quit(&plain(KeyCode::Esc)));
        assert!(is_quit(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_quit(&plain(KeyCode::Char('c'))));
    }

    #[test]
    fn footer_counts_progress_and_findings() {
        let mut finished = Row::pending("a", "1");
        finished.stage = crate::model::Stage::Failed("x".into());
        let rows = [finished, Row::pending("b", "1")];

        let text: String = footer(&rows, Some("note"))
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(text.starts_with("1/2 checked · 1 with findings"));
        assert!(text.ends_with("note"));
    }
}
