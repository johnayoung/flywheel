//! Operator rulings a lead session keeps (the `record_ruling` tool): the
//! operator's own words on how the work goes ("never merge without a
//! review", "phase 2 keeps the old schema"), which compaction drops more
//! often than the task itself. The hub keeps them on the lead's record and
//! says them again, verbatim, after each compaction and in a handoff brief,
//! and a successor inherits them.
//!
//! * A ruling is the operator's words (`text`), optionally with the agent's
//!   one-line `note`, and who recorded it.
//! * A `topic` makes it replaceable: a newer ruling on the same topic
//!   replaces the older one. Retiring (by id or topic) removes one.
//! * A session keeps at most [`MAX_RULINGS`]; the brief says the newest in
//!   full and shortens older ones past [`BRIEF_MAX_CHARS`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Most rulings one session keeps.
pub const MAX_RULINGS: usize = 40;
/// Longest ruling text (characters).
pub const MAX_TEXT_CHARS: usize = 1000;
/// Longest note (characters).
pub const MAX_NOTE_CHARS: usize = 200;
/// Longest topic (characters).
pub const MAX_TOPIC_CHARS: usize = 64;
/// The brief's rulings block is about this long at most: past it, older
/// rulings are shortened to [`SHORT_CHARS`].
pub const BRIEF_MAX_CHARS: usize = 6000;
/// What a shortened ruling keeps of its text.
pub const SHORT_CHARS: usize = 100;

/// One operator ruling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    /// `r<n>`, unique within the session.
    pub id: String,
    /// Replaces an earlier ruling on the same topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The operator's words.
    pub text: String,
    /// The recording agent's one-line gloss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The session that recorded it (its name).
    pub by: String,
    pub at: DateTime<Utc>,
}

/// Why a ruling change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RulingError(pub String);

/// A ruling to record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewRuling {
    pub text: String,
    pub topic: Option<String>,
    pub note: Option<String>,
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// What the `record_ruling` tool does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulingAction {
    /// Record one (replacing the one on its topic).
    Record(NewRuling),
    /// Retire one, by id or topic.
    Retire(String),
    /// List them.
    List,
}

/// Check a topic: `[a-z0-9][a-z0-9._:/-]*`, at most [`MAX_TOPIC_CHARS`].
pub fn validate_topic(topic: &str) -> Result<(), RulingError> {
    let ok = !topic.is_empty()
        && topic.chars().count() <= MAX_TOPIC_CHARS
        && topic
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && topic.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | ':' | '/' | '-')
        });
    if ok {
        Ok(())
    } else {
        Err(RulingError(format!(
            "topic {topic:?}: 1-{MAX_TOPIC_CHARS} lowercase letters, digits, '.', '_', ':', '/', \
             '-', starting with a letter or digit"
        )))
    }
}

/// The next free `r<n>` id in `rulings`.
fn next_id(rulings: &[Ruling]) -> String {
    let top = rulings
        .iter()
        .filter_map(|r| r.id.strip_prefix('r')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("r{}", top + 1)
}

/// Record `new` in `rulings` (by `by`, at `at`): it replaces the ruling on
/// its topic, if any. Returns the recorded ruling and the one it replaced.
/// On any error nothing changes.
pub fn record(
    rulings: &mut Vec<Ruling>,
    new: &NewRuling,
    by: &str,
    at: DateTime<Utc>,
) -> Result<(Ruling, Option<Ruling>), RulingError> {
    let text = new.text.trim();
    if text.is_empty() {
        return Err(RulingError(
            "give text: the operator's ruling, in their words".into(),
        ));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(RulingError(format!(
            "a ruling is at most {MAX_TEXT_CHARS} characters: record each decision on its own"
        )));
    }
    let topic = trimmed(new.topic.as_deref()).map(|t| t.to_ascii_lowercase());
    if let Some(topic) = &topic {
        validate_topic(topic)?;
    }
    let note = trimmed(new.note.as_deref());
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS || n.contains('\n'))
    {
        return Err(RulingError(format!(
            "a note is one line of at most {MAX_NOTE_CHARS} characters"
        )));
    }
    let replaced_at = topic
        .as_ref()
        .and_then(|t| rulings.iter().position(|r| r.topic.as_ref() == Some(t)));
    if replaced_at.is_none() && rulings.len() >= MAX_RULINGS {
        return Err(RulingError(format!(
            "a session keeps at most {MAX_RULINGS} rulings: retire ones that no longer hold, \
             or record this one on the topic of the one it replaces"
        )));
    }
    let ruling = Ruling {
        id: next_id(rulings),
        topic,
        text: text.to_owned(),
        note,
        by: by.to_owned(),
        at,
    };
    let replaced = replaced_at.map(|i| rulings.remove(i));
    rulings.push(ruling.clone());
    Ok((ruling, replaced))
}

