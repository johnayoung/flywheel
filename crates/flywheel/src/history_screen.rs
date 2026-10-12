//! The finished-run listing (the console's history view).
//!
//! Opened from the dashboard via the `h` key or the `/history` slash
//! command. One row per task, most recently finished first -- the same
//! grouping `flywheel history` prints (a retried task folds into one row
//! with `runs=N`). `Enter` drills into the selected run through the same
//! session-screen factory the dashboard uses; `Escape` returns to the
//! dashboard; `r` reloads. History is fetched on open and on demand --
//! finished runs do not change underneath the operator the way live rows
//! do, so there is no polling timer.
//!
//! Pure state plus [`HistoryScreen::render`]: the dashboard owns the
//! screen stack and interprets the [`HistoryAction`] each key returns.

use crate::dashboard::{format_tokens, table_widths};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flywheel_core::lifecycle::Status;
use flywheel_orchestrator::history::HistoryRow;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

/// `(key, label)` pairs: keys are the stable cell identifiers, labels the
/// rendered header.
pub const HISTORY_COLUMNS: [(&str, &str); 7] = [
    ("phase", "phase"),
    ("task_id", "task"),
    ("status", "status"),
    ("finished", "finished"),
    ("runs", "runs"),
    ("tokens", "tokens"),
    ("cost", "cost"),
];

/// The header line above the table.
pub const HISTORY_HEADER: &str = "history - finished runs (enter=open, r=reload, esc=back)";

/// Returns the history rows; an `Err` surfaces inline and keeps the
/// previous rows on screen.
pub type FetchHistory = Box<dyn FnMut() -> Result<Vec<HistoryRow>, String>>;

/// What a key press on the history screen asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryAction {
    None,
    /// `escape`: pop back to the dashboard.
    Back,
    /// `enter` on a row: open that run's session screen.
    OpenSession {
        run_id: String,
        task_id: String,
    },
    /// `ctrl+c`: route through the app's quit path.
    RequestQuit,
}

