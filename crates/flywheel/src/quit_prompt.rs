//! Modal quit prompt for the supervised-daemon handoff.
//!
//! When the operator quits a console that owns a supervised worker and/or
//! autopilot daemon, the spec requires a single one-shot prompt: `Enter`
//! detaches (the daemons keep running, the console exits), `s` stops every
//! supervised child gracefully (SIGTERM; the in-flight finalization lands
//! the run at `interrupted`), `Escape` cancels the quit so the operator can
//! keep working without disturbing anything.
//!
//! The prompt is intentionally minimal -- four lines of instructions, three
//! key bindings -- and is *not* shown when the console spawned nothing
//! (detached / external / already stopped); the dashboard's quit path
//! short-circuits in that case.
//!
//! Structured as pure state plus `render` so it is testable headless: the
//! dashboard pushes it as a modal overlay and feeds it key events through
//! [`QuitPrompt::handle_key`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// The three operator decisions a prompt can resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitChoice {
    /// Detach every supervised daemon (they keep running), exit.
    Detach,
    /// Stop every supervised daemon gracefully (SIGTERM + wait), exit.
    Stop,
    /// Stay on the console; no daemon is touched.
    Cancel,
}

/// The default running-daemons label (a worker-only console).
pub const DEFAULT_RUNNING_LABEL: &str = "supervised worker is running";

/// The prompt body: the running-daemons label plus the three key lines.
pub fn prompt_lines(running_label: &str) -> [String; 4] {
    [
        running_label.to_owned(),
        "  enter  detach (daemons keep running, console exits)".to_owned(),
        "  s      stop all gracefully (SIGTERM; in-flight run finalizes to interrupted)".to_owned(),
        "  esc    cancel quit".to_owned(),
    ]
}

/// The quit prompt's first line, naming the supervised daemons at risk.
pub fn running_daemons_label(worker_live: bool, autopilot_live: bool) -> &'static str {
    if worker_live && autopilot_live {
        "supervised worker and autopilot are running"
    } else if autopilot_live {
        "supervised autopilot is running"
    } else {
        DEFAULT_RUNNING_LABEL
    }
}

/// Single-purpose modal asking detach / stop / cancel on quit.
///
/// `running_label` names which daemons are live (worker, autopilot, or
/// both) so the operator sees exactly what `stop` will terminate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitPrompt {
    running_label: String,
}

impl Default for QuitPrompt {
    fn default() -> Self {
        Self::new(DEFAULT_RUNNING_LABEL)
    }
}

impl QuitPrompt {
    pub fn new(running_label: impl Into<String>) -> Self {
        Self {
            running_label: running_label.into(),
        }
    }

    pub fn running_label(&self) -> &str {
        &self.running_label
    }

    /// Map one key press to a decision; `None` for keys the prompt ignores.
    ///
    /// The modal owns every key while it is on top of the stack, so an
    /// unrelated key is swallowed (returns `None`) rather than reaching the
    /// screen underneath. `ctrl+c` is treated as cancel so the terminal's
    /// interrupt reflex never silently kills a child.
    pub fn handle_key(&self, key: KeyEvent) -> Option<QuitChoice> {
        match key.code {
            KeyCode::Enter => Some(QuitChoice::Detach),
            KeyCode::Char('s') | KeyCode::Char('S') if key.modifiers.is_empty() => {
                Some(QuitChoice::Stop)
            }
            KeyCode::Esc => Some(QuitChoice::Cancel),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(QuitChoice::Cancel)
            }
            _ => None,
        }
    }

    /// The text block the prompt shows.
    pub fn text(&self) -> Text<'static> {
        Text::from(
            prompt_lines(&self.running_label)
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
    }

    /// Draw the prompt as a centered bordered box over `area`, clearing
    /// whatever was underneath so the modal reads as a modal.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let box_area = centered_box(area, 64, 6);
        frame.render_widget(Clear, box_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" quit ");
        frame.render_widget(Paragraph::new(self.text()).block(block), box_area);
    }
}

/// A `width` x `height` rectangle centered inside `area`, clamped to it.
fn centered_box(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn enter_detaches_s_stops_escape_cancels() {
        let prompt = QuitPrompt::default();
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(QuitChoice::Detach)
        );
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('s'))),
            Some(QuitChoice::Stop)
        );
        assert_eq!(
            prompt.handle_key(key(KeyCode::Esc)),
            Some(QuitChoice::Cancel)
        );
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(QuitChoice::Cancel)
        );
        assert_eq!(prompt.handle_key(key(KeyCode::Char('q'))), None);
        assert_eq!(prompt.handle_key(key(KeyCode::Down)), None);
    }

    #[test]
    fn prompt_lines_carry_label_and_three_keys() {
        let lines = prompt_lines("supervised autopilot is running");
        assert_eq!(lines[0], "supervised autopilot is running");
        assert!(lines[1].contains("enter"));
        assert!(lines[1].contains("detach"));
        assert!(lines[2].contains("stop all gracefully"));
        assert!(lines[2].contains("SIGTERM"));
        assert!(lines[3].contains("esc"));
    }

    #[test]
    fn running_daemons_label_names_the_live_set() {
        assert_eq!(
            running_daemons_label(true, true),
            "supervised worker and autopilot are running"
        );
        assert_eq!(
            running_daemons_label(false, true),
            "supervised autopilot is running"
        );
        assert_eq!(running_daemons_label(true, false), DEFAULT_RUNNING_LABEL);
        assert_eq!(running_daemons_label(false, false), DEFAULT_RUNNING_LABEL);
    }

    #[test]
    fn render_draws_a_centered_box_with_every_line() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let prompt = QuitPrompt::new("supervised worker and autopilot are running");
        terminal
            .draw(|frame| prompt.render(frame, frame.area()))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("supervised worker and autopilot are running"));
        assert!(text.contains("enter  detach"));
        assert!(text.contains("s      stop all gracefully"));
        assert!(text.contains("esc    cancel quit"));
        assert!(text.contains(" quit "));
    }

    #[test]
    fn centered_box_clamps_to_small_areas() {
        let area = Rect::new(0, 0, 20, 4);
        let boxed = centered_box(area, 64, 6);
        assert_eq!(boxed, Rect::new(0, 0, 20, 4));
        let wide = centered_box(Rect::new(0, 0, 100, 20), 64, 6);
        assert_eq!(wide, Rect::new(18, 7, 64, 6));
    }
}
