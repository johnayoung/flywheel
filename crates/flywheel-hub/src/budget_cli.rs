//! `fw hub budget`: the build budget (see [`crate::budget`]) -- the
//! compile pool's slots in use and who holds them, the machine's pressure
//! and what bounds it, and the other hubs' last samples.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::util::{fail, print_json, str_field};
use serde_json::Value;
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct BudgetArgs {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn budget(args: &BudgetArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let run = || -> Result<Value, String> {
        Endpoint::resolve(&args.endpoint)?.blocking_call("GET", "/api/budget", None)
    };
    match run() {
        Ok(view) if args.json => {
            print_json(out, &view);
            0
        }
        Ok(view) => {
            print_budget(out, &view);
            0
        }
        Err(message) => fail(err, &message),
    }
}

fn num(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// `cpu 85% busy, io wait 3%, 2 blocked, 12.3 GiB of 62.8 GiB free, swap-out
/// 0 pages/s`; `unavailable` without a reading.
pub(crate) fn pressure_line(pressure: &Value) -> String {
    if !pressure.is_object() {
        return "unavailable".to_owned();
    }
    let pct = |key: &str| pressure[key].as_f64().unwrap_or(0.0);
    format!(
        "cpu {:.0}% busy, io wait {:.0}%, {} blocked on io, load {:.1}, {} of {} available, swap-out {:.0} pages/s",
        pct("cpu_busy_pct"),
        pct("iowait_pct"),
        num(pressure, "blocked"),
        pct("load1"),
        gib(num(pressure, "mem_available_bytes")),
        gib(num(pressure, "mem_total_bytes")),
        pct("swap_out_pages_per_sec"),
    )
}

pub(crate) fn print_budget(out: &mut dyn Write, view: &Value) {
    let pool = &view["pool"];
    if view["enabled"] != true {
        let _ = writeln!(
            out,
            "build budget off (hub.toml [build_budget] enabled = false): sessions start without a pool"
        );
    } else if pool.is_object() {
        let _ = writeln!(
            out,
            "build slots {}/{} in use: {} of {} tokens taken, {} building cargo (each holds one slot of its own; ceiling now {})",
            num(pool, "in_use"),
            num(pool, "jobs"),
            num(pool, "tokens").saturating_sub(num(pool, "free")),
            num(pool, "tokens"),
            num(pool, "cargo"),
            num(pool, "ceiling"),
        );
        let _ = writeln!(
            out,
            "  {} rustc running; test threads {} per session; pool {}",
            num(pool, "rustc"),
            num(view, "test_threads"),
            str_field(pool, "path"),
        );
        match pool["sessions"].as_array() {
            None => {
                let _ = writeln!(out, "  holders unknown on this system");
            }
            Some(sessions) => {
                for usage in sessions {
                    let who = match (usage["name"].as_str(), usage["session"].as_str()) {
                        (Some(name), Some(id)) => format!("{name} ({id})"),
                        (None, Some(id)) => id.to_owned(),
                        _ => "outside any session".to_owned(),
                    };
                    let _ = writeln!(
                        out,
                        "  {who}: {} cargo, {} rustc, {} other",
                        num(usage, "cargo"),
                        num(usage, "rustc"),
                        num(usage, "other"),
                    );
                }
            }
        }
    } else {
        let _ = writeln!(
            out,
            "build budget on, but its pool could not be made (see the hub's log)"
        );
    }
    let _ = writeln!(
        out,
        "machine ({} threads): {}; bound by {}",
        num(view, "threads"),
        pressure_line(&view["pressure"]),
        str_field(view, "constraint"),
    );
    let fleet = view["fleet"].as_array().map_or(&[][..], Vec::as_slice);
    if !fleet.is_empty() {
        let _ = writeln!(out, "other hubs (last sample):");
        let now = chrono::Utc::now().timestamp_millis();
        for host in fleet {
            let sample = &host["sample"];
            let slots = if sample["pool"].is_object() {
                format!(
                    "slots {}/{}",
                    num(&sample["pool"], "in_use"),
                    num(&sample["pool"], "jobs")
                )
            } else {
                "no pool".to_owned()
            };
            let age = (now - host["ts"].as_i64().unwrap_or(now)).max(0) / 1000;
            let _ = writeln!(
                out,
                "  {}  {slots}, bound by {}; {} ({age}s ago)",
                str_field(host, "host"),
                str_field(sample, "constraint"),
                pressure_line(&sample["pressure"]),
            );
        }
    }
}
