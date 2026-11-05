//! The running hub's digests: the scheduler task, sending by hand, the
//! preview, and the view the API and Settings show.

use super::store::{Store, pid_alive};
use super::{
    Claimant, Delivery, DigestConfig, Due, GRACE, Inputs, Live, LiveBlocked, LiveGate, LivePr,
    LiveStall, Report, build, decide, finish, rows_from, schedule,
};
use crate::hub::{Hub, HubEvent};
use crate::status::AgentStatus;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// The longest the scheduler sleeps before looking again (a clock that
/// jumped, a machine that slept).
const POLL: std::time::Duration = std::time::Duration::from_secs(30);

type ConfigFn = Box<dyn Fn() -> DigestConfig + Send + Sync>;
type AliveFn = Box<dyn Fn(u32) -> bool + Send + Sync>;

/// Digests for one hub.
pub struct DigestService {
    store: Store,
    config: ConfigFn,
    notifier: Option<Arc<crate::notify::Notifier>>,
    flywheel: Option<Arc<crate::flywheel::FlywheelMonitor>>,
    /// The hub's URL a phone reaches (links), if it has one.
    hub_url: Option<String>,
    me: Claimant,
    alive: AliveFn,
    /// One send at a time from this hub.
    sending: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for DigestService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestService")
            .field("store", &self.store)
            .field("hub_url", &self.hub_url)
            .finish_non_exhaustive()
    }
}