/// Remove the ruling `which` names (its id or its topic); the removed one.
pub fn retire(rulings: &mut Vec<Ruling>, which: &str) -> Result<Ruling, RulingError> {
    let which = which.trim();
    let at = rulings
        .iter()
        .position(|r| r.id == which)
        .or_else(|| {
            let topic = which.to_ascii_lowercase();
            rulings
                .iter()
                .position(|r| r.topic.as_deref() == Some(topic.as_str()))
        })
        .ok_or_else(|| {
            RulingError(format!(
                "no ruling has id or topic {which:?} (record_ruling with no arguments lists them)"
            ))
        })?;
    Ok(rulings.remove(at))
}

/// Add `inherited` (a predecessor's rulings) to `rulings`: each one whose
/// topic `rulings` does not already hold and whose text it does not
/// already have, under a fresh id, up to [`MAX_RULINGS`]. How many were
/// added.
pub fn inherit(rulings: &mut Vec<Ruling>, inherited: &[Ruling]) -> usize {
    let mut added = 0;
    for ruling in inherited {
        if rulings.len() >= MAX_RULINGS {
            break;
        }
        let held = rulings
            .iter()
            .any(|r| r.text == ruling.text || (ruling.topic.is_some() && r.topic == ruling.topic));
        if held {
            continue;
        }
        let mut copy = ruling.clone();
        copy.id = next_id(rulings);
        rulings.push(copy);
        added += 1;
    }
    added
}

/// One ruling's line, its text cut to `chars` when given.
fn line(ruling: &Ruling, chars: Option<usize>) -> String {
    let topic = ruling
        .topic
        .as_ref()
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default();
    let text = match chars {
        Some(n) if ruling.text.chars().count() > n => {
            let cut: String = ruling.text.chars().take(n).collect();
            format!("{cut}... (shortened)")
        }
        _ => ruling.text.clone(),
    };
    let note = ruling
        .note
        .as_ref()
        .map(|n| format!(" -- {n}"))
        .unwrap_or_default();
    format!(
        "- {}{topic} \"{text}\"{note} ({}, {})",
        ruling.id,
        ruling.by,
        ruling.at.format("%Y-%m-%d %H:%M UTC")
    )
}

