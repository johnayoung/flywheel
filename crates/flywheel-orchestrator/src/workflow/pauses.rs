//! `fw pauses` / `fw pauses clear`: the session-limit pause surface.
//!
//! The reads here are the single pause reader every surface shares:
//! [`pause_entries`] (every pause, newest first), [`active_pauses`] (the
//! subset that still holds workers back), and [`pause_label`] (the
//! `@<name>` / `cli login` / `all accounts` rule). `fw status`, the
//! operator console, and the hub call these, never reimplement them.
//! [`clear_session_pauses`] is the one clear path; it appends an attributed
//! clear record per pause and never rewrites or deletes a pause row.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::claims::{
    ClaimStore, ClaimStoreError, DEFAULT_PAUSE_ACCOUNT, PAUSE_CLEAR_VIA_CLI, SessionPauseClear,
    SessionPauseRecord,
};

use super::args::{PausesAction, PausesArgs};
use super::{WorkflowError, py_json_dumps_indent2};

/// The label of a pause recorded by a worker with no `--account`.
pub const DEFAULT_ACCOUNT_LABEL: &str = "cli login";
/// The label of a pre-account (`NULL`) pause, which holds back every account.
pub const ALL_ACCOUNTS_LABEL: &str = "all accounts";

/// The operator-facing label of a pause's account: `@<name>` for a named
/// account, `cli login` for [`DEFAULT_PAUSE_ACCOUNT`], `all accounts` for a
/// pre-account (`None`) row.
pub fn pause_label(account: Option<&str>) -> String {
    match account {
        None => ALL_ACCOUNTS_LABEL.to_owned(),
        Some(DEFAULT_PAUSE_ACCOUNT) => DEFAULT_ACCOUNT_LABEL.to_owned(),
        Some(name) => format!("@{name}"),
    }
}

/// A pause timestamp as every surface prints it: RFC 3339, UTC `Z`.
pub fn format_pause_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// One pause as the pause surfaces report it: the stored row, its clear
/// record, and whether it is active at the read's `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseEntry {
    pub id: i64,
    pub account: Option<String>,
    pub run_id: String,
    pub reset_at: DateTime<Utc>,
    pub pause_until: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub active: bool,
    pub cleared: Option<SessionPauseClear>,
}

impl PauseEntry {
    fn from_record(record: SessionPauseRecord, now: DateTime<Utc>) -> Self {
        let active = record.is_active(now);
        Self {
            id: record.id,
            account: record.account,
            run_id: record.run_id,
            reset_at: record.reset_at,
            pause_until: record.pause_until,
            recorded_at: record.recorded_at,
            active,
            cleared: record.cleared,
        }
    }

    /// This entry's [`pause_label`].
    pub fn label(&self) -> String {
        pause_label(self.account.as_deref())
    }

    /// The `fw pauses --json` object: exactly `{id, account, run_id,
    /// reset_at, pause_until, recorded_at, active, cleared}`, `cleared`
    /// being `null` or `{at, via, reason}`.
    pub fn to_json(&self) -> Value {
        let cleared = match &self.cleared {
            None => Value::Null,
            Some(clear) => json!({
                "at": format_pause_time(clear.at),
                "via": clear.via,
                "reason": clear.reason,
            }),
        };
        json!({
            "id": self.id,
            "account": self.account,
            "run_id": self.run_id,
            "reset_at": format_pause_time(self.reset_at),
            "pause_until": format_pause_time(self.pause_until),
            "recorded_at": format_pause_time(self.recorded_at),
            "active": self.active,
            "cleared": cleared,
        })
    }
}