impl DigestService {
    /// Digests for the hub at `state_dir`, configured by `config` (read on
    /// every look: `hub.toml` reloads apply at once).
    pub fn new(
        state_dir: &std::path::Path,
        config: impl Fn() -> DigestConfig + Send + Sync + 'static,
        notifier: Option<Arc<crate::notify::Notifier>>,
        flywheel: Option<Arc<crate::flywheel::FlywheelMonitor>>,
        hub_url: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Store::new(state_dir),
            config: Box::new(config),
            notifier,
            flywheel,
            hub_url,
            me: Claimant {
                pid: std::process::id(),
                owner: crate::state::new_id(),
            },
            alive: Box::new(pid_alive),
            sending: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> DigestConfig {
        (self.config)()
    }

    /// Run the scheduler on the current tokio runtime until `hub` is gone.
    pub fn start(self: &Arc<Self>, hub: &Arc<Hub>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(hub);
        let me = Arc::clone(self);
        let mut events = hub.subscribe();
        tokio::spawn(async move {
            loop {
                let Some(hub) = weak.upgrade() else { return };
                let now = Utc::now();
                me.tick(&hub, now).await;
                drop(hub);
                let wait = me.wait_from(Utc::now());
                let sleep = tokio::time::sleep(wait);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        () = &mut sleep => break,
                        event = events.recv() => match event {
                            // hub.toml reloaded: look again now.
                            Ok(HubEvent::Config { .. }) | Err(RecvError::Lagged(_)) => break,
                            Ok(_) => {}
                            Err(RecvError::Closed) => return,
                        },
                    }
                }
            }
        })
    }

    /// How long until the next look: the next slot (a moment after it),
    /// at most [`POLL`].
    fn wait_from(&self, now: DateTime<Utc>) -> std::time::Duration {
        let config = self.config();
        let next = schedule::next_after(&config.timezone, &config.times, now);
        let until = next
            .map(|at| {
                (at - now + Duration::milliseconds(200))
                    .to_std()
                    .unwrap_or_default()
            })
            .unwrap_or(POLL);
        until.clamp(std::time::Duration::from_millis(200), POLL)
    }

    /// Decide what is due at `now` and send it.
    pub async fn tick(&self, hub: &Hub, now: DateTime<Utc>) -> Due {
        let config = self.config();
        let alive = &self.alive;
        let me = &self.me;
        let due = match self
            .store
            .update(|marker| decide(marker, &config, now, me, &|pid| alive(pid)))
        {
            Ok(due) => due,
            Err(e) => {
                eprintln!("fw hub: digest marker: {e}");
                return Due::Idle;
            }
        };
        match &due {
            Due::Skipped(slot) => eprintln!(
                "fw hub: the digest due {} was not sent: the hub was not running then (more than {}h ago)",
                slot.with_timezone(&config.timezone)
                    .format("%Y-%m-%d %H:%M %Z"),
                GRACE.num_hours()
            ),
            Due::Send(pending) => {
                let label = pending
                    .slot
                    .map_or("manual", |slot| schedule::label_of(&config.timezone, slot));
                let _guard = self.sending.lock().await;
                let report = self.make(
                    hub,
                    &config,
                    pending.id.clone(),
                    "scheduled",
                    label,
                    pending.slot,
                    pending.since,
                    pending.until,
                );
                self.deliver(hub, &config, report, true).await;
            }
            Due::Off | Due::Idle => {}
        }
        due
    }

    /// The digest of `since` (default: since the last one) until now,
    /// neither sent nor kept.
    pub fn preview(&self, hub: &Hub, since: Option<DateTime<Utc>>) -> Report {
        let config = self.config();
        let now = Utc::now();
        let since = since.unwrap_or_else(|| self.default_since(now));
        self.make(
            hub,
            &config,
            "preview".to_owned(),
            "preview",
            "preview",
            None,
            since,
            now,
        )
    }

    fn default_since(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.store
            .read()
            .last_until
            .unwrap_or(now - Duration::days(1))
            .max(now - super::MAX_SPAN)
    }

    /// Send a digest now over the configured channels (even an empty one),
    /// from `since` (default: since the last one); the next scheduled one
    /// starts where it ends.
    pub async fn send_now(
        &self,
        hub: &Hub,
        since: Option<DateTime<Utc>>,
    ) -> Result<Report, String> {
        let config = self.config();
        let _guard = self.sending.lock().await;
        let now = Utc::now();
        let default_since = self.default_since(now);
        let since = since.unwrap_or(default_since).min(now);
        self.store.update(|marker| {
            marker.last_until = Some(now);
            ((), true)
        })?;
        let id = format!("dg-m{}", now.timestamp_millis());
        let report = self.make(hub, &config, id, "manual", "manual", None, since, now);
        Ok(self.deliver(hub, &config, report, false).await)
    }

    /// A kept digest.
    pub fn get(&self, id: &str) -> Option<Report> {
        self.store.read().recent.into_iter().find(|r| r.id == id)
    }

    /// `GET /api/digest`: the schedule in force, the next slots, the last
    /// digest, and the recent ones.
    pub fn view(&self) -> Value {
        let config = self.config();
        let marker = self.store.read();
        let now = Utc::now();
        let recent: Vec<Value> = marker
            .recent
            .iter()
            .rev()
            .map(|r| {
                json!({
                    "id": r.id,
                    "trigger": r.trigger,
                    "label": r.label,
                    "since": r.since,
                    "until": r.until,
                    "empty": r.empty,
                    "quiet": r.delivery.as_ref().is_some_and(|d| d.quiet),
                    "channels": r.delivery.as_ref().map(|d| d.channels.clone()).unwrap_or_default(),
                    "push_error": r.delivery.as_ref().and_then(|d| d.push_error.clone()),
                })
            })
            .collect();
        json!({
            "enabled": config.enabled(),
            "times": config.times_text(),
            "timezone": config.timezone.name(),
            "timezone_source": config.zone_source,
            "channels": config.channels.names(),
            "quiet_if_empty": config.quiet_if_empty,
            "grace_secs": GRACE.num_seconds(),
            "next": if config.enabled() { config.upcoming(now, 2) } else { Vec::new() },
            "last_slot": marker.last_slot,
            "last_until": marker.last_until,
            "push": self.notifier.as_ref().map(|n| n.config().host()),
            "recent": recent,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn make(
        &self,
        hub: &Hub,
        config: &DigestConfig,
        id: String,
        trigger: &str,
        label: &str,
        slot: Option<DateTime<Utc>>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Report {
        let tz = config.timezone;
        let filter = crate::activity::Filter {
            since: Some(rows_from(&tz, since, until)),
            until: Some(until),
            ..crate::activity::Filter::default()
        };
        let rows = hub.activity().rows(&filter);
        let live = self.live(hub);
        build(&Inputs {
            id,
            trigger,
            label: label.to_owned(),
            slot,
            since,
            until,
            tz,
            rows: &rows,
            live: &live,
            hub_url: self.hub_url.as_deref(),
        })
    }

    /// What waits on a person now.
    fn live(&self, hub: &Hub) -> Live {
        let workspaces = hub.workspaces();
        let mut live = Live {
            workspaces: workspaces
                .iter()
                .map(|w| (w.id.clone(), w.name.clone()))
                .collect(),
            folders: workspaces
                .iter()
                .flat_map(|w| {
                    w.folders
                        .iter()
                        .map(|f| (crate::autopilot::folder_name(&f.path), w.id.clone()))
                })
                .collect(),
            ..Live::default()
        };
        for view in hub.sessions(None) {
            let record = &view.record;
            if let Some(agent) = &view.agent
                && agent.status == AgentStatus::Blocked
            {
                live.blocked.push(LiveBlocked {
                    session_id: record.id.clone(),
                    session: record.name.clone(),
                    workspace_id: record.workspace_id.clone(),
                    since: agent.since,
                });
            }
            if let Some(stall) = view.queue.as_ref().and_then(|q| q.stalled.as_ref()) {
                live.stalled.push(LiveStall {
                    session_id: record.id.clone(),
                    session: record.name.clone(),
                    workspace_id: record.workspace_id.clone(),
                    at: stall.at,
                });
            }
            if let Some(pr) = &view.pr
                && matches!(pr.state.as_str(), "open" | "draft")
                && pr.checks.rollup == crate::pr::Rollup::Failing
            {
                live.red_prs.push(LivePr {
                    session_id: record.id.clone(),
                    session: record.name.clone(),
                    workspace_id: record.workspace_id.clone(),
                    number: pr.number,
                });
            }
        }
        if let Some(monitor) = &self.flywheel {
            let mut seen = HashSet::new();
            for workspace in &workspaces {
                for folder in &workspace.folders {
                    let Some(panel) = monitor.panel(&folder.path) else {
                        continue;
                    };
                    let Some(intake) = &panel.intake else {
                        continue;
                    };
                    for gate in intake.gates.iter().filter(|g| g.pending.is_none()) {
                        if !seen.insert(gate.run_id.clone()) {
                            continue;
                        }
                        live.gates.push(LiveGate {
                            workspace_id: Some(workspace.id.clone()),
                            folder: crate::autopilot::folder_name(&folder.path),
                            task_id: gate.task_id.clone(),
                            run_id: gate.run_id.clone(),
                            since: gate.since,
                        });
                    }
                }
            }
        }
        live
    }

    /// Deliver `report` over `config`'s channels (`scheduled`: a digest
    /// with nothing in it is kept quiet when `quiet_if_empty`), keep it,
    /// and release its claim.
    async fn deliver(
        &self,
        hub: &Hub,
        config: &DigestConfig,
        mut report: Report,
        scheduled: bool,
    ) -> Report {
        let mut delivery = Delivery::default();
        if scheduled && report.empty && config.quiet_if_empty {
            delivery.quiet = true;
        } else {
            if config.channels.push
                && let Some(notifier) = &self.notifier
            {
                match notifier.send(&report.push()).await {
                    Ok(()) => delivery.channels.push("push".to_owned()),
                    Err(e) => {
                        eprintln!("fw hub: digest push failed: {e}");
                        delivery.push_error = Some(e);
                    }
                }
            }
            if config.channels.browser {
                delivery.channels.push("browser".to_owned());
            }
            if config.channels.plugin {
                delivery.channels.push("plugin".to_owned());
            }
        }
        report.delivery = Some(delivery);
        // Kept before it is announced: a browser told reads it at once.
        if let Err(e) = self.store.update(|marker| {
            finish(marker, &report);
            ((), true)
        }) {
            eprintln!("fw hub: digest marker: {e}");
        }
        let channels = &report
            .delivery
            .as_ref()
            .map(|d| d.channels.clone())
            .unwrap_or_default();
        if !report.delivery.as_ref().is_some_and(|d| d.quiet) {
            let row =
                crate::activity::digest_row(&serde_json::to_value(&report).unwrap_or(Value::Null));
            if let Some(row) = hub.activity().record(row) {
                hub.emit(HubEvent::Activity {
                    row: serde_json::to_value(&row).unwrap_or(Value::Null),
                });
            }
        }
        if channels.iter().any(|c| c == "browser") {
            hub.emit(HubEvent::Digest {
                id: report.id.clone(),
            });
        }
        if channels.iter().any(|c| c == "plugin") {
            hub.emit_lifecycle(
                "digest.ready",
                json!({ "digest": serde_json::to_value(&report).unwrap_or(Value::Null) }),
            );
        }
        report
    }
}
