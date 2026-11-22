//! The hub's side of the build budget (see [`crate::budget`]): the ticker
//! that keeps the pool whole and records samples, and the reading `GET
//! /api/budget` and `fw hub budget` print.

use super::Hub;
use crate::budget::{Reading, SAMPLE_EVENT, TICK};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

/// Other hubs' samples older than this are left out of the fleet view.
const FLEET_SINCE: Duration = Duration::from_secs(15 * 60);
/// The window a reading measures over when the ticker is off.
pub const READ_WINDOW: Duration = Duration::from_secs(1);

/// Another hub's last sample (a collector's view of its fleet).
#[derive(Debug, Clone, Serialize)]
pub struct HostSample {
    pub host: String,
    /// When it was taken (unix ms).
    pub ts: i64,
    pub sample: serde_json::Value,
}

/// `GET /api/budget`.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetView {
    /// This hub's id.
    pub host: String,
    #[serde(flatten)]
    pub reading: Reading,
    /// Every other hub that reported a sample here lately.
    pub fleet: Vec<HostSample>,
}

impl Hub {
    /// What a session spawned with `explicit` variables adds for the
    /// budget: nothing for one started with it off.
    pub(crate) fn budget_env(
        &self,
        record: &crate::state::SessionRecord,
        explicit: &[(String, String)],
    ) -> Vec<(String, String)> {
        if record.no_build_budget {
            return Vec::new();
        }
        self.budget.env_for(explicit)
    }

    /// Look at the pool and the machine every [`TICK`] (its own thread),
    /// with the budget on.
    pub(crate) fn start_budget(self: &Arc<Self>) {
        if !self.budget.ticks() {
            return;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-budget".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    let (reading, due) = hub.budget.tick();
                    if due {
                        hub.log_hub_event(SAMPLE_EVENT, None, crate::budget::sample_body(&reading));
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!(
                "fw hub: build budget: its pool is not watched: cannot start its thread: {e}"
            );
        }
    }

    /// The budget now, its sessions named, and the other hubs' samples.
    pub fn budget_view(&self) -> BudgetView {
        let mut reading = self.budget.read(READ_WINDOW);
        if let Some(sessions) = reading.pool.as_mut().and_then(|p| p.sessions.as_mut()) {
            let store = self.store();
            for usage in sessions.iter_mut() {
                usage.name = usage
                    .session
                    .as_deref()
                    .and_then(|id| store.session(id).ok())
                    .map(|record| record.name.clone());
            }
        }
        let host = self.telemetry.host().to_owned();
        let since =
            crate::telemetry::now_ms() - i64::try_from(FLEET_SINCE.as_millis()).unwrap_or(i64::MAX);
        let fleet = self
            .telemetry
            .latest_per_host(SAMPLE_EVENT, since)
            .unwrap_or_default()
            .into_iter()
            .filter(|(from, _, _)| *from != host)
            .map(|(host, ts, sample)| HostSample { host, ts, sample })
            .collect();
        BudgetView {
            host,
            reading,
            fleet,
        }
    }
}
