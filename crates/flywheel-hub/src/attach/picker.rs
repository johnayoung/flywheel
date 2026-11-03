//! The session picker of `fw hub attach`: workspaces > sessions with live
//! agent status, on the alternate screen (ratatui). `/` filters by session,
//! kind, or workspace name; scrolled, the selected rows' workspace stays
//! pinned on top. Sessions are made, ended, and relaunched with `fw hub
//! new|kill|relaunch` or in the web UI.

use super::{Ev, HubView, status_counts};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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

/// `rows` narrowed to those matching `query` (case-insensitive): sessions
/// whose name or kind matches, and every session of a workspace whose name
/// does; each under its workspace's header.
pub fn filter_rows(rows: Vec<Row>, query: &str) -> Vec<Row> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return rows;
    }
    let hit = |s: &str| s.to_lowercase().contains(&query);
    let mut out = Vec::new();
    // The current header: the row, whether its name matches, whether it
    // is in `out` yet.
    let mut header: Option<(Row, bool, bool)> = None;
    for row in rows {
        match &row {
            Row::Workspace { name, .. } => {
                let matched = hit(name);
                header = Some((row, matched, false));
            }
            Row::Session { name, kind, .. } => {
                let ws_match = header.as_ref().is_some_and(|(_, m, _)| *m);
                if ws_match || hit(name) || hit(kind) {
                    if let Some((h, _, placed)) = &mut header
                        && !*placed
                    {
                        out.push(h.clone());
                        *placed = true;
                    }
                    out.push(row);
                }
            }
        }
    }
    out
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
#[derive(Default)]
struct Picker {
    rows: Vec<Row>,
    selected: Option<usize>,
    want: Option<String>,
    message: Option<String>,
    /// The `/` filter.
    query: String,
    /// Typing the filter.
    editing: bool,
    /// The first row on screen.
    offset: usize,
}

