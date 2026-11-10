//! Per-workspace pane layouts: named tabs, each a binary tree of splits
//! whose leaves are panes showing one session (or none yet).
//!
//! The server only stores and validates layouts so every browser sees the
//! same tabs and splits; which tab is active, which pane has focus, and
//! zoom are each client's own. Writes carry the revision they were made
//! against (`rev`): a stale one is refused, so two browsers editing at once
//! never silently undo each other.
//!
//! JSON shape (`GET/PUT /api/workspaces/{id}/layout`):
//!
//! ```json
//! {"rev": 3, "tabs": [{"id": "t1", "name": "agents", "root":
//!   {"type": "split", "dir": "right", "ratio": 0.5,
//!    "a": {"type": "pane", "id": "p1", "session": "<session id>"},
//!    "b": {"type": "pane", "id": "p2", "session": null}}}]}
//! ```
//!
//! A tab's root may instead be a flywheel board bound to one of the
//! workspace's folders (never nested in a split; one per folder):
//! `{"type": "board", "id": "b1", "folder": "/abs/path"}`.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Most tabs one workspace layout holds.
pub const MAX_TABS: usize = 12;
/// Most panes one tab holds.
pub const MAX_PANES: usize = 12;
/// Longest tab name, in characters.
pub const MAX_TAB_NAME: usize = 32;
/// Longest tab / pane id.
pub const MAX_ID: usize = 32;
/// The narrowest share a split gives either side.
pub const MIN_RATIO: f64 = 0.05;

/// One workspace's layout. `rev` 0 with no tabs: never saved.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Layout {
    pub rev: u64,
    #[serde(default)]
    pub tabs: Vec<Tab>,
}

/// A named arrangement of panes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub name: String,
    pub root: Node,
}

/// Which way a split lays out its two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDir {
    /// Side by side: `a` left, `b` right.
    Right,
    /// Stacked: `a` above, `b` below.
    Down,
}

/// A tab's tree: a pane, or a split of two subtrees -- or, only as a
/// tab's root, a board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    Pane {
        id: String,
        /// The session shown; `None`: an empty pane waiting for one.
        #[serde(default)]
        session: Option<String>,
    },
    Split {
        dir: SplitDir,
        /// `a`'s share of the space, in `[MIN_RATIO, 1 - MIN_RATIO]`.
        ratio: f64,
        a: Box<Node>,
        b: Box<Node>,
    },
    /// The flywheel board of one of the workspace's folders. Holds no
    /// panes, so session pruning never closes it.
    Board {
        id: String,
        /// Canonical absolute path of the workspace folder it shows.
        folder: String,
    },
}