/// The lines saying `rulings`, oldest first: the newest in full, older
/// ones shortened once the block would pass [`BRIEF_MAX_CHARS`].
pub fn lines(rulings: &[Ruling]) -> Vec<String> {
    let mut out = Vec::with_capacity(rulings.len());
    let mut used = 0;
    let mut shortened = false;
    for ruling in rulings.iter().rev() {
        let full = line(ruling, None);
        let chosen = if used + full.len() <= BRIEF_MAX_CHARS {
            full
        } else {
            shortened = true;
            line(ruling, Some(SHORT_CHARS))
        };
        used += chosen.len() + 1;
        out.push(chosen);
    }
    out.reverse();
    if shortened {
        out.push(
            "(Some are shortened: record_ruling with no arguments lists them in full; retire \
             the ones that no longer hold.)"
                .to_owned(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-11-23T10:{minute:02}:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn new(text: &str, topic: Option<&str>) -> NewRuling {
        NewRuling {
            text: text.to_owned(),
            topic: topic.map(str::to_owned),
            note: None,
        }
    }

    #[test]
    fn a_ruling_on_a_topic_replaces_the_older_one() {
        let mut rulings = Vec::new();
        record(
            &mut rulings,
            &new("keep the old schema", Some("schema")),
            "lead",
            at(1),
        )
        .unwrap();
        record(
            &mut rulings,
            &new("no merges on friday", None),
            "lead",
            at(2),
        )
        .unwrap();
        let (ruling, replaced) = record(
            &mut rulings,
            &new("move to schema v2", Some("Schema")),
            "lead",
            at(3),
        )
        .unwrap();
        assert_eq!(ruling.id, "r3");
        assert_eq!(replaced.unwrap().text, "keep the old schema");
        let texts: Vec<&str> = rulings.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["no merges on friday", "move to schema v2"]);
    }

    #[test]
    fn retire_by_id_or_topic_and_refuse_what_is_not_there() {
        let mut rulings = Vec::new();
        record(&mut rulings, &new("a", Some("one")), "lead", at(1)).unwrap();
        record(&mut rulings, &new("b", None), "lead", at(2)).unwrap();
        assert_eq!(retire(&mut rulings, "r2").unwrap().text, "b");
        assert_eq!(retire(&mut rulings, "one").unwrap().text, "a");
        assert!(retire(&mut rulings, "r1").is_err());
        assert!(rulings.is_empty());
    }

    #[test]
    fn bad_rulings_are_refused_whole() {
        let mut rulings = Vec::new();
        assert!(record(&mut rulings, &new("  ", None), "lead", at(1)).is_err());
        assert!(record(&mut rulings, &new("x", Some("Bad Topic")), "lead", at(1)).is_err());
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(record(&mut rulings, &new(&long, None), "lead", at(1)).is_err());
        let noted = NewRuling {
            note: Some("two\nlines".into()),
            ..new("x", None)
        };
        assert!(record(&mut rulings, &noted, "lead", at(1)).is_err());
        assert!(rulings.is_empty());
        for n in 0..MAX_RULINGS {
            record(
                &mut rulings,
                &new(&format!("rule {n}"), None),
                "lead",
                at(1),
            )
            .unwrap();
        }
        assert!(record(&mut rulings, &new("one more", None), "lead", at(2)).is_err());
    }

    #[test]
    fn a_successor_inherits_what_it_does_not_hold() {
        let mut old = Vec::new();
        record(
            &mut old,
            &new("keep the old schema", Some("schema")),
            "lead",
            at(1),
        )
        .unwrap();
        record(&mut old, &new("no merges on friday", None), "lead", at(2)).unwrap();
        record(
            &mut old,
            &new("review every milestone", None),
            "lead",
            at(3),
        )
        .unwrap();
        let mut mine = Vec::new();
        record(
            &mut mine,
            &new("schema v2 now", Some("schema")),
            "lead-2",
            at(4),
        )
        .unwrap();
        record(
            &mut mine,
            &new("no merges on friday", None),
            "lead-2",
            at(5),
        )
        .unwrap();
        assert_eq!(inherit(&mut mine, &old), 1);
        let last = mine.last().unwrap();
        assert_eq!(
            (last.id.as_str(), last.text.as_str()),
            ("r3", "review every milestone")
        );
        assert_eq!(last.by, "lead");
    }

    #[test]
    fn the_brief_keeps_the_newest_in_full() {
        let mut rulings = Vec::new();
        for n in 0..12 {
            let text = format!("{n} {}", "y".repeat(800));
            record(&mut rulings, &new(&text, None), "lead", at(n)).unwrap();
        }
        let lines = lines(&rulings);
        assert_eq!(lines.len(), 13);
        assert!(lines[0].contains("(shortened)"), "{}", lines[0]);
        assert!(!lines[11].contains("(shortened)"));
        assert!(lines[12].contains("shortened"));
        let total: usize = lines.iter().map(|l| l.len() + 1).sum();
        assert!(total < BRIEF_MAX_CHARS + 2000, "{total}");
    }
}
