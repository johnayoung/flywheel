//! Eval sets: the frozen, versioned list of past tasks a replay runs.
//!
//! A set is built as a **draft** (`evals/sets/<name>/draft/`), validated
//! task by task ([`crate::validate`] writes one [`Proof`] each), then
//! **frozen** into `v<N>/`: the draft is copied, its hash computed over
//! every task's files, its git commits pinned under
//! `refs/flywheel/eval/<name>/v<N>/<task>/{base,ref}` (so gc keeps them and
//! no operator branch is touched), its files made read-only, and the
//! version indexed in the store. A frozen version is never edited; a fix is
//! a new version.
//!
//! ```text
//! evals/sets/<name>/<draft|vN>/
//!   set.json            the manifest ([`SetManifest`])
//!   tasks/<task>.json   the Task (prerequisites stripped, paths rewritten)
//!   oracles/<task>/     registration.json + the oracle files it runs
//!   specs/<task>/       context docs copied from the project home
//!   proofs/<task>.json  V1-V7 receipts ([`Proof`])
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git;
use crate::store::{EvalStore, EvalStoreError, SetRow};

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Landed with a held-out oracle on its first try.
    Landed,
    /// Its first landing was blocked by the held-out gate; it landed later.
    HardParked,
    /// It needed more than one attempt (earlier ones failed validation).
    HardRetried,
    /// Its run failed; the reference is the first later commit the oracle
    /// passes on.
    HardFailed,
    /// A landed task replayed at its reference: the right move is no change.
    DontAct,
    /// A recorded hub session turned into a task.
    Session,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landed => "landed",
            Self::HardParked => "hard-parked",
            Self::HardRetried => "hard-retried",
            Self::HardFailed => "hard-failed",
            Self::DontAct => "dont-act",
            Self::Session => "session",
        }
    }

    /// Whether the source nominates the task as hard.
    pub fn is_hard(self) -> bool {
        matches!(
            self,
            Self::HardParked | Self::HardRetried | Self::HardFailed
        )
    }
}

/// The calibration tier: measured by running the baseline, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    #[default]
    Uncalibrated,
    /// The baseline never passed: capability headroom.
    Frontier,
    /// The baseline sometimes passed: where success differences show.
    Discriminating,
    /// The baseline always passed: regressions and efficiency.
    Regression,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncalibrated => "uncalibrated",
            Self::Frontier => "frontier",
            Self::Discriminating => "discriminating",
            Self::Regression => "regression",
        }
    }

    /// The tier a baseline pass count puts a task in.
    pub fn of(passes: u32, trials: u32) -> Self {
        match (passes, trials) {
            (_, 0) => Self::Uncalibrated,
            (0, _) => Self::Frontier,
            (p, n) if p >= n => Self::Regression,
            _ => Self::Discriminating,
        }
    }
}

/// Which split a task belongs to (fixed at freeze).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Split {
    #[default]
    Dev,
    Val,
    Test,
}

impl Split {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Val => "val",
            Self::Test => "test",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dev" => Ok(Self::Dev),
            "val" => Ok(Self::Val),
            "test" => Ok(Self::Test),
            other => Err(format!("split {other:?}: dev, val, or test")),
        }
    }
}

/// What a correct trial does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    /// Change the code until the oracle passes.
    #[default]
    Build,
    /// Inspect, verify, and change nothing.
    DontAct,
}

/// Where a set task came from, for the record.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Provenance {
    /// The run it was mined from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The hub session it was mined from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The task id it derives from (a don't-act task's original).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_task: Option<String>,
    /// The phase it was filed under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// One task of a set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetTask {
    pub id: String,
    pub source: Source,
    #[serde(default)]
    pub tier: Tier,
    #[serde(default)]
    pub split: Split,
    #[serde(default)]
    pub kind: TaskKind,
    /// The commit a trial starts from.
    pub base: String,
    /// The known-good commit (the oracle passes there).
    pub reference: String,
    /// Digest of the task's files in the set dir ([`files_digest`]).
    #[serde(default)]
    pub files_digest: String,
    /// Whether its oracle may run beside other trials (V5); `None` until
    /// validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_safe: Option<bool>,
    /// What the original run cost (the estimator's fallback prior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_cost_usd: Option<f64>,
    /// How long the oracle took at the reference (seconds), from V2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_secs: Option<f64>,
    /// Each prerequisite task id and the commit it landed as (empty when
    /// it never landed): V3 checks every one is in the base.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prerequisites: BTreeMap<String, String>,
    #[serde(default)]
    pub provenance: Provenance,
}

