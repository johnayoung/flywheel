//! Session labels: `key=value` pairs on a session, one value per key, the
//! grouping and routing axis for many sessions at once (`fw hub ls
//! --label team=web`, broadcast to a label, a fleet filtered by one).
//!
//! * Keys are short lowercase words (`team`, `area.db`); values one word
//!   (no whitespace, `,` or `|`, which filters use). A session holds at
//!   most [`MAX_LABELS`].
//! * `fw.*` keys are the hub's: set at the transition they name
//!   ([`KIND`], [`PARENT`], [`CHANGE`], [`PR`], [`VERIFIED`]), never by an
//!   operator or agent. [`STATE`] is live: it reads the session's fleet
//!   status when asked and is never stored, so a status change is not
//!   also a label change.
//! * A set may be conditional ([`Cond`]): only if the key now holds a
//!   value, or holds none (a claim: the first `--if-unset` wins).
//! * A session labelled [`EVAL`] (`eval=<run>`, set by anyone) or
//!   [`EVAL_RUN`] (`fw.eval=<run>`, set by the hub on a captured run whose
//!   batch is `eval:<run>:<trial>`) is an eval trial, exempt from telemetry
//!   pruning: its trace outlives the retention window.
//! * Every change is recorded in the session's telemetry log
//!   (`labels.changed`), so a collector's fleet and the event log filter
//!   by labels too.

use std::collections::BTreeMap;

/// A session's labels.
pub type Labels = BTreeMap<String, String>;
/// A change: each key to its new value, or `None` to unset it.
pub type Change = BTreeMap<String, Option<String>>;

/// The label that pins a session's telemetry past the retention window
/// (`eval=<run>`): an eval trial's trace is its evidence.
pub const EVAL: &str = "eval";
/// The hub's own eval label: the run a captured `fw eval` trial belongs
/// to, from its batch ([`eval_run_of_batch`]); no agent can unset it.
pub const EVAL_RUN: &str = "fw.eval";
/// The batch prefix an `fw eval` trial's launcher uses.
pub const EVAL_BATCH_PREFIX: &str = "eval:";

/// The eval run a batch label names (`eval:<run>:<trial>` or
/// `eval:<run>`), when it is one and the run is a valid label value.
pub fn eval_run_of_batch(batch: &str) -> Option<&str> {
    let rest = batch.strip_prefix(EVAL_BATCH_PREFIX)?;
    let run = rest.split(':').next()?;
    (validate_value(EVAL_RUN, run).is_ok()).then_some(run)
}

/// Most labels on one session.
pub const MAX_LABELS: usize = 32;
/// Longest key, and longest value (characters).
pub const MAX_KEY_CHARS: usize = 63;
pub const MAX_VALUE_CHARS: usize = 128;
/// The hub's own keys start with this.
pub const HUB_PREFIX: &str = "fw.";
/// The session's kind (`shell`, `claude`, `codex`).
pub const KIND: &str = "fw.kind";
/// The agent session that opened it (its id).
pub const PARENT: &str = "fw.parent";
/// The change it works on: its worktree's branch.
pub const CHANGE: &str = "fw.change";
/// Its branch's pull request number, once one is known.
pub const PR: &str = "fw.pr";
/// What a fan-out's graders said of its work: `pass` or `fail`.
pub const VERIFIED: &str = "fw.verified";
/// Its fleet status, read live (never stored).
pub const STATE: &str = "fw.state";
/// Words a filter uses for its own fields: never label keys.
pub const FILTER_WORDS: &[&str] = &["session", "ws", "state", "kind", "children", "after"];

/// A condition on one key, checked under the same lock as the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    /// The key holds exactly this value now.
    Is(String),
    /// The key holds no value now.
    Unset,
}

/// Why a label change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("{0}")]
    Invalid(String),
    /// The condition did not hold: nothing changed.
    #[error("{0}")]
    Condition(String),
}

/// Check a key: `[a-z0-9][a-z0-9._-]*`, at most [`MAX_KEY_CHARS`], not a
/// filter word; `fw.*` only when `hub` sets it.
pub fn validate_key(key: &str, hub: bool) -> Result<(), LabelError> {
    let bad = |why: &str| Err(LabelError::Invalid(format!("label key {key:?}: {why}")));
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return bad(&format!("1-{MAX_KEY_CHARS} characters"));
    }
    let mut chars = key.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok
        || !key.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
        })
    {
        return bad("lowercase letters, digits, '.', '_', '-', starting with a letter or digit");
    }
    if FILTER_WORDS.contains(&key) {
        return bad("a filter word (session, ws, state, kind, children, after)");
    }
    if key.starts_with(HUB_PREFIX) && !hub {
        return bad("fw.* keys are set by the hub");
    }
    Ok(())
}

