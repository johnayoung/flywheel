//! The session picker of `fw hub attach`: workspaces > sessions with live
//! agent status, on the alternate screen (ratatui).

use super::{Ev, HubView, status_counts};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use serde_json::Value;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// What the picker ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Attach(String),
    /// Back to the session it was opened from.
    Back,
    Quit,
    /// The program must end (a signal, the hub went away): the reason.
    Abort(String),
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Workspace {
        name: String,
        status: Option<String>,
    },
    Session {
        id: String,
        name: String,
        kind: String,
        state: String,
        status: Option<String>,
        /// The server holds its terminal (attachable).
        live: bool,
    },
}

fn str_of<'a>(v: &'a Value, field: &str) -> &'a str {
    v.get(field).and_then(Value::as_str).unwrap_or("")
}

/// The list: each workspace (in hub order) with its sessions, then any
/// session whose workspace is not listed.
pub fn rows(view: &HubView) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut placed = std::collections::HashSet::new();
    let session_row = |s: &Value| {
        let id = str_of(s, "id").to_owned();
        Row::Session {
            name: str_of(s, "name").to_owned(),
            kind: crate::script::kind_label(s),
            state: process_state(s),
            status: view.status.get(&id).cloned(),
            live: s.get("live").is_some_and(|l| !l.is_null()),
            id,
        }
    };
    for workspace in &view.workspaces {
        let ws_id = str_of(workspace, "id");
        let members: Vec<&Value> = view
            .sessions
            .iter()
            .filter(|s| str_of(s, "workspace_id") == ws_id)
            .collect();
        rows.push(Row::Workspace {
            name: str_of(workspace, "name").to_owned(),
            status: view.workspace_status.get(ws_id).cloned(),
        });
        for s in members {
            placed.insert(str_of(s, "id").to_owned());
            rows.push(session_row(s));
        }
    }
    let orphans: Vec<&Value> = view
        .sessions
        .iter()
        .filter(|s| !placed.contains(str_of(s, "id")))
        .collect();
    if !orphans.is_empty() {
        rows.push(Row::Workspace {
            name: "(other)".into(),
            status: None,
        });
        rows.extend(orphans.into_iter().map(session_row));
    }
    rows
}

/// `running`, `exited 0`, `killed (Hangup)`, `stopped`, ...
fn process_state(session: &Value) -> String {
    let label = crate::script::state_label(session);
    // The agent status has its own column.
    match label.split_once(" (") {
        Some(("running", _)) => "running".into(),
        _ => label,
    }
}