/// A set's manifest (`set.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetManifest {
    pub name: String,
    /// `None` for the draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The project the set's tasks belong to.
    pub project_id: String,
    /// The repository the commits live in.
    pub repo: PathBuf,
    pub created_at: DateTime<Utc>,
    pub tasks: Vec<SetTask>,
    /// The content hash; set at freeze.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

impl SetManifest {
    pub fn task(&self, id: &str) -> Option<&SetTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// `name@vN` (or `name@draft`).
    pub fn label(&self) -> String {
        match self.version {
            Some(v) => format!("{}@v{v}", self.name),
            None => format!("{}@draft", self.name),
        }
    }
}

/// One validation check's receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofCheck {
    /// `V1`..`V7`.
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// A task's validation receipts (`proofs/<task>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub task_id: String,
    pub passed: bool,
    /// The task files' digest the checks ran against: a draft edited after
    /// validation no longer freezes.
    pub files_digest: String,
    pub base: String,
    pub reference: String,
    pub parallel_safe: bool,
    pub oracle_secs: f64,
    pub checks: Vec<ProofCheck>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error(transparent)]
    Store(#[from] EvalStoreError),
}

pub(crate) fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SetError + '_ {
    move |source| SetError::Io {
        path: path.to_owned(),
        source,
    }
}

/// `name`, `name@vN`, or `name@draft`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRef {
    pub name: String,
    pub version: Option<VersionRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRef {
    Draft,
    V(u32),
}

impl SetRef {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (name, version) = match raw.split_once('@') {
            None => (raw, None),
            Some((name, "draft")) => (name, Some(VersionRef::Draft)),
            Some((name, v)) => {
                let n = v
                    .strip_prefix('v')
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("set {raw:?}: version is vN or draft"))?;
                (name, Some(VersionRef::V(n)))
            }
        };
        validate_name(name)?;
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }
}

impl fmt::Display for SetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.version {
            None => write!(f, "{}", self.name),
            Some(VersionRef::Draft) => write!(f, "{}@draft", self.name),
            Some(VersionRef::V(n)) => write!(f, "{}@v{n}", self.name),
        }
    }
}

/// A set (or task) name: `[a-z0-9][a-z0-9._-]*`, at most 64 characters.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "name {name:?}: lowercase letters, digits, '.', '_', '-'; at most 64"
        ))
    }
}

/// The files of one set version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDir {
    root: PathBuf,
}