/// Check a value: 1..=[`MAX_VALUE_CHARS`] characters, no whitespace,
/// control characters, `,` or `|`.
pub fn validate_value(key: &str, value: &str) -> Result<(), LabelError> {
    if value.is_empty() || value.chars().count() > MAX_VALUE_CHARS {
        return Err(LabelError::Invalid(format!(
            "label {key}: a value is 1-{MAX_VALUE_CHARS} characters (unset it instead of emptying it)"
        )));
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == ',' || c == '|')
    {
        return Err(LabelError::Invalid(format!(
            "label {key}: a value has no spaces, ',' or '|'"
        )));
    }
    Ok(())
}

/// Apply `change` to `labels` (only `fw.*` keys when `hub`, none of them
/// otherwise), first checking `cond` against `change`'s one key. Returns
/// what each changed key held before (`None`: unset); keys set to what
/// they already hold are left out. On any error nothing changes.
pub fn apply(
    labels: &mut Labels,
    change: &Change,
    cond: Option<&Cond>,
    hub: bool,
) -> Result<Change, LabelError> {
    for (key, value) in change {
        validate_key(key, hub)?;
        if key == STATE {
            return Err(LabelError::Invalid(format!(
                "{STATE} is the session's live status: it is read, never set"
            )));
        }
        if let Some(value) = value {
            validate_value(key, value)?;
        }
    }
    if let Some(cond) = cond {
        let mut keys = change.keys();
        let (Some(key), None) = (keys.next(), keys.next()) else {
            return Err(LabelError::Invalid(
                "a conditional set changes exactly one label".into(),
            ));
        };
        let now = labels.get(key);
        let holds = match cond {
            Cond::Is(want) => now == Some(want),
            Cond::Unset => now.is_none(),
        };
        if !holds {
            return Err(LabelError::Condition(match (cond, now) {
                (Cond::Is(want), Some(now)) => format!("{key} is {now}, not {want}"),
                (Cond::Is(want), None) => format!("{key} is unset, not {want}"),
                (Cond::Unset, Some(now)) => format!("{key} is already {now}"),
                (Cond::Unset, None) => unreachable!("an unset key holds"),
            }));
        }
    }
    let added = change
        .iter()
        .filter(|(k, v)| v.is_some() && !labels.contains_key(*k))
        .count();
    if labels.len() + added > MAX_LABELS {
        return Err(LabelError::Invalid(format!(
            "a session holds at most {MAX_LABELS} labels"
        )));
    }
    let mut before = Change::new();
    for (key, value) in change {
        let old = labels.get(key).cloned();
        if old == *value {
            continue;
        }
        match value {
            Some(value) => labels.insert(key.clone(), value.clone()),
            None => labels.remove(key),
        };
        before.insert(key.clone(), old);
    }
    Ok(before)
}

/// One `KEY` or `KEY=VALUE[|VALUE...]` pair a filter or a set is given.
pub fn split_pair(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once('=') {
        Some((key, value)) => (key.trim(), Some(value.trim())),
        None => (raw.trim(), None),
    }
}

/// `KEY=VALUE` arguments as a change (`KEY=` unsets; `unset` keys too).
pub fn parse_change(pairs: &[String], unset: &[String]) -> Result<Change, LabelError> {
    let mut change = Change::new();
    for pair in pairs {
        let (key, value) = split_pair(pair);
        let Some(value) = value else {
            return Err(LabelError::Invalid(format!(
                "{pair:?}: give KEY=VALUE (KEY= or --unset KEY to remove one)"
            )));
        };
        change.insert(
            key.to_owned(),
            Some(value.to_owned()).filter(|v| !v.is_empty()),
        );
    }
    for key in unset {
        change.insert(key.trim().to_owned(), None);
    }
    Ok(change)
}

/// One label term of a filter: the key holds one of `values` (empty: any
/// value).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Term {
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

