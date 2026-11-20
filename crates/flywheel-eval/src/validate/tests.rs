use super::*;
use crate::set::{Provenance, SetTask, Source, Split, TaskKind, Tier, write_json};
use crate::testutil::TempRepo;
use chrono::TimeZone;
use std::collections::BTreeMap;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

struct Fixture {
    repo: TempRepo,
    evals: tempfile::TempDir,
    work: tempfile::TempDir,
    set: SetDir,
    manifest: SetManifest,
}

impl Fixture {
    fn new() -> Self {
        let repo = TempRepo::new();
        let evals = tempfile::tempdir().unwrap();
        let set = SetDir::draft(evals.path(), "core");
        let manifest = SetManifest {
            name: "core".into(),
            version: None,
            project_id: "p1".into(),
            repo: repo.path().to_owned(),
            created_at: now(),
            tasks: Vec::new(),
            hash: None,
        };
        Self {
            repo,
            evals,
            work: tempfile::tempdir().unwrap(),
            set,
            manifest,
        }
    }

    fn add(&mut self, id: &str, source: Source, base: &str, reference: &str, oracle: &str) {
        write_json(
            &self.set.task_file(id),
            &serde_json::json!({"id": id, "goal": "make a.txt 2"}),
        )
        .unwrap();
        let home = self.set.oracle_dir(id).join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("o.sh"), oracle).unwrap();
        std::fs::write(
            self.set.registration(id),
            format!(r#"{{"graders": [{{"type": "command", "name": "o", "run": "sh {PROJECT_HOME_TOKEN}/o.sh"}}]}}"#),
        )
        .unwrap();
        self.manifest.tasks.push(SetTask {
            id: id.into(),
            source,
            tier: Tier::Uncalibrated,
            split: Split::Dev,
            kind: TaskKind::Build,
            base: base.into(),
            reference: reference.into(),
            files_digest: String::new(),
            parallel_safe: None,
            prior_cost_usd: None,
            oracle_secs: None,
            prerequisites: BTreeMap::new(),
            provenance: Provenance::default(),
        });
        self.set.save(&self.manifest).unwrap();
    }

    fn options(&self, noise: Option<Vec<String>>) -> ValidateOptions {
        ValidateOptions {
            work: self.work.path().join("work"),
            target_dir: self.work.path().join("target"),
            timeout: Duration::from_secs(30),
            noise,
            search_head: "main".into(),
        }
    }

    fn validate(&mut self, id: &str, noise: Option<Vec<String>>) -> Proof {
        let options = self.options(noise);
        validate_task(&self.set, &mut self.manifest, id, &options, now()).unwrap()
    }
}

fn get<'a>(proof: &'a Proof, name: &str) -> &'a ProofCheck {
    proof
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no {name}: {proof:?}"))
}

const GOOD: &str = "test \"$(cat a.txt)\" = 2\n";

#[test]
fn a_discriminating_oracle_passes_every_check() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    let reference = f.repo.commit("ref", &[("a.txt", "2")]);
    f.repo.commit("later", &[("a.txt", "2"), ("b", "x")]);
    f.add("good", Source::Landed, &base, &reference, GOOD);
    let proof = f.validate("good", Some(vec!["true".into()]));
    assert!(proof.passed, "{proof:#?}");
    assert!(proof.parallel_safe);
    for name in ["V1", "V2", "V3", "V4", "V5", "V6", "V7"] {
        assert!(get(&proof, name).passed, "{name}: {proof:#?}");
    }
    // Saved as returned (a float may come back one ulp off through JSON).
    let mut saved = f.set.proof("good").unwrap().unwrap();
    assert!((saved.oracle_secs - proof.oracle_secs).abs() < 1e-9);
    saved.oracle_secs = proof.oracle_secs;
    assert_eq!(saved, proof);
    assert_eq!(proof.files_digest, files_digest(&f.set, "good").unwrap());
}

#[test]
fn vacuous_and_broken_oracles_fail_v1_and_v2() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    let reference = f.repo.commit("ref", &[("a.txt", "2")]);
    f.add("vacuous", Source::Landed, &base, &reference, "true\n");
    f.add("broken", Source::Landed, &base, &reference, "false\n");
    let vacuous = f.validate("vacuous", None);
    assert!(!vacuous.passed && !get(&vacuous, "V1").passed);
    let broken = f.validate("broken", None);
    assert!(!broken.passed && !get(&broken, "V2").passed);
}

#[test]
fn an_oracle_that_fails_beside_fw_processes_is_kept_but_graded_alone() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    let reference = f.repo.commit("ref", &[("a.txt", "2")]);
    let marker = f.work.path().join("fw-ran");
    let oracle = format!(
        "rm -f {m}; sleep 1; test ! -e {m} && {GOOD}",
        m = marker.display()
    );
    f.add("touchy", Source::Landed, &base, &reference, &oracle);
    let noise = vec!["touch".to_owned(), marker.display().to_string()];
    let proof = f.validate("touchy", Some(noise));
    assert!(proof.passed, "{proof:#?}");
    assert!(!proof.parallel_safe);
    assert!(get(&proof, "V5").detail.contains("graded alone"));
}

#[test]
fn a_base_missing_a_prerequisite_moves_to_the_references_parent() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    let prereq = f.repo.commit("prereq lands", &[("p", "1")]);
    let reference = f.repo.commit("ref", &[("a.txt", "2")]);
    f.add("dependent", Source::Landed, &base, &reference, GOOD);
    f.manifest.tasks[0]
        .prerequisites
        .insert("p-task".into(), prereq.clone());
    f.set.save(&f.manifest).unwrap();
    let proof = f.validate("dependent", None);
    assert!(proof.passed, "{proof:#?}");
    assert_eq!(proof.base, prereq);
    assert_eq!(f.manifest.tasks[0].base, prereq);
    assert!(get(&proof, "V3").detail.contains("p-task"));

    // A prerequisite that never landed cannot be satisfied.
    f.manifest.tasks[0]
        .prerequisites
        .insert("ghost".into(), String::new());
    f.set.save(&f.manifest).unwrap();
    let proof = f.validate("dependent", None);
    assert!(!proof.passed && !get(&proof, "V3").passed);
}

#[test]
fn a_hard_failed_tasks_reference_is_bisected() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    f.repo.commit("noise", &[("x", "1")]);
    f.repo.commit("noise", &[("x", "2")]);
    let fix = f.repo.commit("the fix", &[("a.txt", "2")]);
    f.repo.commit("after", &[("x", "3")]);
    f.add("failed", Source::HardFailed, &base, "", GOOD);
    let proof = f.validate("failed", None);
    assert!(proof.passed, "{proof:#?}");
    assert_eq!(proof.reference, fix);
    assert_eq!(get(&proof, "ref").detail, format!("bisected to {fix}"));
}

#[test]
fn live_home_paths_fail_v6_and_v7() {
    let mut f = Fixture::new();
    let base = f.repo.commit("base", &[("a.txt", "1")]);
    let reference = f.repo.commit("ref", &[("a.txt", "2")]);
    f.add(
        "leaky",
        Source::Landed,
        &base,
        &reference,
        &format!("cat /home/x/.flywheel/projects/p/specs/s.md >/dev/null; {GOOD}"),
    );
    write_json(
        &f.set.task_file("leaky"),
        &serde_json::json!({"id": "leaky", "goal": "g", "prerequisites": ["a"]}),
    )
    .unwrap();
    let proof = f.validate("leaky", None);
    assert!(!proof.passed);
    assert!(!get(&proof, "V6").passed);
    assert!(!get(&proof, "V7").passed);
    let _ = &f.evals;
}
