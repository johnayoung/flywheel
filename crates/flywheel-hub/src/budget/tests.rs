use super::pressure::{self, Constraint, Pressure};
use super::*;

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn wanted() -> Vec<(String, String)> {
    pairs(&[
        ("CARGO_MAKEFLAGS", "-j4 --jobserver-auth=fifo:/s/p"),
        ("MAKEFLAGS", "-j4 --jobserver-auth=fifo:/s/p"),
        ("RUST_TEST_THREADS", "8"),
        ("NEXTEST_TEST_THREADS", "8"),
    ])
}

#[test]
fn a_session_gets_every_variable_it_does_not_already_have() {
    assert_eq!(session_env(&wanted(), |_| false, &[]), wanted());
    // The hub's own environment wins, variable by variable.
    let kept = session_env(&wanted(), |k| k == "RUST_TEST_THREADS", &[]);
    assert_eq!(
        kept.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["CARGO_MAKEFLAGS", "MAKEFLAGS", "NEXTEST_TEST_THREADS"]
    );
    // An explicit spawn value wins too.
    let explicit = pairs(&[("NEXTEST_TEST_THREADS", "1")]);
    assert!(
        !session_env(&wanted(), |_| false, &explicit)
            .iter()
            .any(|(k, _)| k == "NEXTEST_TEST_THREADS")
    );
}

#[test]
fn the_jobserver_pair_goes_together() {
    for inherited in ["MAKEFLAGS", "CARGO_MAKEFLAGS"] {
        let got = session_env(&wanted(), |k| k == inherited, &[]);
        assert_eq!(
            got.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["RUST_TEST_THREADS", "NEXTEST_TEST_THREADS"],
            "{inherited} set: neither jobserver variable is added"
        );
    }
    // CARGO_BUILD_JOBS is not the budget's: it neither blocks nor is set.
    assert_eq!(
        session_env(&wanted(), |k| k == "CARGO_BUILD_JOBS", &[]),
        wanted()
    );
}

#[test]
fn defaults_follow_the_machine() {
    assert_eq!(default_jobs(32), 30);
    assert_eq!(default_jobs(2), 1);
    assert_eq!(default_jobs(1), 1);
    assert_eq!(default_test_threads(32), 8);
    assert_eq!(default_test_threads(4), 2);
    assert_eq!(default_test_threads(1), 2);
    let off = BudgetConfig::off();
    assert!(!off.enabled);
    assert!(
        BuildBudget::open(Path::new("/nonexistent"), off, SAMPLE_EVERY)
            .wanted_env()
            .is_empty()
    );
}

#[cfg(target_os = "linux")]
mod pool {
    use super::*;
    use std::io::Read;
    use std::process::{Child, Command};
    use std::time::Duration;

    fn holder_of(fifo: &Path) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec 7<>'{}'; exec sleep 60", fifo.display()))
            .spawn()
            .unwrap()
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_pool_starts_full_and_reads_without_taking() {
        let dir = tempfile::tempdir().unwrap();
        let budget = BuildBudget::open(
            dir.path(),
            BudgetConfig {
                enabled: true,
                jobs: 6,
                test_threads: 3,
            },
            SAMPLE_EVERY,
        );
        let pool = budget.pool().expect("a pool");
        assert_eq!(pool.tokens(), 5);
        assert_eq!(pool.free().unwrap(), 5);
        assert_eq!(pool.free().unwrap(), 5, "reading takes nothing");
        let flags = format!(
            "-j6 --jobserver-auth=fifo:{}",
            std::fs::canonicalize(dir.path().join(FIFO_FILE))
                .unwrap()
                .display()
        );
        assert_eq!(
            budget.wanted_env(),
            vec![
                ("CARGO_MAKEFLAGS".to_owned(), flags.clone()),
                ("MAKEFLAGS".to_owned(), flags),
                ("RUST_TEST_THREADS".to_owned(), "3".to_owned()),
                ("NEXTEST_TEST_THREADS".to_owned(), "3".to_owned()),
            ]
        );
        // Opening it again (a restarted hub) with nobody holding it leaves
        // it full, not doubled.
        drop(budget);
        let again = Pool::open(&dir.path().join(FIFO_FILE), 6).unwrap();
        assert_eq!(again.free().unwrap(), 5);
    }