fn status_style(status: &str) -> Option<Style> {
    if status == Status::Done.as_str() {
        Some(Style::default().fg(Color::Green))
    } else if status == Status::Failed.as_str() {
        Some(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else if status == Status::FailedValidation.as_str() {
        Some(Style::default().fg(Color::Red))
    } else {
        None
    }
}

fn format_finished(ts: Option<DateTime<Utc>>) -> String {
    ts.map_or_else(
        || "-".to_owned(),
        |t| t.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// The seven plain cell strings for one history row.
pub fn row_to_cells(row: &HistoryRow) -> [String; 7] {
    let run = &row.latest;
    [
        row.phase.clone().unwrap_or_else(|| "-".to_owned()),
        row.task_id.clone(),
        run.status.as_str().to_owned(),
        format_finished(run.finished_at),
        (1 + row.prior_runs.len()).to_string(),
        format_tokens(run.tokens_total),
        format!("${:.4}", run.cost_usd_total),
    ]
}

/// Finished-run listing with drill-in to the per-run session view.
///
/// Exposed for tests: `rows` (the most recent fetch) and
/// `visible_run_order` (run ids in table order).
pub struct HistoryScreen {
    fetch: FetchHistory,
    pub rows: Vec<HistoryRow>,
    pub visible_run_order: Vec<String>,
    cells: Vec<[String; 7]>,
    cursor: usize,
    last_error: Option<String>,
}

impl HistoryScreen {
    /// Construct and perform the initial fetch.
    pub fn new(fetch: impl FnMut() -> Result<Vec<HistoryRow>, String> + 'static) -> Self {
        let mut screen = Self {
            fetch: Box::new(fetch),
            rows: Vec::new(),
            visible_run_order: Vec::new(),
            cells: Vec::new(),
            cursor: 0,
            last_error: None,
        };
        screen.reload_now();
        screen
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Fetch one history snapshot and rebuild the table. A fetch failure
    /// keeps the previous rows on screen and surfaces the error inline.
    pub fn reload_now(&mut self) {
        match (self.fetch)() {
            Ok(rows) => {
                self.rows = rows;
                self.last_error = None;
            }
            Err(err) => self.last_error = Some(format!("history read failed: {err}")),
        }
        self.visible_run_order = self.rows.iter().map(|r| r.latest.run_id.clone()).collect();
        self.cells = self.rows.iter().map(row_to_cells).collect();
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// The run id under the cursor, or `None` on an empty table.
    pub fn selected_run_id(&self) -> Option<&str> {
        self.visible_run_order.get(self.cursor).map(String::as_str)
    }

    pub fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn cursor_down(&mut self) {
        if self.cursor + 1 < self.visible_run_order.len() {
            self.cursor += 1;
        }
    }

    /// Route one key press.
    pub fn handle_key(&mut self, key: KeyEvent) -> HistoryAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => HistoryAction::RequestQuit,
                _ => HistoryAction::None,
            };
        }
        match key.code {
            KeyCode::Esc => HistoryAction::Back,
            KeyCode::Up => {
                self.cursor_up();
                HistoryAction::None
            }
            KeyCode::Down => {
                self.cursor_down();
                HistoryAction::None
            }
            KeyCode::Char('r') => {
                self.reload_now();
                HistoryAction::None
            }
            KeyCode::Enter => self.open_selected(),
            _ => HistoryAction::None,
        }
    }

    /// The open action for the selected finished run, or `None` when the
    /// table is empty. A completed run renders fine in the session screen:
    /// its transcript file persists under `logs/runs/` and the status
    /// banner pins the terminal state.
    pub fn open_selected(&self) -> HistoryAction {
        let Some(run_id) = self.selected_run_id() else {
            return HistoryAction::None;
        };
        match self.rows.iter().find(|r| r.latest.run_id == run_id) {
            Some(row) => HistoryAction::OpenSession {
                run_id: run_id.to_owned(),
                task_id: row.task_id.clone(),
            },
            None => HistoryAction::None,
        }
    }

    /// Draw the header, the table, and the empty-state / error lines.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let empty_rows = u16::from(self.rows.is_empty());
        let error_rows = u16::from(self.last_error.is_some());
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(empty_rows),
            Constraint::Length(error_rows),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(HISTORY_HEADER))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            chunks[0],
        );
        let header = Row::new(
            HISTORY_COLUMNS
                .iter()
                .map(|(_, label)| Cell::from(*label))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
        let rows: Vec<Row> = self
            .cells
            .iter()
            .map(|cells| {
                let mut row_cells: Vec<Cell> =
                    cells.iter().map(|c| Cell::from(c.as_str())).collect();
                if let Some(style) = status_style(&cells[2]) {
                    row_cells[2] = Cell::from(cells[2].as_str()).style(style);
                }
                Row::new(row_cells)
            })
            .collect();
        let labels: Vec<&str> = HISTORY_COLUMNS.iter().map(|(_, label)| *label).collect();
        let widths = table_widths(&labels, &self.cells);
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol("> ")
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default();
        if !self.cells.is_empty() {
            state.select(Some(self.cursor));
        }
        frame.render_stateful_widget(table, chunks[1], &mut state);
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new("(no finished runs)")
                    .style(Style::default().add_modifier(Modifier::DIM)),
                chunks[2],
            );
        }
        if let Some(error) = &self.last_error {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(Style::default().fg(Color::Yellow)),
                chunks[3],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{buffer_text, ctrl, key, terminal};
    use chrono::TimeZone;
    use flywheel_orchestrator::history::HistoryRun;
    use std::cell::Cell as StdCell;
    use std::rc::Rc;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn hrun(task_id: &str, status: Status) -> HistoryRun {
        HistoryRun {
            run_id: format!("run-{task_id}"),
            task_id: task_id.to_owned(),
            status,
            source: format!(".flywheel/tasks/active/30-a/{task_id}.json"),
            started_at: Some(t0()),
            finished_at: Some(t0()),
            retries: 0,
            error: String::new(),
            attempts: 1,
            tokens_total: 1234,
            cost_usd_total: 0.5,
            turns_total: 9,
        }
    }

    fn hrow(task_id: &str, status: Status) -> HistoryRow {
        HistoryRow {
            task_id: task_id.to_owned(),
            phase: Some("30-a".to_owned()),
            latest: hrun(task_id, status),
            prior_runs: Vec::new(),
        }
    }

    #[test]
    fn renders_rows_and_hides_empty_marker() {
        let rows = vec![hrow("alpha", Status::Done), hrow("beta", Status::Failed)];
        let mut screen = HistoryScreen::new(move || Ok(rows.clone()));
        assert_eq!(screen.visible_run_order, vec!["run-alpha", "run-beta"]);
        let mut term = terminal(80, 10);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains(HISTORY_HEADER));
        assert!(text.contains("phase  task"));
        assert!(text.contains("30-a"));
        assert!(text.contains("alpha"));
        assert!(text.contains("failed"));
        assert!(text.contains("2026-06-01 12:00"));
        assert!(text.contains("$0.5000"));
        assert!(!text.contains("(no finished runs)"));
    }

    #[test]
    fn empty_fetch_shows_marker() {
        let mut screen = HistoryScreen::new(|| Ok(Vec::new()));
        let mut term = terminal(80, 6);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("(no finished runs)"));
        assert_eq!(screen.open_selected(), HistoryAction::None);
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), HistoryAction::None);
    }

    #[test]
    fn fetch_error_is_contained_and_rows_survive() {
        let calls = Rc::new(StdCell::new(0));
        let probe = Rc::clone(&calls);
        let mut screen = HistoryScreen::new(move || {
            probe.set(probe.get() + 1);
            if probe.get() > 1 {
                Err("store gone".to_owned())
            } else {
                Ok(vec![hrow("alpha", Status::Done)])
            }
        });
        assert_eq!(screen.visible_run_order, vec!["run-alpha"]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('r'))),
            HistoryAction::None
        );
        assert_eq!(screen.visible_run_order, vec!["run-alpha"]);
        assert_eq!(screen.last_error(), Some("history read failed: store gone"));
        let mut term = terminal(80, 6);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("history read failed"));
    }

    #[test]
    fn reload_picks_up_new_rows() {
        let rows = Rc::new(std::cell::RefCell::new(vec![hrow("alpha", Status::Done)]));
        let probe = Rc::clone(&rows);
        let mut screen = HistoryScreen::new(move || Ok(probe.borrow().clone()));
        assert_eq!(screen.visible_run_order, vec!["run-alpha"]);
        rows.borrow_mut().push(hrow("beta", Status::Done));
        screen.handle_key(key(KeyCode::Char('r')));
        assert_eq!(screen.visible_run_order, vec!["run-alpha", "run-beta"]);
    }

    #[test]
    fn navigation_enter_and_escape_actions() {
        let mut screen = HistoryScreen::new(|| {
            Ok(vec![
                hrow("alpha", Status::Done),
                hrow("beta", Status::Done),
            ])
        });
        assert_eq!(screen.cursor(), 0);
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.cursor(), 1);
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.cursor(), 1);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            HistoryAction::OpenSession {
                run_id: "run-beta".to_owned(),
                task_id: "beta".to_owned()
            }
        );
        screen.handle_key(key(KeyCode::Up));
        assert_eq!(screen.cursor(), 0);
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), HistoryAction::Back);
        assert_eq!(screen.handle_key(ctrl('c')), HistoryAction::RequestQuit);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('z'))),
            HistoryAction::None
        );
    }

    #[test]
    fn cells_render_status_runs_tokens_and_cost() {
        let mut row = hrow("alpha", Status::Failed);
        row.prior_runs.push(hrun("alpha", Status::FailedValidation));
        let cells = row_to_cells(&row);
        assert_eq!(cells[0], "30-a");
        assert_eq!(cells[1], "alpha");
        assert_eq!(cells[2], "failed");
        assert_eq!(cells[3], "2026-06-01 12:00");
        assert_eq!(cells[4], "2");
        assert_eq!(cells[5], "1234");
        assert_eq!(cells[6], "$0.5000");
        let mut ungrouped = hrow("x", Status::Done);
        ungrouped.phase = None;
        ungrouped.latest.finished_at = None;
        let cells = row_to_cells(&ungrouped);
        assert_eq!(cells[0], "-");
        assert_eq!(cells[3], "-");
    }
}