impl Picker {
    fn refresh(&mut self, view: &HubView) {
        let keep = self.selected_id().or_else(|| self.want.clone());
        self.rows = filter_rows(rows(view), &self.query);
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
        want: current.map(str::to_owned),
        ..Picker::default()
    };
    let lock = |v: &Arc<Mutex<HubView>>| {
        let guard = v.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    };
    let mut snapshot = lock(view);
    picker.refresh(&snapshot);
    loop {
        let drawn = terminal.draw(|frame| {
            draw(frame, &mut picker, &snapshot, base, current.is_some());
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
        let query = picker.query.clone();
        if let Some(done) = on_key(&mut picker, key, current.is_some()) {
            return done;
        }
        if picker.query != query {
            picker.refresh(&snapshot);
        }
    }
}

fn on_key(picker: &mut Picker, key: KeyEvent, can_go_back: bool) -> Option<Picked> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if picker.editing && !ctrl {
        match key.code {
            KeyCode::Char(c) => {
                picker.query.push(c);
                return None;
            }
            KeyCode::Backspace => {
                picker.query.pop();
                return None;
            }
            KeyCode::Esc => {
                picker.editing = false;
                picker.query.clear();
                return None;
            }
            KeyCode::Enter => picker.editing = false,
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char('c' | 'd') if ctrl => return Some(Picked::Quit),
        KeyCode::Char('u') if ctrl && picker.editing => picker.query.clear(),
        KeyCode::Char('/') => picker.editing = true,
        KeyCode::Char('q') => return Some(Picked::Quit),
        KeyCode::Esc if !picker.query.is_empty() => picker.query.clear(),
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

/// `s` cut to `max` columns, an ellipsis marking the cut, padded to
/// `max`.
fn fit(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let mut out = String::new();
    if s.width() <= max {
        out.push_str(s);
    } else if max > 0 {
        let mut used = 0;
        for c in s.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > max - 1 {
                break;
            }
            used += w;
            out.push(c);
        }
        out.push('\u{2026}');
    }
    let pad = max.saturating_sub(out.width());
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

/// Column widths `(name, kind, state)`: each as wide as its widest entry,
/// within a cap.
fn columns(rows: &[Row]) -> (usize, usize, usize) {
    use unicode_width::UnicodeWidthStr;
    let mut widths = (4, 4, 5);
    for row in rows {
        if let Row::Session {
            name, kind, state, ..
        } = row
        {
            widths.0 = widths.0.max(name.width());
            widths.1 = widths.1.max(kind.width());
            widths.2 = widths.2.max(state.width());
        }
    }
    (widths.0.min(32), widths.1.min(20), widths.2.min(18))
}

/// The rows on screen in `height` lines: the first row index, and the
/// header pinned above them (the selected rows' workspace, scrolled off).
fn window(picker: &mut Picker, height: usize) -> (usize, Option<usize>) {
    let len = picker.rows.len();
    let height = height.max(1);
    let selected = picker.selected.unwrap_or(0).min(len.saturating_sub(1));
    let mut offset = picker.offset.min(len.saturating_sub(1));
    // A selected first session shows its header too.
    let top =
        if selected > 0 && matches!(picker.rows.get(selected - 1), Some(Row::Workspace { .. })) {
            selected - 1
        } else {
            selected
        };
    offset = offset.min(top);
    loop {
        let pinned = header_above(&picker.rows, offset);
        let room = if pinned.is_some() {
            height.saturating_sub(1).max(1)
        } else {
            height
        };
        if selected >= offset + room {
            offset = selected + 1 - room;
            continue;
        }
        picker.offset = offset;
        return (offset, pinned);
    }
}

/// The header of the workspace row `at` belongs to, when `at` is not it.
fn header_above(rows: &[Row], at: usize) -> Option<usize> {
    if !matches!(rows.get(at), Some(Row::Session { .. })) {
        return None;
    }
    rows[..at]
        .iter()
        .rposition(|r| matches!(r, Row::Workspace { .. }))
}

fn row_line(row: &Row, widths: (usize, usize, usize), selected: bool) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut line = match row {
        Row::Workspace { name, status } => Line::from(vec![
            Span::raw("  "),
            Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(
                status.clone().map(|s| format!("[{s}]")).unwrap_or_default(),
                status_style(status.as_deref()),
            ),
        ]),
        Row::Session {
            name,
            kind,
            state,
            status,
            live,
            ..
        } => {
            let style = if *live { Style::default() } else { dim };
            Line::from(vec![
                Span::raw(if selected { "> " } else { "  " }),
                Span::raw("  "),
                Span::styled("\u{25cf} ", status_style(status.as_deref())),
                Span::styled(fit(name, widths.0), style),
                Span::raw("  "),
                Span::styled(fit(kind, widths.1), dim),
                Span::raw("  "),
                Span::styled(fit(state, widths.2), style),
                Span::raw("  "),
                Span::styled(
                    status.clone().unwrap_or_default(),
                    status_style(status.as_deref()),
                ),
            ])
        }
    };
    if selected {
        line = line.style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn draw(
    frame: &mut ratatui::Frame,
    picker: &mut Picker,
    view: &HubView,
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

    let lines: Vec<Line> = if picker.rows.is_empty() {
        let text = if picker.query.is_empty() {
            "  no sessions yet: open one in the web UI or with `fw hub new WS`".to_owned()
        } else {
            format!("  nothing matches {:?}", picker.query)
        };
        vec![Line::styled(text, dim)]
    } else {
        let height = usize::from(body.height);
        let (offset, pinned) = window(picker, height);
        let widths = columns(&picker.rows);
        let mut lines = Vec::new();
        if let Some(at) = pinned {
            lines.push(row_line(&picker.rows[at], widths, false));
        }
        for (i, row) in picker.rows.iter().enumerate().skip(offset) {
            if lines.len() >= height {
                break;
            }
            lines.push(row_line(row, widths, picker.selected == Some(i)));
        }
        lines
    };
    frame.render_widget(Paragraph::new(lines), body);

    let keys = match (picker.editing, can_go_back) {
        (true, _) => format!("/{}_   Enter attach   Esc clear", picker.query),
        (false, back) => {
            let filter = if picker.query.is_empty() {
                String::new()
            } else {
                format!("[/{}]   ", picker.query)
            };
            let esc = if !picker.query.is_empty() {
                "Esc clear   "
            } else if back {
                "Esc back   "
            } else {
                ""
            };
            format!("{filter}Enter attach   j/k move   / filter   {esc}q quit")
        }
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
            want: Some("s2".into()),
            ..Picker::default()
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

    #[test]
    fn the_filter_keeps_matches_under_their_headers() {
        let names = |rows: &[Row]| -> Vec<String> {
            rows.iter()
                .map(|r| match r {
                    Row::Workspace { name, .. } => format!("# {name}"),
                    Row::Session { name, .. } => name.clone(),
                })
                .collect()
        };
        assert_eq!(
            names(&filter_rows(rows(&view()), "AG")),
            ["# beta", "agent"]
        );
        assert_eq!(
            names(&filter_rows(rows(&view()), "beta")),
            ["# beta", "agent", "old"]
        );
        assert_eq!(
            names(&filter_rows(rows(&view()), "shell")),
            ["# alpha", "shell", "# beta", "old"]
        );
        assert!(filter_rows(rows(&view()), "zzz").is_empty());
        // Typed with `/`; Esc clears it before it means back.
        let mut picker = Picker::default();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        on_key(&mut picker, key('/'), true);
        on_key(&mut picker, key('q'), true);
        assert_eq!(picker.query, "q");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        on_key(&mut picker, enter, true);
        assert!(!picker.editing);
        assert_eq!(on_key(&mut picker, esc, true), None);
        assert!(picker.query.is_empty());
        assert_eq!(on_key(&mut picker, esc, true), Some(Picked::Back));
    }

    #[test]
    fn columns_align_and_long_names_are_cut() {
        assert_eq!(fit("abcdef", 4), "abc\u{2026}");
        assert_eq!(fit("ab", 4), "ab  ");
        let mut view = view();
        view.sessions.push(json!({ "id": "s4", "workspace_id": "w1",
            "name": "a-very-long-session-name-that-keeps-going-and-going", "kind": "shell",
            "state": "running", "live": { "pid": 3 } }));
        let rows = rows(&view);
        let widths = columns(&rows);
        assert_eq!(widths.0, 32);
        let text = |row: &Row| -> String {
            row_line(row, widths, false)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        let lines: Vec<String> = rows
            .iter()
            .filter(|r| matches!(r, Row::Session { .. }))
            .map(text)
            .collect();
        // The state column starts at the same place on every line.
        let at: Vec<usize> = lines
            .iter()
            .map(|l| {
                use unicode_width::UnicodeWidthStr;
                let i = l.find("running").or_else(|| l.find("stopped")).unwrap();
                l[..i].width()
            })
            .collect();
        assert!(at.windows(2).all(|w| w[0] == w[1]), "{lines:#?}");
    }

    #[test]
    fn scrolled_rows_keep_their_workspace_header_pinned() {
        let mut view = HubView {
            workspaces: vec![json!({ "id": "w", "name": "big" })],
            ..HubView::default()
        };
        for i in 0..30 {
            view.sessions.push(json!({ "id": format!("s{i}"), "workspace_id": "w",
                "name": format!("s{i}"), "kind": "shell", "state": "running", "live": { "pid": 1 } }));
        }
        let mut picker = Picker::default();
        picker.refresh(&view);
        assert_eq!(window(&mut picker, 10), (0, None));
        picker.step(27);
        let (offset, pinned) = window(&mut picker, 10);
        assert_eq!(pinned, Some(0));
        assert!(offset > 0 && picker.selected.unwrap() < offset + 9);
        picker.edge(false);
        assert_eq!(window(&mut picker, 10), (0, None));
    }
}