    #[test]
    fn lost_tokens_come_back_once_nobody_holds_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let budget = BuildBudget::open(
            dir.path(),
            BudgetConfig {
                enabled: true,
                jobs: 5,
                test_threads: 2,
            },
            SAMPLE_EVERY,
        );
        let fifo = budget.pool().unwrap().path().to_owned();
        // A build takes three tokens and dies holding them.
        let mut taker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut three = [0u8; 3];
        taker.read_exact(&mut three).unwrap();
        drop(taker);
        assert_eq!(budget.pool().unwrap().free().unwrap(), 1);
        // While another process holds the FIFO, nothing is touched.
        let mut holder = holder_of(&fifo);
        wait_for("the holder opens the FIFO", || {
            holders(&fifo, std::process::id())
                .is_some_and(|h| h.iter().any(|h| h.pid == holder.id()))
        });
        let (reading, _) = budget.tick();
        let pool = reading.pool.unwrap();
        assert_eq!((pool.free, pool.in_use), (1, 3));
        let sessions = pool.sessions.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].other, 1);
        holder.kill().unwrap();
        holder.wait().unwrap();
        // Then it is refilled to exactly jobs - 1.
        let (reading, _) = budget.tick();
        assert_eq!(reading.pool.unwrap().free, 4);
        assert_eq!(budget.pool().unwrap().free().unwrap(), 4);
    }

    #[test]
    fn extra_tokens_are_drained_too() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::open(&dir.path().join(FIFO_FILE), 3).unwrap();
        let mut extra = std::fs::OpenOptions::new()
            .write(true)
            .open(pool.path())
            .unwrap();
        std::io::Write::write_all(&mut extra, b"++++").unwrap();
        drop(extra);
        assert_eq!(pool.free().unwrap(), 6);
        pool.refill().unwrap();
        assert_eq!(pool.free().unwrap(), 2);
    }

    #[test]
    fn a_stray_file_in_the_fifos_place_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FIFO_FILE), "not a fifo").unwrap();
        let pool = Pool::open(&dir.path().join(FIFO_FILE), 4).unwrap();
        assert_eq!(pool.free().unwrap(), 3);
    }
}

fn holder(pid: u32, comm: &str, session: Option<&str>) -> Holder {
    Holder {
        pid,
        comm: comm.to_owned(),
        session: session.map(str::to_owned),
    }
}

#[cfg(unix)]
#[test]
fn slots_in_use_count_each_cargos_own_slot() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Pool::open(&dir.path().join(FIFO_FILE), 10).unwrap();
    let held = [
        holder(1, "cargo", Some("a")),
        holder(2, "rustc", Some("a")),
        holder(3, "rustc", Some("a")),
        holder(4, "cargo", Some("b")),
        holder(5, "rustc", Some("b")),
        holder(6, "cc", None),
    ];
    // 9 tokens, 6 free: 3 taken, plus two cargos' own slots.
    let reading = read_pool(&pool, 6, Some(&held));
    assert_eq!(
        (
            reading.in_use,
            reading.cargo,
            reading.rustc,
            reading.ceiling
        ),
        (5, 2, 3, 11)
    );
    let sessions = reading.sessions.unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|s| (s.session.as_deref(), s.cargo, s.rustc, s.other))
            .collect::<Vec<_>>(),
        [(Some("a"), 1, 2, 0), (Some("b"), 1, 1, 0), (None, 0, 0, 1)]
    );
    let unknown = read_pool(&pool, 9, None);
    assert_eq!((unknown.in_use, unknown.sessions), (0, None));
}

const STAT_1: &str =
    "cpu  100 0 50 800 20 0 0 0 0 0\ncpu0 1 2 3\nprocs_running 5\nprocs_blocked 1\n";
const STAT_2: &str = "cpu  160 10 70 830 30 5 5 0 0 0\nprocs_running 40\nprocs_blocked 3\n";
const MEMINFO: &str =
    "MemTotal:       65536000 kB\nMemFree:  100 kB\nMemAvailable:    3276800 kB\n";