/// Every recorded pause, newest first (by `recorded_at`, then id), each
/// marked active or not at `now`.
pub fn pause_entries(
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
) -> Result<Vec<PauseEntry>, ClaimStoreError> {
    let mut entries: Vec<PauseEntry> = claims
        .list_session_pauses()?
        .into_iter()
        .map(|record| PauseEntry::from_record(record, now))
        .collect();
    entries.sort_by(|a, b| {
        b.recorded_at
            .cmp(&a.recorded_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(entries)
}

/// The pauses holding workers back at `now` (unexpired and uncleared),
/// newest first: the one active-pauses read every surface shares.
pub fn active_pauses(
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
) -> Result<Vec<PauseEntry>, ClaimStoreError> {
    Ok(pause_entries(claims, now)?
        .into_iter()
        .filter(|entry| entry.active)
        .collect())
}

/// Which active pauses a clear targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseSelector {
    /// These pause ids (each cleared only if active).
    Ids(Vec<i64>),
    /// Pauses recorded under exactly this account; [`DEFAULT_PAUSE_ACCOUNT`]
    /// (`""`) is the worker's default login (`--default`). Never matches a
    /// pre-account (`None`) row.
    Account(String),
    /// Every active pause, pre-account rows included.
    All,
}

impl PauseSelector {
    /// Whether `entry` is in this selector's scope (activity aside).
    pub fn matches(&self, entry: &PauseEntry) -> bool {
        match self {
            PauseSelector::Ids(ids) => ids.contains(&entry.id),
            PauseSelector::Account(account) => entry.account.as_deref() == Some(account.as_str()),
            PauseSelector::All => true,
        }
    }

    /// How the selector reads in a "nothing matched" message.
    pub fn describe(&self) -> String {
        match self {
            PauseSelector::Ids(ids) => {
                let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
                format!("id {}", ids.join(", "))
            }
            PauseSelector::Account(account) => {
                format!("account {}", pause_label(Some(account.as_str())))
            }
            PauseSelector::All => "any account".to_owned(),
        }
    }
}

/// Clear every active pause `selector` matches at `now`, appending one
/// clear record each (`via`, `reason` verbatim). Returns the cleared ids in
/// ascending order (empty when nothing active matched; nothing is then
/// recorded). An unknown `via` or blank `reason` is refused before any
/// read.
pub fn clear_session_pauses(
    claims: &dyn ClaimStore,
    selector: &PauseSelector,
    now: DateTime<Utc>,
    via: &str,
    reason: &str,
) -> Result<Vec<i64>, ClaimStoreError> {
    crate::claims::require_pause_clear(via, reason)?;
    let mut targets: Vec<i64> = active_pauses(claims, now)?
        .into_iter()
        .filter(|entry| selector.matches(entry))
        .map(|entry| entry.id)
        .collect();
    targets.sort_unstable();
    let mut cleared = Vec::with_capacity(targets.len());
    for id in targets {
        if claims.clear_session_pause(id, now, via, reason)? {
            cleared.push(id);
        }
    }
    Ok(cleared)
}

/// One `fw pauses` text line.
fn format_entry_line(entry: &PauseEntry) -> String {
    let state = match (&entry.cleared, entry.active) {
        (Some(clear), _) => format!(
            "cleared {} via {}: {}",
            format_pause_time(clear.at),
            clear.via,
            clear.reason
        ),
        (None, true) => "active".to_owned(),
        (None, false) => "expired".to_owned(),
    };
    format!(
        "{} {} until {} (run {}) {state}",
        entry.id,
        entry.label(),
        format_pause_time(entry.pause_until),
        entry.run_id
    )
}

/// Run `fw pauses [--json]` or `fw pauses clear SELECTOR --reason TEXT`.
///
/// List: one line per pause (or the `--json` array), newest first; exit 0.
/// Clear: one `cleared <id>` line per cleared pause, attributed `cli`;
/// exit 2 on a blank reason and exit 1 when no active pause matches, both
/// recording nothing.
pub fn cmd_pauses(
    args: &PausesArgs,
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    match &args.action {
        PausesAction::List { json } => {
            let entries = pause_entries(claims, now)?;
            if *json {
                let array = Value::Array(entries.iter().map(PauseEntry::to_json).collect());
                writeln!(out, "{}", py_json_dumps_indent2(&array))?;
            } else if entries.is_empty() {
                writeln!(out, "no session pauses")?;
            } else {
                for entry in &entries {
                    writeln!(out, "{}", format_entry_line(entry))?;
                }
            }
            Ok(0)
        }
        PausesAction::Clear { selector, reason } => {
            if reason.trim().is_empty() {
                writeln!(err, "error: --reason must not be empty")?;
                return Ok(2);
            }
            let cleared = clear_session_pauses(claims, selector, now, PAUSE_CLEAR_VIA_CLI, reason)?;
            if cleared.is_empty() {
                writeln!(
                    err,
                    "error: no active session pause matches {}",
                    selector.describe()
                )?;
                return Ok(1);
            }
            for id in cleared {
                writeln!(out, "cleared {id}")?;
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{InMemoryClaimStore, PAUSE_CLEAR_VIA_HUB};
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap()
    }

    /// Pauses: 1 on A (active), 2 on B (active), 3 on default (active),
    /// 4 on A (expired by `t0() + 2h`).
    fn seeded() -> InMemoryClaimStore {
        let store = InMemoryClaimStore::new();
        let rec = |reset_h: i64, at_min: i64, run: &str, account: &str| {
            store
                .record_session_pause(
                    t0() + Duration::hours(reset_h),
                    t0() + Duration::minutes(at_min),
                    86_400.0,
                    run,
                    account,
                )
                .unwrap()
                .unwrap();
        };
        rec(5, 0, "run-a", "A");
        rec(5, 1, "run-b", "B");
        rec(5, 2, "run-d", DEFAULT_PAUSE_ACCOUNT);
        rec(1, 3, "run-a-old", "A");
        store
    }

    fn now() -> DateTime<Utc> {
        t0() + Duration::hours(2)
    }

    fn clear_args(selector: PauseSelector, reason: &str) -> PausesArgs {
        PausesArgs {
            action: PausesAction::Clear {
                selector,
                reason: reason.to_owned(),
            },
            db: None,
        }
    }

    fn run(args: &PausesArgs, store: &dyn ClaimStore) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cmd_pauses(args, store, now(), &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn pauses_verb_label_rule() {
        assert_eq!(pause_label(Some("work")), "@work");
        assert_eq!(pause_label(Some("")), "cli login");
        assert_eq!(pause_label(None), "all accounts");
    }

    #[test]
    fn pauses_verb_json_lists_every_pause_newest_first_with_the_exact_shape() {
        let store = seeded();
        let (code, out, err) = run(
            &PausesArgs {
                action: PausesAction::List { json: true },
                db: None,
            },
            &store,
        );
        assert_eq!((code, err.as_str()), (0, ""));
        let value: Value = serde_json::from_str(&out).unwrap();
        let entries = value.as_array().unwrap();
        let ids: Vec<i64> = entries.iter().map(|e| e["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [4, 3, 2, 1]);
        let mut keys: Vec<&str> = entries[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "account",
                "active",
                "cleared",
                "id",
                "pause_until",
                "recorded_at",
                "reset_at",
                "run_id"
            ]
        );
        assert_eq!(entries[0]["active"], json!(false));
        assert_eq!(entries[0]["account"], json!("A"));
        assert_eq!(entries[1]["account"], json!(""));
        assert_eq!(entries[1]["active"], json!(true));
        assert_eq!(entries[1]["cleared"], Value::Null);
        assert_eq!(entries[3]["run_id"], json!("run-a"));
        assert_eq!(entries[3]["pause_until"], json!("2026-11-10T17:00:00Z"));
        assert_eq!(entries[3]["recorded_at"], json!("2026-11-10T12:00:00Z"));
    }

    #[test]
    fn pauses_verb_clear_account_clears_only_that_accounts_active_pauses() {
        let store = seeded();
        let reason = "  limit reset early; resuming \u{e9} ";
        let (code, out, err) = run(
            &clear_args(PauseSelector::Account("A".into()), reason),
            &store,
        );
        assert_eq!((code, out.as_str(), err.as_str()), (0, "cleared 1\n", ""));
        let entries = pause_entries(&store, now()).unwrap();
        let by_id = |id: i64| entries.iter().find(|e| e.id == id).unwrap().clone();
        let a = by_id(1);
        assert!(!a.active);
        let clear = a.cleared.unwrap();
        assert_eq!(clear.reason, reason);
        assert_eq!(clear.via, "cli");
        assert_eq!(clear.at, now());
        assert!(by_id(2).active && by_id(3).active);
        assert!(by_id(4).cleared.is_none(), "expired pause is not cleared");
        // The pause row itself is untouched.
        assert_eq!(a.run_id, "run-a");
        assert_eq!(a.pause_until, t0() + Duration::hours(5));
        assert_eq!(a.recorded_at, t0());
    }

    #[test]
    fn pauses_verb_clear_default_ids_and_all_selectors() {
        let store = seeded();
        let (code, out, _) = run(
            &clear_args(PauseSelector::Account(String::new()), "r"),
            &store,
        );
        assert_eq!((code, out.as_str()), (0, "cleared 3\n"));
        let (code, out, _) = run(&clear_args(PauseSelector::Ids(vec![2, 4, 99]), "r"), &store);
        assert_eq!((code, out.as_str()), (0, "cleared 2\n"));
        let (code, out, _) = run(&clear_args(PauseSelector::All, "r"), &store);
        assert_eq!((code, out.as_str()), (0, "cleared 1\n"));
        assert!(active_pauses(&store, now()).unwrap().is_empty());
    }

    #[test]
    fn pauses_verb_all_selects_pre_account_rows_and_account_does_not() {
        let entry = |account: Option<&str>| PauseEntry {
            id: 1,
            account: account.map(str::to_owned),
            run_id: "r".into(),
            reset_at: t0(),
            pause_until: t0(),
            recorded_at: t0(),
            active: true,
            cleared: None,
        };
        let legacy = entry(None);
        assert!(PauseSelector::All.matches(&legacy));
        assert!(!PauseSelector::Account("A".into()).matches(&legacy));
        assert!(!PauseSelector::Account(String::new()).matches(&legacy));
        assert!(PauseSelector::Account(String::new()).matches(&entry(Some(""))));
        assert!(!PauseSelector::Account("A".into()).matches(&entry(Some("B"))));
    }

    #[test]
    fn pauses_verb_no_match_and_empty_reason_record_nothing() {
        let store = seeded();
        let before = store.list_session_pauses().unwrap();
        let (code, out, err) = run(&clear_args(PauseSelector::Account("C".into()), "r"), &store);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.contains("no active session pause matches account @C"),
            "{err}"
        );
        let (code, _, _) = run(&clear_args(PauseSelector::Ids(vec![4]), "r"), &store);
        assert_eq!(code, 1, "an expired pause is not clearable");
        for reason in ["", "   "] {
            let (code, out, err) = run(&clear_args(PauseSelector::All, reason), &store);
            assert_eq!((code, out.as_str()), (2, ""));
            assert!(err.contains("--reason must not be empty"), "{err}");
        }
        assert_eq!(store.list_session_pauses().unwrap(), before);
    }

    #[test]
    fn pauses_verb_active_read_and_hub_attribution() {
        let store = seeded();
        let active: Vec<i64> = active_pauses(&store, now())
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(active, [3, 2, 1]);
        let cleared =
            clear_session_pauses(&store, &PauseSelector::All, now(), PAUSE_CLEAR_VIA_HUB, "h")
                .unwrap();
        assert_eq!(cleared, [1, 2, 3]);
        let entries = pause_entries(&store, now()).unwrap();
        assert!(
            entries
                .iter()
                .filter_map(|e| e.cleared.as_ref())
                .all(|c| c.via == "hub" && c.reason == "h")
        );
    }

    #[test]
    fn pauses_verb_text_listing() {
        let store = seeded();
        run(&clear_args(PauseSelector::Ids(vec![2]), "why"), &store);
        let (code, out, _) = run(
            &PausesArgs {
                action: PausesAction::List { json: false },
                db: None,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "4 @A until 2026-11-10T13:00:00Z (run run-a-old) expired\n\
             3 cli login until 2026-11-10T17:00:00Z (run run-d) active\n\
             2 @B until 2026-11-10T17:00:00Z (run run-b) cleared 2026-11-10T14:00:00Z via cli: why\n\
             1 @A until 2026-11-10T17:00:00Z (run run-a) active\n"
        );
        let empty = InMemoryClaimStore::new();
        let (_, out, _) = run(
            &PausesArgs {
                action: PausesAction::List { json: false },
                db: None,
            },
            &empty,
        );
        assert_eq!(out, "no session pauses\n");
    }
}