fn status_style(status: Option<&str>) -> Style {
    match status {
        Some("blocked") => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Some("working") => Style::default().fg(Color::Cyan),
        Some("done") => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        Some("idle") => Style::default().fg(Color::Gray),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn selectable(row: &Row) -> bool {
    matches!(row, Row::Session { .. })
}

/// The picker's state between draws.
struct Picker {
    rows: Vec<Row>,
    selected: Option<usize>,
    want: Option<String>,
    message: Option<String>,
}

impl Picker {
    fn refresh(&mut self, view: &HubView) {
        let keep = self.selected_id().or_else(|| self.want.clone());
        self.rows = rows(view);
        self.selected = keep
            .and_then(|id| {
                self.rows
                    .iter()
                    .position(|r| matches!(r, Row::Session { id: i, .. } if *i == id))
            })
            .or_else(|| {
                // The first attachable session, else the first at all.
                self.rows
                    .iter()
                    .position(|r| matches!(r, Row::Session { live: true, .. }))
                    .or_else(|| self.rows.iter().position(selectable))
            });
        if self.selected.is_some() {
            self.want = None;
        }
    }

    fn selected_id(&self) -> Option<String> {
        match self.rows.get(self.selected?) {
            Some(Row::Session { id, .. }) => Some(id.clone()),
            _ => None,
        }
    }

    fn step(&mut self, by: isize) {
        let Some(mut at) = self.selected else {
            self.selected = self.rows.iter().position(selectable);
            return;
        };
        let len = self.rows.len() as isize;
        let mut moved = 0;
        let mut i = at as isize;
        while moved < by.abs() {
            let next = i + by.signum();
            if next < 0 || next >= len {
                break;
            }
            i = next;
            if selectable(&self.rows[i as usize]) {
                at = i as usize;
                moved += 1;
            }
        }
        self.selected = Some(at);
    }

    fn edge(&mut self, last: bool) {
        self.selected = if last {
            self.rows.iter().rposition(selectable)
        } else {
            self.rows.iter().position(selectable)
        };
    }
}

/// Run the picker until a choice. `current`: the session it was opened
/// from (preselected; Esc returns to it).
pub fn pick(
    view: &Arc<Mutex<HubView>>,
    events: &Receiver<Ev>,
    current: Option<&str>,
    base: &str,
) -> Picked {
    super::term::alternate_screen(true);
    let result = run(view, events, current, base);
    super::term::alternate_screen(false);
    result
}

fn run(
    view: &Arc<Mutex<HubView>>,
    events: &Receiver<Ev>,
    current: Option<&str>,
    base: &str,
) -> Picked {
    let mut terminal = match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
        Ok(t) => t,
        Err(e) => return Picked::Abort(format!("cannot draw: {e}")),
    };
    let _ = terminal.hide_cursor();
    let mut picker = Picker {
        rows: Vec::new(),
        selected: None,
        want: current.map(str::to_owned),
        message: None,
    };
    let lock = |v: &Arc<Mutex<HubView>>| {
        let guard = v.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    };
    let mut snapshot = lock(view);
    picker.refresh(&snapshot);
    let mut list_state = ListState::default();
    loop {
        list_state.select(picker.selected);
        let drawn = terminal.draw(|frame| {
            draw(
                frame,
                &picker,
                &snapshot,
                &mut list_state,
                base,
                current.is_some(),
            );
        });
        if let Err(e) = drawn {
            return Picked::Abort(format!("cannot draw: {e}"));
        }
        // Hub changes and signals.
        let mut changed = false;
        while let Ok(ev) = events.try_recv() {
            match ev {
                Ev::Hub => changed = true,
                Ev::Terminate(why) => return Picked::Abort(why),
                // A stale session's traffic, or a resize (redrawn anyway).
                _ => {}
            }
        }
        if changed {
            snapshot = lock(view);
            picker.refresh(&snapshot);
            continue;
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => return Picked::Abort(format!("reading keys: {e}")),
        }
        let key = match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => key,
            Ok(_) => continue,
            Err(e) => return Picked::Abort(format!("reading keys: {e}")),
        };
        picker.message = None;
        if let Some(done) = on_key(&mut picker, key, current.is_some()) {
            return done;
        }
    }
}

fn on_key(picker: &mut Picker, key: KeyEvent, can_go_back: bool) -> Option<Picked> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c' | 'd') if ctrl => return Some(Picked::Quit),
        KeyCode::Char('q') => return Some(Picked::Quit),
        KeyCode::Esc if can_go_back => return Some(Picked::Back),
        KeyCode::Esc => return Some(Picked::Quit),
        KeyCode::Up | KeyCode::Char('k') => picker.step(-1),
        KeyCode::Char('p') if ctrl => picker.step(-1),
        KeyCode::Down | KeyCode::Char('j') => picker.step(1),
        KeyCode::Char('n') if ctrl => picker.step(1),
        KeyCode::PageUp => picker.step(-10),
        KeyCode::PageDown => picker.step(10),
        KeyCode::Home | KeyCode::Char('g') => picker.edge(false),
        KeyCode::End | KeyCode::Char('G') => picker.edge(true),
        KeyCode::Enter => {
            let row = picker.selected.and_then(|i| picker.rows.get(i));
            match row {
                Some(Row::Session { id, live: true, .. }) => {
                    return Some(Picked::Attach(id.clone()));
                }
                Some(Row::Session { name, state, .. }) => {
                    picker.message = Some(format!(
                        "{name} has no terminal on the hub ({state}): `fw hub relaunch {name}` starts it again"
                    ));
                }
                _ => picker.message = Some("no session selected".into()),
            }
        }
        _ => {}
    }
    None
}