impl Term {
    /// `KEY` or `KEY=V1|V2`.
    pub fn parse(raw: &str) -> Result<Self, LabelError> {
        let (key, values) = split_pair(raw);
        validate_key(key, true)?;
        let values: Vec<String> = match values {
            None => Vec::new(),
            Some(values) => values
                .split('|')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        if raw.contains('=') && values.is_empty() {
            return Err(LabelError::Invalid(format!(
                "{raw:?}: give KEY or KEY=VALUE[|VALUE...]"
            )));
        }
        for value in &values {
            validate_value(key, value)?;
        }
        Ok(Self {
            key: key.to_owned(),
            values,
        })
    }

    /// Whether `value` (the key's, `None`: unset) satisfies it.
    pub fn holds(&self, value: Option<&str>) -> bool {
        value.is_some_and(|v| self.values.is_empty() || self.values.iter().any(|w| w == v))
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.values.is_empty() {
            f.write_str(&self.key)
        } else {
            write!(f, "{}={}", self.key, self.values.join("|"))
        }
    }
}

/// Each `--label` argument (or a comma list of them) as terms.
pub fn parse_terms<S: AsRef<str>>(raw: &[S]) -> Result<Vec<Term>, LabelError> {
    raw.iter()
        .flat_map(|r| r.as_ref().split(',').map(str::to_owned).collect::<Vec<_>>())
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .map(|r| Term::parse(&r))
        .collect()
}

/// Whether every term holds for a session whose labels are `labels` and
/// whose live status is `state` ([`STATE`]).
pub fn matches(terms: &[Term], labels: &Labels, state: Option<&str>) -> bool {
    terms.iter().all(|term| {
        let value = if term.key == STATE {
            state
        } else {
            labels.get(&term.key).map(String::as_str)
        };
        term.holds(value)
    })
}

/// Terms as one query value (`team=web,fw.state=done|blocked`).
pub fn query_value(terms: &[Term]) -> String {
    terms
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(pairs: &[(&str, Option<&str>)]) -> Change {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn keys_and_values_are_checked() {
        assert!(validate_key("team", false).is_ok());
        assert!(validate_key("area.db-2", false).is_ok());
        for bad in [
            "",
            "Team",
            "-x",
            "a b",
            "state",
            "children",
            &"k".repeat(64),
        ] {
            assert!(validate_key(bad, false).is_err(), "{bad:?}");
        }
        assert!(validate_key("fw.pr", false).is_err());
        assert!(validate_key("fw.pr", true).is_ok());
        assert!(validate_value("k", "agent/x-1").is_ok());
        for bad in ["", "a b", "a,b", "a|b", &"v".repeat(129)] {
            assert!(validate_value("k", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_set_reports_what_changed_and_conditions_hold_or_refuse() {
        let mut labels = Labels::new();
        let before = apply(&mut labels, &change(&[("team", Some("web"))]), None, false).unwrap();
        assert_eq!(before, change(&[("team", None)]));
        // The same value again: nothing changed.
        let before = apply(&mut labels, &change(&[("team", Some("web"))]), None, false).unwrap();
        assert!(before.is_empty());
        // A claim: the first wins, the second is refused and changes nothing.
        let claim = change(&[("owner", Some("a"))]);
        apply(&mut labels, &claim, Some(&Cond::Unset), false).unwrap();
        let other = change(&[("owner", Some("b"))]);
        let refused = apply(&mut labels, &other, Some(&Cond::Unset), false).unwrap_err();
        assert_eq!(refused, LabelError::Condition("owner is already a".into()));
        assert_eq!(labels["owner"], "a");
        let moved = apply(&mut labels, &other, Some(&Cond::Is("a".into())), false).unwrap();
        assert_eq!(moved, change(&[("owner", Some("a"))]));
        let refused = apply(&mut labels, &claim, Some(&Cond::Is("a".into())), false).unwrap_err();
        assert!(matches!(refused, LabelError::Condition(_)));
        // Unset.
        apply(&mut labels, &change(&[("owner", None)]), None, false).unwrap();
        assert!(!labels.contains_key("owner"));
        // Conditions take one key; fw.state is never set.
        let two = change(&[("a", Some("1")), ("b", Some("2"))]);
        assert!(apply(&mut labels, &two, Some(&Cond::Unset), false).is_err());
        assert!(apply(&mut labels, &change(&[(STATE, Some("done"))]), None, true).is_err());
    }

    #[test]
    fn a_session_holds_a_bounded_number_of_labels() {
        let mut labels = Labels::new();
        let many: Change = (0..MAX_LABELS)
            .map(|i| (format!("k{i}"), Some("v".to_owned())))
            .collect();
        apply(&mut labels, &many, None, false).unwrap();
        let more = change(&[("one", Some("more"))]);
        assert!(apply(&mut labels, &more, None, false).is_err());
        // Changing one it holds is fine.
        apply(&mut labels, &change(&[("k0", Some("w"))]), None, false).unwrap();
    }

    #[test]
    fn terms_match_values_presence_and_the_live_state() {
        let terms = parse_terms(&["team=web|api", "pr"]).unwrap();
        let mut labels = Labels::new();
        labels.insert("team".into(), "api".into());
        assert!(!matches(&terms, &labels, None));
        labels.insert("pr".into(), "12".into());
        assert!(matches(&terms, &labels, None));
        let state = parse_terms(&["fw.state=done|blocked,team"]).unwrap();
        assert_eq!(state.len(), 2);
        assert!(matches(&state, &labels, Some("done")));
        assert!(!matches(&state, &labels, Some("working")));
        assert_eq!(query_value(&state), "fw.state=done|blocked,team");
        assert!(parse_terms(&["team="]).is_err());
        assert!(parse_change(&["team".into()], &[]).is_err());
        assert_eq!(
            parse_change(&["a=1".into(), "b=".into()], &["c".into()]).unwrap(),
            change(&[("a", Some("1")), ("b", None), ("c", None)])
        );
    }
}
