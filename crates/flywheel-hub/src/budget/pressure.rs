//! How loaded the machine is, from `/proc` (Linux; elsewhere it reads as
//! unavailable): CPU busy and I/O wait over a window (`/proc/stat`, as
//! `vmstat` counts them), processes blocked on I/O and running now,
//! memory available (`/proc/meminfo`), swap-out rate (`/proc/vmstat`),
//! and the load average (`/proc/loadavg`). [`Pressure::constraint`] names
//! what bounds the machine.

use serde::Serialize;
use std::time::{Duration, Instant};

/// What bounds the machine now, most binding first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    /// Little memory available, or pages swapped out.
    Memory,
    /// Time waiting on disk.
    Io,
    /// The CPUs are busy, or more runs than they can.
    Cpu,
    #[default]
    None,
}

impl Constraint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Io => "io",
            Self::Cpu => "cpu",
            Self::None => "none",
        }
    }
}

/// CPU busy at or over this (percent) is CPU-bound.
pub const CPU_BUSY_PCT: f64 = 85.0;
/// I/O wait at or over this (percent) is I/O-bound; so is half of it
/// with [`blocked_floor`] processes blocked on I/O.
pub const IOWAIT_PCT: f64 = 8.0;
/// Memory available under this share of the total is memory-bound.
pub const MEM_AVAILABLE_SHARE: f64 = 0.10;
/// Pages swapped out per second at or over this (1 MiB/s of 4 KiB pages)
/// is memory-bound.
pub const SWAP_OUT_PAGES_PER_SEC: f64 = 256.0;

/// Blocked processes that, with some I/O wait, make a machine of
/// `threads` I/O-bound.
pub fn blocked_floor(threads: usize) -> u32 {
    u32::try_from((threads / 8).max(2)).unwrap_or(u32::MAX)
}

/// One reading over a window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pressure {
    /// User, nice, system, irq, and softirq time (`vmstat`'s us + sy).
    pub cpu_busy_pct: f64,
    /// `vmstat`'s wa.
    pub iowait_pct: f64,
    /// Processes blocked on I/O now (`vmstat`'s b).
    pub blocked: u32,
    /// Processes runnable now (`vmstat`'s r).
    pub running: u32,
    pub load1: f64,
    pub mem_available_bytes: u64,
    pub mem_total_bytes: u64,
    pub swap_out_pages_per_sec: f64,
    /// The window the rates cover.
    pub window_ms: u64,
}

impl Pressure {
    /// What bounds a machine of `threads` hardware threads: memory, then
    /// I/O, then CPU, else none.
    pub fn constraint(&self, threads: usize) -> Constraint {
        let total = self.mem_total_bytes as f64;
        if (total > 0.0 && (self.mem_available_bytes as f64) < total * MEM_AVAILABLE_SHARE)
            || self.swap_out_pages_per_sec >= SWAP_OUT_PAGES_PER_SEC
        {
            return Constraint::Memory;
        }
        if self.iowait_pct >= IOWAIT_PCT
            || (self.iowait_pct >= IOWAIT_PCT / 2.0 && self.blocked >= blocked_floor(threads))
        {
            return Constraint::Io;
        }
        let threads = u32::try_from(threads).unwrap_or(u32::MAX);
        if self.cpu_busy_pct >= CPU_BUSY_PCT || self.running > threads.saturating_mul(2) {
            return Constraint::Cpu;
        }
        Constraint::None
    }
}

/// The cumulative counters a window is the difference of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub busy: u64,
    pub iowait: u64,
    pub total: u64,
    pub pswpout: u64,
}

/// `/proc/stat`'s aggregate `cpu` line: busy, iowait, and total jiffies.
pub fn parse_cpu(stat: &str) -> Option<(u64, u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|f| f.parse().unwrap_or(0))
        .collect();
    let at = |i: usize| fields.get(i).copied().unwrap_or(0);
    // user nice system idle iowait irq softirq steal (guest is in user).
    let busy = at(0) + at(1) + at(2) + at(5) + at(6);
    let total = (0..8).map(at).sum();
    Some((busy, at(4), total))
}

/// A `name value` line's value (`/proc/stat`, `/proc/vmstat`).
pub fn field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        (parts.next()? == name).then(|| parts.next()?.parse().ok())?
    })
}

/// `/proc/meminfo`'s `name:` in bytes (it counts kB).
pub fn meminfo(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(name)?.strip_prefix(':')?;
        rest.split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    })
}

/// `/proc/loadavg`'s one-minute load.
pub fn load1(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn counters(stat: &str, vmstat: &str) -> Option<Counters> {
    let (busy, iowait, total) = parse_cpu(stat)?;
    Some(Counters {
        busy,
        iowait,
        total,
        pswpout: field(vmstat, "pswpout").unwrap_or(0),
    })
}

/// A reading from two counter sets `elapsed` apart and the gauges now.
pub fn between(
    before: Counters,
    after: Counters,
    elapsed: Duration,
    stat: &str,
    meminfo_text: &str,
    loadavg: &str,
) -> Pressure {
    let total = after.total.saturating_sub(before.total).max(1) as f64;
    let pct = |a: u64, b: u64| (a.saturating_sub(b) as f64 / total * 100.0).clamp(0.0, 100.0);
    let secs = elapsed.as_secs_f64().max(0.001);
    let gauge = |name| field(stat, name).map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    Pressure {
        cpu_busy_pct: pct(after.busy, before.busy),
        iowait_pct: pct(after.iowait, before.iowait),
        blocked: gauge("procs_blocked"),
        running: gauge("procs_running"),
        load1: load1(loadavg).unwrap_or(0.0),
        mem_available_bytes: meminfo(meminfo_text, "MemAvailable").unwrap_or(0),
        mem_total_bytes: meminfo(meminfo_text, "MemTotal").unwrap_or(0),
        swap_out_pages_per_sec: after.pswpout.saturating_sub(before.pswpout) as f64 / secs,
        window_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    }
}

/// Readings each over the time since the one before.
#[derive(Debug, Default)]
pub struct Sampler {
    last: Option<(Instant, Counters)>,
}

impl Sampler {
    /// The reading since the last call; `None` the first time (and where
    /// `/proc` cannot be read).
    pub fn sample(&mut self) -> Option<Pressure> {
        let stat = read("/proc/stat")?;
        let now = Instant::now();
        let after = counters(&stat, &read("/proc/vmstat").unwrap_or_default())?;
        let before = self.last.replace((now, after));
        let (at, before) = before?;
        Some(between(
            before,
            after,
            now.duration_since(at),
            &stat,
            &read("/proc/meminfo").unwrap_or_default(),
            &read("/proc/loadavg").unwrap_or_default(),
        ))
    }
}

/// One reading over `window` (blocks for it).
pub fn measure(window: Duration) -> Option<Pressure> {
    let mut sampler = Sampler::default();
    sampler.sample();
    std::thread::sleep(window);
    sampler.sample()
}