fn draw(
    frame: &mut ratatui::Frame,
    picker: &Picker,
    view: &HubView,
    list_state: &mut ListState,
    base: &str,
    can_go_back: bool,
) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let (blocked, done) = status_counts(view);
    let dim = Style::default().fg(Color::DarkGray);
    let mut title = vec![
        Span::styled(
            " fw hub ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(base.to_owned(), dim),
        Span::raw("   "),
        Span::styled(
            format!("{blocked} blocked"),
            status_style((blocked > 0).then_some("blocked")),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{done} done"),
            status_style((done > 0).then_some("done")),
        ),
    ];
    if !view.online {
        title.push(Span::raw("   "));
        title.push(Span::styled(
            view.error
                .clone()
                .unwrap_or_else(|| "events offline".into()),
            Style::default().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(title)), head);

    let items: Vec<ListItem> = if picker.rows.is_empty() {
        vec![ListItem::new(Line::styled(
            "  no sessions yet: open one in the web UI or with `fw hub new WS`",
            dim,
        ))]
    } else {
        picker
            .rows
            .iter()
            .map(|row| match row {
                Row::Workspace { name, status } => ListItem::new(Line::from(vec![
                    Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(
                        status.clone().map(|s| format!("[{s}]")).unwrap_or_default(),
                        status_style(status.as_deref()),
                    ),
                ])),
                Row::Session {
                    name,
                    kind,
                    state,
                    status,
                    live,
                    ..
                } => {
                    let style = if *live { Style::default() } else { dim };
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{25cf} ", status_style(status.as_deref())),
                        Span::styled(format!("{name:<24}"), style),
                        Span::styled(format!(" {kind:<16}"), dim),
                        Span::styled(format!(" {state:<16}"), style),
                        Span::styled(
                            status.clone().unwrap_or_default(),
                            status_style(status.as_deref()),
                        ),
                    ]))
                }
            })
            .collect()
    };
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, body, list_state);

    let keys = if can_go_back {
        "Enter attach   j/k move   Esc back   q quit"
    } else {
        "Enter attach   j/k move   q quit"
    };
    let mut lines = vec![Line::styled(keys, dim)];
    if let Some(message) = &picker.message {
        lines.insert(
            0,
            Line::styled(message.clone(), Style::default().fg(Color::Yellow)),
        );
    }
    frame.render_widget(Paragraph::new(lines), foot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view() -> HubView {
        let mut view = HubView {
            workspaces: vec![
                json!({ "id": "w1", "name": "alpha" }),
                json!({ "id": "w2", "name": "beta" }),
            ],
            sessions: vec![
                json!({ "id": "s1", "workspace_id": "w1", "name": "shell", "kind": "shell", "state": "running", "live": { "pid": 1 } }),
                json!({ "id": "s2", "workspace_id": "w2", "name": "agent", "kind": "claude", "state": "running", "live": { "pid": 2 }, "agent": { "status": "done" } }),
                json!({ "id": "s3", "workspace_id": "w2", "name": "old", "kind": "shell", "state": "stopped", "live": null }),
            ],
            ..HubView::default()
        };
        view.status.insert("s2".into(), "done".into());
        view.workspace_status.insert("w2".into(), "done".into());
        view
    }

    #[test]
    fn rows_group_sessions_under_their_workspaces() {
        let rows = rows(&view());
        let names: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Workspace { name, .. } => format!("# {name}"),
                Row::Session {
                    name,
                    state,
                    status,
                    live,
                    ..
                } => format!("{name} {state} {status:?} {live}"),
            })
            .collect();
        assert_eq!(
            names,
            [
                "# alpha",
                "shell running None true",
                "# beta",
                "agent running Some(\"done\") true",
                "old stopped None false",
            ]
        );
    }

    #[test]
    fn selection_skips_headers_and_keeps_its_session() {
        let mut picker = Picker {
            rows: Vec::new(),
            selected: None,
            want: Some("s2".into()),
            message: None,
        };
        picker.refresh(&view());
        assert_eq!(picker.selected_id().as_deref(), Some("s2"));
        picker.step(-1);
        assert_eq!(picker.selected_id().as_deref(), Some("s1"));
        picker.step(-1);
        assert_eq!(picker.selected_id().as_deref(), Some("s1"));
        picker.edge(true);
        assert_eq!(picker.selected_id().as_deref(), Some("s3"));
        // A stopped session is listed but not attachable.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(on_key(&mut picker, key, false), None);
        assert!(picker.message.as_deref().unwrap().contains("relaunch"));
        picker.step(-1);
        assert_eq!(
            on_key(&mut picker, key, false),
            Some(Picked::Attach("s2".into()))
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(on_key(&mut picker, esc, true), Some(Picked::Back));
        assert_eq!(on_key(&mut picker, esc, false), Some(Picked::Quit));
    }
}
