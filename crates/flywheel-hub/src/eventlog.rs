//! An event-log filter as people and agents write it (`fw hub wait
//! --fleet`, watches, the `watch` tool): space- or comma-separated terms.
//!
//! ```text
//! state=done|blocked|failed children        # a lead's children, finishing
//! team=web kind=review                      # labels, and an event family
//! session=api ws=web state=blocked
//! ```
//!
//! `session=`, `ws=`, `state=`, `kind=` (`review`: `review.*`) and
//! `children[=SESSION]` (the sessions SESSION opened; bare: the watch's
//! owner's) are fields; any other `KEY[=V1|V2]` is a label term (see
//! [`crate::labels`]). Terms of one field are alternatives (`|`); fields
//! and labels must all hold.

use crate::labels::{self, Term};
use serde::{Deserialize, Serialize};

/// The statuses a state term may name (the fleet's).
pub const STATES: &[&str] = &[
    "blocked",
    "asking",
    "failed",
    "compacting",
    "working",
    "background",
    "done",
    "idle",
    "ended",
];

/// A parsed filter. Empty fields select everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    /// Sessions (ids, id prefixes, or names; ids once resolved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    /// Workspaces (ids, prefixes, or names; ids once resolved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// Statuses an event moved its session to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// Event kinds, or families (`review`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Term>,
    /// The sessions this one opened (`""`: the owner's; its id once
    /// resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<String>,
}

fn alternatives(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Filter {
    /// Parse a filter string (see the module doc).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut filter = Self::default();
        for word in raw
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
        {
            let (key, value) = labels::split_pair(word);
            let need = |field: &str| -> Result<Vec<String>, String> {
                let values = value.map(alternatives).unwrap_or_default();
                if values.is_empty() {
                    Err(format!("{field}= needs a value: {field}=A|B"))
                } else {
                    Ok(values)
                }
            };
            match key {
                "session" => filter.sessions.extend(need("session")?),
                "ws" => filter.workspaces.extend(need("ws")?),
                "state" => {
                    for state in need("state")? {
                        if !STATES.contains(&state.as_str()) {
                            return Err(format!(
                                "unknown state {state:?}: one of {}",
                                STATES.join(", ")
                            ));
                        }
                        filter.states.push(state);
                    }
                }
                "kind" => filter.kinds.extend(need("kind")?),
                "children" => {
                    if filter.children.is_some() {
                        return Err("name children once".into());
                    }
                    filter.children = Some(value.unwrap_or("").to_owned());
                }
                _ => filter
                    .labels
                    .push(Term::parse(word).map_err(|e| e.to_string())?),
            }
        }
        Ok(filter)
    }

    /// Whether it selects every event.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for Filter {
    /// The filter as [`Filter::parse`] reads it back.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut words = Vec::new();
        let field = |name: &str, values: &[String]| format!("{name}={}", values.join("|"));
        if !self.sessions.is_empty() {
            words.push(field("session", &self.sessions));
        }
        if !self.workspaces.is_empty() {
            words.push(field("ws", &self.workspaces));
        }
        match self.children.as_deref() {
            Some("") => words.push("children".to_owned()),
            Some(of) => words.push(format!("children={of}")),
            None => {}
        }
        if !self.states.is_empty() {
            words.push(field("state", &self.states));
        }
        if !self.kinds.is_empty() {
            words.push(field("kind", &self.kinds));
        }
        words.extend(self.labels.iter().map(ToString::to_string));
        f.write_str(&words.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_reads_fields_and_labels_and_prints_back() {
        let filter = Filter::parse("state=done|blocked,children  team=web kind=review").unwrap();
        assert_eq!(filter.states, ["done", "blocked"]);
        assert_eq!(filter.children.as_deref(), Some(""));
        assert_eq!(filter.kinds, ["review"]);
        assert_eq!(filter.labels.len(), 1);
        assert_eq!(filter.labels[0].key, "team");
        let printed = filter.to_string();
        assert_eq!(printed, "children state=done|blocked kind=review team=web");
        assert_eq!(Filter::parse(&printed).unwrap(), filter);
        let named = Filter::parse("children=lead session=a|b ws=w").unwrap();
        assert_eq!(named.children.as_deref(), Some("lead"));
        assert_eq!(named.sessions, ["a", "b"]);
        assert!(Filter::parse("").unwrap().is_empty());
    }

    #[test]
    fn bad_filters_say_why() {
        assert!(
            Filter::parse("state=sleeping")
                .unwrap_err()
                .contains("unknown state")
        );
        assert!(Filter::parse("state=").is_err());
        assert!(Filter::parse("children children=x").is_err());
        assert!(Filter::parse("Team=web").is_err());
    }
}