impl SetDir {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<evals>/sets/<name>/draft`.
    pub fn draft(evals: &Path, name: &str) -> Self {
        Self::at(evals.join("sets").join(name).join("draft"))
    }

    /// `<evals>/sets/<name>/v<N>`.
    pub fn frozen(evals: &Path, name: &str, version: u32) -> Self {
        Self::at(evals.join("sets").join(name).join(format!("v{version}")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn manifest(&self) -> PathBuf {
        self.root.join("set.json")
    }
    pub fn task_file(&self, id: &str) -> PathBuf {
        self.root.join("tasks").join(format!("{id}.json"))
    }
    pub fn oracle_dir(&self, id: &str) -> PathBuf {
        self.root.join("oracles").join(id)
    }
    /// The task's held-out registration inside the set.
    pub fn registration(&self, id: &str) -> PathBuf {
        self.oracle_dir(id).join("registration.json")
    }
    pub fn spec_dir(&self, id: &str) -> PathBuf {
        self.root.join("specs").join(id)
    }
    pub fn proof_file(&self, id: &str) -> PathBuf {
        self.root.join("proofs").join(format!("{id}.json"))
    }

    pub fn load(&self) -> Result<SetManifest, SetError> {
        let path = self.manifest();
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        serde_json::from_str(&text)
            .map_err(|e| SetError::Invalid(format!("{}: {e}", path.display())))
    }

    pub fn save(&self, manifest: &SetManifest) -> Result<(), SetError> {
        write_json(&self.manifest(), manifest)
    }

    pub fn proof(&self, id: &str) -> Result<Option<Proof>, SetError> {
        let path = self.proof_file(id);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| SetError::Invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&path)(e)),
        }
    }

    pub fn save_proof(&self, proof: &Proof) -> Result<(), SetError> {
        write_json(&self.proof_file(&proof.task_id), proof)
    }
}

/// Write `value` as pretty JSON, creating the parent directory.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), SetError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| SetError::Invalid(format!("{}: {e}", path.display())))?;
    std::fs::write(path, format!("{text}\n")).map_err(io(path))
}

/// The sha256 of a file's bytes, hex.
pub fn file_sha256(path: &Path) -> Result<String, SetError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Every regular file under `dir` (recursively), as sorted relative paths.
pub fn files_under(dir: &Path) -> Result<Vec<PathBuf>, SetError> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut stack = vec![dir.to_owned()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).map_err(io(&at))? {
            let entry = entry.map_err(io(&at))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(io(&path))?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                out.push(path.strip_prefix(dir).expect("walked under dir").to_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The digest of everything a set task is made of: its task file, its
/// oracle dir, and its spec dir (relative paths and contents).
pub fn files_digest(dir: &SetDir, id: &str) -> Result<String, SetError> {
    let mut hasher = Sha256::new();
    let task = dir.task_file(id);
    hasher.update(format!("tasks/{id}.json\0{}\n", file_sha256(&task)?));
    for (prefix, root) in [("oracles", dir.oracle_dir(id)), ("specs", dir.spec_dir(id))] {
        for rel in files_under(&root)? {
            let digest = file_sha256(&root.join(&rel))?;
            hasher.update(format!("{prefix}/{id}/{}\0{digest}\n", rel.display()));
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The content hash of a set: its name and every task's record (sorted by
/// id), digests included; independent of task order, creation time, and
/// version number.
pub fn set_hash(manifest: &SetManifest) -> String {
    let mut tasks: Vec<&SetTask> = manifest.tasks.iter().collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    let value = serde_json::json!({
        "name": manifest.name,
        "project_id": manifest.project_id,
        "tasks": tasks,
    });
    let canonical = flywheel_core::loaders::canonical_json(&value);
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Split fractions (`dev`, `val`, `test`), each in [0, 1], summing to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitRatios {
    pub dev: f64,
    pub val: f64,
    pub test: f64,
}

impl Default for SplitRatios {
    fn default() -> Self {
        Self {
            dev: 1.0,
            val: 0.0,
            test: 0.0,
        }
    }
}

impl SplitRatios {
    /// `dev=.6,val=.2,test=.2` (missing keys are 0).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut r = Self {
            dev: 0.0,
            val: 0.0,
            test: 0.0,
        };
        for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("split {part:?}: NAME=FRACTION"))?;
            let v: f64 = v
                .trim()
                .parse()
                .map_err(|_| format!("split {part:?}: not a number"))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("split {part:?}: a fraction in [0, 1]"));
            }
            match Split::parse(k.trim())? {
                Split::Dev => r.dev = v,
                Split::Val => r.val = v,
                Split::Test => r.test = v,
            }
        }
        if ((r.dev + r.val + r.test) - 1.0).abs() > 1e-6 {
            return Err(format!("split {raw:?}: fractions must sum to 1"));
        }
        Ok(r)
    }
}

/// Assign splits deterministically: tasks ordered by sha256(seed, id), the
/// first `round(n * dev)` dev, the next `round(n * val)` val, the rest test.
pub fn assign_splits(tasks: &mut [SetTask], ratios: SplitRatios, seed: u64) {
    let mut order: Vec<(String, usize)> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let key = hex::encode(Sha256::digest(format!("{seed}:{}", t.id).as_bytes()));
            (key, i)
        })
        .collect();
    order.sort();
    let n = tasks.len() as f64;
    let dev = (n * ratios.dev).round() as usize;
    let val = (n * ratios.val).round() as usize;
    for (rank, (_, i)) in order.into_iter().enumerate() {
        tasks[i].split = if rank < dev {
            Split::Dev
        } else if rank < dev + val {
            Split::Val
        } else {
            Split::Test
        };
    }
}

/// The frozen versions of `name` on disk, ascending.
pub fn frozen_versions(evals: &Path, name: &str) -> Result<Vec<u32>, SetError> {
    let dir = evals.join("sets").join(name);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
        let entry = entry.map_err(io(&dir))?;
        if let Some(n) = entry
            .file_name()
            .to_str()
            .and_then(|f| f.strip_prefix('v'))
            .and_then(|n| n.parse::<u32>().ok())
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Resolve a set reference to its directory (`name` = its latest frozen
/// version).
pub fn resolve(evals: &Path, set: &SetRef) -> Result<SetDir, SetError> {
    match set.version {
        Some(VersionRef::Draft) => Ok(SetDir::draft(evals, &set.name)),
        Some(VersionRef::V(n)) => Ok(SetDir::frozen(evals, &set.name, n)),
        None => frozen_versions(evals, &set.name)?
            .last()
            .map(|n| SetDir::frozen(evals, &set.name, *n))
            .ok_or_else(|| SetError::Invalid(format!("set {} has no frozen version", set.name))),
    }
}

/// The ref a set version pins a task's commit under.
pub fn pin_ref(name: &str, version: u32, task: &str, which: &str) -> String {
    format!("refs/flywheel/eval/{name}/v{version}/{task}/{which}")
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), SetError> {
    for rel in files_under(from)? {
        let dest = to.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        std::fs::copy(from.join(&rel), &dest).map_err(io(&dest))?;
    }
    Ok(())
}

fn make_read_only(dir: &Path) -> Result<(), SetError> {
    for rel in files_under(dir)? {
        let path = dir.join(rel);
        let mut perms = std::fs::metadata(&path).map_err(io(&path))?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).map_err(io(&path))?;
    }
    Ok(())
}

/// Freeze `name`'s draft into its next version.
///
/// Refused unless every task has a passing proof made against its current
/// files (a draft edited after validation must be validated again). The
/// version is the next after both what is on disk and what the store
/// indexes. Returns the frozen manifest.
pub fn freeze(
    evals: &Path,
    name: &str,
    store: &EvalStore,
    ratios: SplitRatios,
    seed: u64,
    now: DateTime<Utc>,
) -> Result<SetManifest, SetError> {
    let draft = SetDir::draft(evals, name);
    let mut manifest = draft.load()?;
    if manifest.tasks.is_empty() {
        return Err(SetError::Invalid(format!("set {name}@draft has no tasks")));
    }
    let mut unproven = Vec::new();
    for task in &mut manifest.tasks {
        let digest = files_digest(&draft, &task.id)?;
        match draft.proof(&task.id)? {
            Some(proof)
                if proof.passed
                    && proof.files_digest == digest
                    && proof.base == task.base
                    && proof.reference == task.reference =>
            {
                task.files_digest = digest;
                task.parallel_safe = Some(proof.parallel_safe);
                task.oracle_secs = Some(proof.oracle_secs);
            }
            Some(proof) if proof.passed => {
                unproven.push(format!("{} (changed since it was validated)", task.id));
            }
            Some(_) => unproven.push(format!("{} (failed validation)", task.id)),
            None => unproven.push(format!("{} (not validated)", task.id)),
        }
    }
    if !unproven.is_empty() {
        return Err(SetError::Invalid(format!(
            "set {name}@draft cannot freeze; run `fw eval set validate {name}@draft` for: {}",
            unproven.join(", ")
        )));
    }
    assign_splits(&mut manifest.tasks, ratios, seed);
    let on_disk = frozen_versions(evals, name)?.last().copied().unwrap_or(0);
    let indexed = store
        .sets()?
        .iter()
        .filter(|s| s.name == name)
        .map(|s| s.version)
        .max()
        .unwrap_or(0);
    let version = on_disk.max(indexed) + 1;
    manifest.version = Some(version);
    manifest.created_at = now;
    let hash = set_hash(&manifest);
    manifest.hash = Some(hash.clone());

    let frozen = SetDir::frozen(evals, name, version);
    if frozen.root().exists() {
        return Err(SetError::Invalid(format!(
            "{} already exists",
            frozen.root().display()
        )));
    }
    copy_tree(draft.root(), frozen.root())?;
    frozen.save(&manifest)?;
    for task in &manifest.tasks {
        git::update_ref(
            &manifest.repo,
            &pin_ref(name, version, &task.id, "base"),
            &task.base,
        )?;
        git::update_ref(
            &manifest.repo,
            &pin_ref(name, version, &task.id, "ref"),
            &task.reference,
        )?;
    }
    make_read_only(frozen.root())?;
    store.put_set(&SetRow {
        name: name.to_owned(),
        version,
        hash,
        created_at: now,
        task_count: u32::try_from(manifest.tasks.len()).unwrap_or(u32::MAX),
        path: frozen.root().display().to_string(),
    })?;
    Ok(manifest)
}

/// What changed between two set versions, by task id.
pub fn diff(a: &SetManifest, b: &SetManifest) -> Vec<String> {
    let index = |m: &SetManifest| -> BTreeMap<String, SetTask> {
        m.tasks.iter().map(|t| (t.id.clone(), t.clone())).collect()
    };
    let (ia, ib) = (index(a), index(b));
    let mut out = Vec::new();
    for (id, ta) in &ia {
        match ib.get(id) {
            None => out.push(format!("- {id}")),
            Some(tb) => {
                let mut what = Vec::new();
                if ta.base != tb.base {
                    what.push("base");
                }
                if ta.reference != tb.reference {
                    what.push("reference");
                }
                if ta.files_digest != tb.files_digest {
                    what.push("files");
                }
                if ta.tier != tb.tier {
                    what.push("tier");
                }
                if ta.split != tb.split {
                    what.push("split");
                }
                if !what.is_empty() {
                    out.push(format!("~ {id} ({})", what.join(", ")));
                }
            }
        }
    }
    for id in ib.keys().filter(|id| !ia.contains_key(*id)) {
        out.push(format!("+ {id}"));
    }
    out
}

#[cfg(test)]
mod tests;