#[test]
fn readings_count_as_vmstat_does() {
    assert_eq!(pressure::parse_cpu(STAT_1), Some((150, 20, 970)));
    let before = pressure::Counters {
        busy: 150,
        iowait: 20,
        total: 970,
        pswpout: 1000,
    };
    let (busy, iowait, total) = pressure::parse_cpu(STAT_2).unwrap();
    let after = pressure::Counters {
        busy,
        iowait,
        total,
        pswpout: 1600,
    };
    let p = pressure::between(
        before,
        after,
        Duration::from_secs(2),
        STAT_2,
        MEMINFO,
        "12.50 9.00 8.00 3/900 1234\n",
    );
    // 140 jiffies: 100 busy (60 user + 10 nice + 20 sys + 5 irq + 5 soft),
    // 10 iowait, 30 idle.
    assert!((p.cpu_busy_pct - 100.0 * 100.0 / 140.0).abs() < 1e-9);
    assert!((p.iowait_pct - 100.0 * 10.0 / 140.0).abs() < 1e-9);
    assert_eq!((p.blocked, p.running), (3, 40));
    assert_eq!(p.load1, 12.5);
    assert_eq!(p.mem_available_bytes, 3_276_800 * 1024);
    assert_eq!(p.mem_total_bytes, 65_536_000 * 1024);
    assert_eq!(p.swap_out_pages_per_sec, 300.0);
    assert_eq!(p.window_ms, 2000);
}

fn calm() -> Pressure {
    Pressure {
        cpu_busy_pct: 30.0,
        iowait_pct: 2.0,
        blocked: 0,
        running: 4,
        load1: 4.0,
        mem_available_bytes: 40 << 30,
        mem_total_bytes: 64 << 30,
        swap_out_pages_per_sec: 0.0,
        window_ms: 2000,
    }
}

#[test]
fn the_constraint_is_the_most_binding_one() {
    assert_eq!(calm().constraint(32), Constraint::None);
    let cpu = Pressure {
        cpu_busy_pct: 92.0,
        ..calm()
    };
    assert_eq!(cpu.constraint(32), Constraint::Cpu);
    let queued = Pressure {
        running: 70,
        ..calm()
    };
    assert_eq!(queued.constraint(32), Constraint::Cpu);
    let io = Pressure {
        iowait_pct: 11.0,
        ..cpu.clone()
    };
    assert_eq!(io.constraint(32), Constraint::Io);
    let blocked = Pressure {
        iowait_pct: 5.0,
        blocked: 4,
        ..calm()
    };
    assert_eq!(blocked.constraint(32), Constraint::Io);
    let swapping = Pressure {
        swap_out_pages_per_sec: 400.0,
        ..io.clone()
    };
    assert_eq!(swapping.constraint(32), Constraint::Memory);
    let short = Pressure {
        mem_available_bytes: 3 << 30,
        ..calm()
    };
    assert_eq!(short.constraint(32), Constraint::Memory);
}

fn reading(in_use: usize, pressure: Pressure) -> Reading {
    let constraint = pressure.constraint(32);
    Reading {
        enabled: true,
        jobs: 30,
        test_threads: 8,
        threads: 32,
        pool: Some(PoolReading {
            path: PathBuf::from("/s/p"),
            jobs: 30,
            tokens: 29,
            free: 29 - in_use,
            in_use,
            cargo: 0,
            rustc: in_use,
            ceiling: 29,
            sessions: None,
        }),
        pressure: Some(pressure),
        constraint,
    }
}

#[test]
fn samples_are_recorded_at_most_every_period_and_only_when_moved() {
    let mut throttle = Throttle::new(SAMPLE_EVERY);
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    assert!(throttle.admit(&reading(0, calm()), at(0)), "the first");
    // Nothing moved: never again, however long.
    for s in [2, 30, 60, 600] {
        assert!(!throttle.admit(&reading(0, calm()), at(s)));
    }
    // Gauge jitter inside a step is not a move.
    let jitter = Pressure {
        cpu_busy_pct: 33.0,
        iowait_pct: 2.4,
        ..calm()
    };
    assert!(!throttle.admit(&reading(0, jitter), at(700)));
    // A move: not before the period is up since the last recorded.
    let busy = || reading(12, calm());
    assert!(throttle.admit(&busy(), at(700)));
    assert!(!throttle.admit(&reading(20, calm()), at(710)));
    assert!(!throttle.admit(&reading(20, calm()), at(729)));
    assert!(throttle.admit(&reading(20, calm()), at(730)));
    // The body leaves the per-session breakdown at home.
    let mut with_sessions = busy();
    with_sessions.pool.as_mut().unwrap().sessions = Some(Vec::new());
    let body = sample_body(&with_sessions);
    assert!(body["pool"].get("sessions").is_none(), "{body}");
    assert_eq!(body["pool"]["in_use"], 12);
    assert_eq!(body["constraint"], "none");
}