impl Node {
    /// Every pane, left to right / top to bottom.
    pub fn panes(&self) -> Vec<(&str, Option<&str>)> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<(&'a str, Option<&'a str>)>) {
        match self {
            Self::Pane { id, session } => out.push((id, session.as_deref())),
            Self::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
            Self::Board { .. } => {}
        }
    }

    /// Close every pane showing `session_id` (its sibling takes the
    /// split's place).
    fn prune(&mut self, session_id: &str) -> Pruned {
        match self {
            Self::Pane { session, .. } => {
                if session.as_deref() == Some(session_id) {
                    Pruned::Closed
                } else {
                    Pruned::Unchanged
                }
            }
            Self::Board { .. } => Pruned::Unchanged,
            Self::Split { a, b, .. } => match (a.prune(session_id), b.prune(session_id)) {
                (Pruned::Closed, Pruned::Closed) => Pruned::Closed,
                (Pruned::Closed, _) => {
                    let keep = std::mem::replace(b.as_mut(), placeholder());
                    *self = keep;
                    Pruned::Changed
                }
                (_, Pruned::Closed) => {
                    let keep = std::mem::replace(a.as_mut(), placeholder());
                    *self = keep;
                    Pruned::Changed
                }
                (Pruned::Unchanged, Pruned::Unchanged) => Pruned::Unchanged,
                _ => Pruned::Changed,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pruned {
    Unchanged,
    Changed,
    /// Every pane of this subtree showed the session: remove it.
    Closed,
}

fn placeholder() -> Node {
    Node::Pane {
        id: String::new(),
        session: None,
    }
}

impl Layout {
    /// Close every pane showing `session_id`: its sibling takes the
    /// split's place, and a tab left without panes closes -- as closing
    /// its last pane does in the UI -- unless it is the workspace's only
    /// tab of panes, which stays as one empty pane. Board tabs are never
    /// closed. Returns whether the layout changed (the caller bumps `rev`).
    pub fn prune_session(&mut self, session_id: &str) -> bool {
        let mut changed = false;
        let mut emptied = Vec::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            match tab.root.prune(session_id) {
                Pruned::Unchanged => {}
                Pruned::Changed => changed = true,
                Pruned::Closed => {
                    let id = tab
                        .root
                        .panes()
                        .first()
                        .map(|(id, _)| (*id).to_owned())
                        .unwrap_or_default();
                    tab.root = Node::Pane { id, session: None };
                    emptied.push(index);
                    changed = true;
                }
            }
        }
        // Emptied tabs go, the first one staying when no other pane tab
        // would.
        let pane_tabs = self.tabs.iter().filter(|t| !t.is_board()).count();
        let keep_one = emptied.len() == pane_tabs;
        for index in emptied.into_iter().skip(usize::from(keep_one)).rev() {
            self.tabs.remove(index);
        }
        changed
    }

    /// Close every board tab whose folder `keep` refuses (one the
    /// workspace no longer has). Returns whether the layout changed.
    pub fn prune_boards(&mut self, keep: impl Fn(&str) -> bool) -> bool {
        let before = self.tabs.len();
        self.tabs.retain(|t| match &t.root {
            Node::Board { folder, .. } => keep(folder),
            _ => true,
        });
        self.tabs.len() != before
    }

    /// Whether any pane shows `session_id`.
    pub fn shows(&self, session_id: &str) -> bool {
        self.tabs
            .iter()
            .any(|t| t.root.panes().iter().any(|(_, s)| *s == Some(session_id)))
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Tab {
    /// Whether this tab is a board (rather than panes).
    pub fn is_board(&self) -> bool {
        matches!(self.root, Node::Board { .. })
    }
}

/// Check `tabs` (as a client sent them) and normalize tab names (trimmed).
/// `sessions`: the ids of the workspace's sessions; a pane may show only
/// one of those, and a session at most once per tab. `folders`: the
/// workspace's folder paths; a board may show only one of those, at most
/// once per layout, and only as a tab's root.
pub fn validate(
    tabs: &mut [Tab],
    sessions: &HashSet<&str>,
    folders: &HashSet<&str>,
) -> Result<(), String> {
    if tabs.len() > MAX_TABS {
        return Err(format!("at most {MAX_TABS} tabs per workspace"));
    }
    let mut ids: HashSet<String> = HashSet::new();
    let mut names: HashSet<String> = HashSet::new();
    let mut boards: HashSet<String> = HashSet::new();
    for tab in tabs.iter_mut() {
        if !valid_id(&tab.id) {
            return Err(format!(
                "tab id {:?}: 1-{MAX_ID} letters, digits, '-' or '_'",
                tab.id
            ));
        }
        if !ids.insert(tab.id.clone()) {
            return Err(format!("id {:?} is used twice", tab.id));
        }
        let name = tab.name.trim();
        if name.is_empty() {
            return Err("a tab name must not be empty".into());
        }
        if name.chars().count() > MAX_TAB_NAME {
            return Err(format!(
                "tab name {name:?} is over {MAX_TAB_NAME} characters"
            ));
        }
        if name.chars().any(char::is_control) {
            return Err(format!("tab name {name:?} has control characters"));
        }
        if !names.insert(name.to_lowercase()) {
            return Err(format!("two tabs are named {name:?}"));
        }
        tab.name = name.to_owned();
        if let Node::Board { id, folder } = &tab.root {
            check_id("board", id, &mut ids)?;
            if !folders.contains(folder.as_str()) {
                return Err(format!(
                    "board folder {folder:?} is not a folder of this workspace"
                ));
            }
            if !boards.insert(folder.clone()) {
                return Err(format!("two boards show folder {folder:?}"));
            }
            continue;
        }
        let mut shown: HashSet<&str> = HashSet::new();
        let mut count = 0;
        check_node(
            &tab.root, &tab.name, sessions, &mut ids, &mut shown, &mut count,
        )?;
    }
    Ok(())
}

fn check_node<'a>(
    node: &'a Node,
    tab: &str,
    sessions: &HashSet<&str>,
    ids: &mut HashSet<String>,
    shown: &mut HashSet<&'a str>,
    count: &mut usize,
) -> Result<(), String> {
    match node {
        Node::Pane { id, session } => {
            *count += 1;
            if *count > MAX_PANES {
                return Err(format!("tab {tab:?}: at most {MAX_PANES} panes"));
            }
            check_id("pane", id, ids)?;
            if let Some(session) = session {
                if !sessions.contains(session.as_str()) {
                    return Err(format!(
                        "session {session} is not a session of this workspace"
                    ));
                }
                if !shown.insert(session) {
                    return Err(format!("tab {tab:?} shows session {session} in two panes"));
                }
            }
            Ok(())
        }
        Node::Split { ratio, a, b, .. } => {
            if !ratio.is_finite() || *ratio < MIN_RATIO || *ratio > 1.0 - MIN_RATIO {
                return Err(format!(
                    "split ratio {ratio} is outside {MIN_RATIO}..{}",
                    1.0 - MIN_RATIO
                ));
            }
            check_node(a, tab, sessions, ids, shown, count)?;
            check_node(b, tab, sessions, ids, shown, count)
        }
        Node::Board { .. } => Err(format!(
            "tab {tab:?}: a board must be a tab's root, not inside a split"
        )),
    }
}

fn check_id(kind: &str, id: &str, ids: &mut HashSet<String>) -> Result<(), String> {
    if !valid_id(id) {
        return Err(format!(
            "{kind} id {id:?}: 1-{MAX_ID} letters, digits, '-' or '_'"
        ));
    }
    if !ids.insert(id.to_owned()) {
        return Err(format!("id {id:?} is used twice"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, session: Option<&str>) -> Node {
        Node::Pane {
            id: id.into(),
            session: session.map(Into::into),
        }
    }

    fn split(dir: SplitDir, a: Node, b: Node) -> Node {
        Node::Split {
            dir,
            ratio: 0.5,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn tab(id: &str, name: &str, root: Node) -> Tab {
        Tab {
            id: id.into(),
            name: name.into(),
            root,
        }
    }

    fn sessions() -> HashSet<&'static str> {
        ["s1", "s2", "s3"].into_iter().collect()
    }

    fn folders() -> HashSet<&'static str> {
        ["/w/one", "/w/two"].into_iter().collect()
    }

    fn board(id: &str, folder: &str) -> Node {
        Node::Board {
            id: id.into(),
            folder: folder.into(),
        }
    }

    #[test]
    fn parses_and_round_trips_the_documented_shape() {
        let json = r#"{"rev":3,"tabs":[{"id":"t1","name":"agents","root":
            {"type":"split","dir":"right","ratio":0.5,
             "a":{"type":"pane","id":"p1","session":"s1"},
             "b":{"type":"pane","id":"p2","session":null}}}]}"#;
        let layout: Layout = serde_json::from_str(json).unwrap();
        assert_eq!(layout.rev, 3);
        assert_eq!(
            layout.tabs[0].root.panes(),
            vec![("p1", Some("s1")), ("p2", None)]
        );
        let again: Layout = serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn validates_ids_names_ratios_sessions_and_limits() {
        let ok = |tabs: Vec<Tab>| {
            let mut tabs = tabs;
            validate(&mut tabs, &sessions(), &folders()).map(|()| tabs)
        };
        let tabs = ok(vec![
            tab(
                "t1",
                "  agents ",
                split(
                    SplitDir::Right,
                    pane("p1", Some("s1")),
                    pane("p2", Some("s2")),
                ),
            ),
            tab("t2", "logs", pane("p3", Some("s1"))),
        ])
        .unwrap();
        assert_eq!(tabs[0].name, "agents", "names are trimmed");

        let err = |tabs: Vec<Tab>| ok(tabs).unwrap_err();
        assert!(err(vec![tab("t1", " ", pane("p1", None))]).contains("empty"));
        assert!(err(vec![tab("t 1", "a", pane("p1", None))]).contains("tab id"));
        assert!(err(vec![tab("t1", "a", pane("p/1", None))]).contains("pane id"));
        assert!(err(vec![tab("t1", "a", pane("t1", None))]).contains("twice"));
        assert!(
            err(vec![
                tab("t1", "Logs", pane("p1", None)),
                tab("t2", "logs", pane("p2", None))
            ])
            .contains("named")
        );
        assert!(err(vec![tab("t1", "a", pane("p1", Some("nope")))]).contains("not a session"));
        assert!(
            err(vec![tab(
                "t1",
                "a",
                split(
                    SplitDir::Down,
                    pane("p1", Some("s1")),
                    pane("p2", Some("s1"))
                )
            )])
            .contains("two panes")
        );
        let mut bad_ratio = split(SplitDir::Down, pane("p1", None), pane("p2", None));
        if let Node::Split { ratio, .. } = &mut bad_ratio {
            *ratio = 0.99;
        }
        assert!(err(vec![tab("t1", "a", bad_ratio)]).contains("ratio"));
        assert!(err(vec![tab("t1", &"x".repeat(33), pane("p1", None))]).contains("over"));
        assert!(err(vec![tab("t1", "a\u{7}", pane("p1", None))]).contains("control"));

        let many: Vec<Tab> = (0..=MAX_TABS)
            .map(|i| {
                tab(
                    &format!("t{i}"),
                    &format!("n{i}"),
                    pane(&format!("p{i}"), None),
                )
            })
            .collect();
        assert!(err(many).contains("tabs"));
        let mut root = pane("p0", None);
        for i in 1..=MAX_PANES {
            root = split(SplitDir::Right, root, pane(&format!("p{i}"), None));
        }
        assert!(err(vec![tab("t1", "a", root)]).contains("panes"));
    }

    #[test]
    fn pruning_a_session_closes_its_panes() {
        let mut layout = Layout {
            rev: 1,
            tabs: vec![
                tab(
                    "t1",
                    "a",
                    split(
                        SplitDir::Right,
                        pane("p1", Some("s1")),
                        split(SplitDir::Down, pane("p2", Some("s2")), pane("p3", None)),
                    ),
                ),
                tab("t2", "b", pane("p4", Some("s2"))),
            ],
        };
        assert!(layout.shows("s2"));
        assert!(!layout.prune_session("s9"));
        assert!(layout.prune_session("s2"));
        assert!(!layout.shows("s2"));
        // p2 closed (its sibling, an empty pane made on purpose, takes
        // its place); the lone root pane of t2 empties.
        assert_eq!(
            layout.tabs[0].root.panes(),
            vec![("p1", Some("s1")), ("p3", None)]
        );
        // t2 showed only s2: it closes, like its last pane closed in the UI.
        assert_eq!(layout.tabs.len(), 1);
        assert!(layout.prune_session("s1"));
        assert_eq!(layout.tabs[0].root, pane("p3", None));
    }

    #[test]
    fn pruning_the_only_tabs_session_keeps_one_empty_tab() {
        let mut layout = Layout {
            rev: 1,
            tabs: vec![
                tab("t1", "a", pane("p1", Some("s1"))),
                tab("t2", "b", pane("p2", Some("s1"))),
            ],
        };
        assert!(layout.prune_session("s1"));
        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(layout.tabs[0].id, "t1");
        assert_eq!(layout.tabs[0].root, pane("p1", None));
    }

    #[test]
    fn board_tab_parses_as_the_documented_shape() {
        let json = r#"{"rev":1,"tabs":[{"id":"t1","name":"board","root":
            {"type":"board","id":"b1","folder":"/w/one"}}]}"#;
        let layout: Layout = serde_json::from_str(json).unwrap();
        assert_eq!(layout.tabs[0].root, board("b1", "/w/one"));
        assert!(layout.tabs[0].is_board());
        assert!(layout.tabs[0].root.panes().is_empty());
        let raw: serde_json::Value = serde_json::to_value(&layout.tabs[0].root).unwrap();
        assert_eq!(
            raw,
            serde_json::json!({"type": "board", "id": "b1", "folder": "/w/one"})
        );
    }

    #[test]
    fn board_tabs_validate_only_as_roots_of_workspace_folders() {
        let check = |tabs: Vec<Tab>| {
            let mut tabs = tabs;
            validate(&mut tabs, &sessions(), &folders())
        };
        check(vec![
            tab("t1", "a", pane("p1", Some("s1"))),
            tab("t2", "one", board("b1", "/w/one")),
            tab("t3", "two", board("b2", "/w/two")),
        ])
        .unwrap();
        let err = |tabs: Vec<Tab>| check(tabs).unwrap_err();
        assert!(err(vec![tab("t1", "a", board("b1", "/elsewhere"))]).contains("not a folder"));
        assert!(
            err(vec![tab(
                "t1",
                "a",
                split(SplitDir::Right, pane("p1", None), board("b1", "/w/one"))
            )])
            .contains("tab's root")
        );
        assert!(
            err(vec![
                tab("t1", "a", board("b1", "/w/one")),
                tab("t2", "b", board("b2", "/w/one"))
            ])
            .contains("two boards")
        );
        assert!(err(vec![tab("t1", "a", board("b/1", "/w/one"))]).contains("board id"));
        assert!(err(vec![tab("t1", "a", board("t1", "/w/one"))]).contains("twice"));
    }

    #[test]
    fn pruning_the_last_session_keeps_the_board_tab() {
        let mut layout = Layout {
            rev: 1,
            tabs: vec![
                tab("t1", "a", pane("p1", Some("s1"))),
                tab("t2", "board", board("b1", "/w/one")),
                tab("t3", "c", pane("p2", Some("s1"))),
            ],
        };
        assert!(layout.prune_session("s1"));
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[0].root, pane("p1", None));
        assert_eq!(layout.tabs[1].root, board("b1", "/w/one"));
    }

    #[test]
    fn pruning_boards_drops_only_those_of_removed_folders() {
        let mut layout = Layout {
            rev: 1,
            tabs: vec![
                tab("t1", "a", pane("p1", None)),
                tab("t2", "one", board("b1", "/w/one")),
                tab("t3", "two", board("b2", "/w/two")),
            ],
        };
        assert!(!layout.prune_boards(|_| true));
        assert!(layout.prune_boards(|f| f == "/w/one"));
        let ids: Vec<&str> = layout.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["t1", "t2"]);
    }
}
