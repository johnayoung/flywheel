//! Per-cycle repo context for autopilot: the shared digest, the structural
//! tier pre-filter, and the durable discovery-verdict cache.
//!
//! Three cost cuts over the refill pass (`docs/autopilot.md`, "Cycle cost
//! controls"):
//!
//! * [`build_repo_digest`] -- ONE bounded repo summary (file-tree layout,
//!   file-type histogram, structural markers, root manifest and README
//!   excerpts, recent git log) built once per cycle and injected into every
//!   discovery and authoring prompt, so each agent session stops
//!   re-surveying the same tree from cold.
//! * [`structural_prefilter`] -- a deterministic, file-listing-only check
//!   that skips a tier whose precondition is structurally absent (no
//!   deployment config for "production down", no build manifest or CI for
//!   "broken build", no source code for the code-only tiers). A skipped tier
//!   is recorded with an explicit reason; it never reaches an agent.
//! * [`DiscoveryCache`] -- tier verdicts persisted under
//!   `.flywheel/logs/autopilot/` keyed on the repo state (HEAD sha plus a
//!   working-tree fingerprint) and, per tier, on the exact rendered prompt
//!   and agent configuration, so an unchanged repo reuses its verdicts
//!   across cycles and daemon restarts. Bounded by entry count and TTL.
//!
//! Everything here is best-effort by design: a git or file failure degrades
//! to "no cache" / "no markers", never to a failed cycle.

use crate::autopilot::{Tier, TierVerdict};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Bumped whenever the digest layout changes, so a cached digest from an
/// older build is never reused.
pub const DIGEST_FORMAT_VERSION: u32 = 1;

/// Bumped whenever the pre-filter rules change, so a cached skip decision
/// from an older build is never reused.
pub const PREFILTER_VERSION: u32 = 1;

/// Hard cap on the digest text (characters). Sized so 11 subagent prompts
/// plus each authoring prompt carry it for well under one discovery
/// session's own repo survey.
pub const DIGEST_MAX_CHARS: usize = 8_000;

/// Repo-relative path prefix autopilot owns; excluded from the inventory and
/// the working-tree fingerprint so emitting a task never invalidates the
/// cache or reshapes the digest.
pub const FLYWHEEL_DIR_PREFIX: &str = ".flywheel/";

/// Upper bound on the inventory size, so a pathological tree cannot balloon
/// the per-cycle scan.
pub const MAX_INVENTORY_FILES: usize = 200_000;

/// Directories the non-git fallback walk never descends into.
const WALK_SKIP_DIRS: [&str; 7] = [
    ".git",
    ".flywheel",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
];

// --- Inventory ----------------------------------------------------------------------

/// The repo's file listing (repo-relative, `/`-separated, sorted, unique),
/// excluding autopilot's own `.flywheel/` state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoInventory {
    pub files: Vec<String>,
}

impl RepoInventory {
    /// An inventory over an explicit listing (normalized and sorted).
    pub fn from_files<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: BTreeSet<String> = files
            .into_iter()
            .map(Into::into)
            .filter(|f| !f.is_empty() && !f.starts_with(FLYWHEEL_DIR_PREFIX))
            .take(MAX_INVENTORY_FILES)
            .collect();
        Self {
            files: set.into_iter().collect(),
        }
    }

    /// List `repo_root`'s files: tracked plus untracked-not-ignored via
    /// `git ls-files`, falling back to a bounded directory walk outside git.
    pub fn scan(repo_root: &Path) -> Self {
        match git_bytes(
            repo_root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        ) {
            Some(out) => Self::from_files(
                out.split(|b| *b == 0)
                    .filter(|chunk| !chunk.is_empty())
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned()),
            ),
            None => Self::from_files(walk_files(repo_root)),
        }
    }
}

/// Bounded, deterministic directory walk for a repo outside git.
fn walk_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let is_dir = e.file_type().ok()?.is_dir();
                Some((e.file_name().to_string_lossy().into_owned(), is_dir))
            })
            .collect();
        names.sort();
        for (name, is_dir) in names {
            let child = rel.join(&name);
            if is_dir {
                if !WALK_SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(child);
                }
            } else {
                out.push(child.to_string_lossy().replace('\\', "/"));
                if out.len() >= MAX_INVENTORY_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// Run git in `repo_root`; stdout on success, `None` on any failure.
fn git_bytes(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn git_text(repo_root: &Path, args: &[&str]) -> Option<String> {
    git_bytes(repo_root, args).map(|b| String::from_utf8_lossy(&b).trim().to_owned())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(path: &str) -> Option<String> {
    let name = basename(path);
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

fn dir_segments(path: &str) -> impl Iterator<Item = String> + '_ {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.into_iter().map(str::to_ascii_lowercase)
}

// --- Structural markers -------------------------------------------------------------

const DEPLOY_BASENAMES: [&str; 33] = [
    "dockerfile",
    "containerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
    "procfile",
    "fly.toml",
    "vercel.json",
    "now.json",
    "netlify.toml",
    "app.yaml",
    "render.yaml",
    "serverless.yml",
    "serverless.yaml",
    "serverless.ts",
    "chart.yaml",
    "kustomization.yaml",
    "kustomization.yml",
    "skaffold.yaml",
    "wrangler.toml",
    "wrangler.json",
    "wrangler.jsonc",
    "appspec.yml",
    "appspec.yaml",
    "railway.json",
    "railway.toml",
    "nixpacks.toml",
    "heroku.yml",
    "cloudbuild.yaml",
    "cloudbuild.yml",
    "cdk.json",
    "pulumi.yaml",
];
const DEPLOY_EXTENSIONS: [&str; 3] = ["tf", "nomad", "service"];
const DEPLOY_DIRS: [&str; 9] = [
    "k8s",
    "kubernetes",
    "helm",
    "deploy",
    "deployment",
    "deployments",
    "terraform",
    "ansible",
    ".ebextensions",
];

/// True when `path` is evidence the repo is deployed somewhere: container
/// or orchestration config, a PaaS/serverless manifest, infrastructure as
/// code, a service unit, or a deploy workflow/script.
pub fn is_deployment_marker(path: &str) -> bool {
    let name = basename(path).to_ascii_lowercase();
    DEPLOY_BASENAMES.contains(&name.as_str())
        || name.starts_with("dockerfile.")
        || name.ends_with(".dockerfile")
        || name.contains("deploy")
        || extension(path).is_some_and(|e| DEPLOY_EXTENSIONS.contains(&e.as_str()))
        || dir_segments(path).any(|d| DEPLOY_DIRS.contains(&d.as_str()))
}

const BUILD_BASENAMES: [&str; 40] = [
    "cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "cmakelists.txt",
    "makefile",
    "gnumakefile",
    "meson.build",
    "build",
    "build.bazel",
    "workspace",
    "module.bazel",
    "gemfile",
    "rakefile",
    "mix.exs",
    "composer.json",
    "package.swift",
    "deno.json",
    "deno.jsonc",
    "build.zig",
    "justfile",
    "flake.nix",
    "default.nix",
    "stack.yaml",
    "dune-project",
    "rebar.config",
    "project.clj",
    "deps.edn",
    "build.sbt",
    "pubspec.yaml",
    "tox.ini",
    "noxfile.py",
    "taskfile.yml",
    "requirements.txt",
];
const BUILD_EXTENSIONS: [&str; 7] = [
    "csproj", "fsproj", "vbproj", "sln", "cabal", "gemspec", "nimble",
];
const CI_BASENAMES: [&str; 9] = [
    ".gitlab-ci.yml",
    "jenkinsfile",
    "azure-pipelines.yml",
    ".travis.yml",
    "bitbucket-pipelines.yml",
    ".drone.yml",
    "appveyor.yml",
    ".woodpecker.yml",
    ".cirrus.yml",
];
const CI_DIR_PREFIXES: [&str; 4] = [
    ".github/workflows/",
    ".circleci/",
    ".buildkite/",
    ".woodpecker/",
];

/// True when `path` is a build manifest (something that can fail to build).
pub fn is_build_manifest(path: &str) -> bool {
    let name = basename(path).to_ascii_lowercase();
    BUILD_BASENAMES.contains(&name.as_str())
        || extension(path).is_some_and(|e| BUILD_EXTENSIONS.contains(&e.as_str()))
}

/// True when `path` is CI pipeline configuration.
pub fn is_ci_config(path: &str) -> bool {
    let name = basename(path).to_ascii_lowercase();
    CI_BASENAMES.contains(&name.as_str())
        || CI_DIR_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

const SOURCE_EXTENSIONS: [&str; 56] = [
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "tsx", "go", "java", "kt", "kts", "scala",
    "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "fs", "vb", "rb", "php", "swift", "m", "mm",
    "ex", "exs", "erl", "hs", "ml", "mli", "clj", "cljs", "lua", "sh", "bash", "zsh", "ps1", "pl",
    "pm", "r", "jl", "dart", "zig", "nim", "sql", "vue", "svelte", "sol", "elm", "groovy", "f90",
];

/// True when `path` is source code in a recognized language.
pub fn is_source_file(path: &str) -> bool {
    extension(path).is_some_and(|e| SOURCE_EXTENSIONS.contains(&e.as_str()))
}

// --- Structural pre-filter ----------------------------------------------------------

/// A tier skipped before discovery because its precondition is
/// structurally absent from the repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierPrefilterSkip {
    pub tier: Tier,
    pub reason: String,
}

/// The reason prefix every pre-filter verdict carries.
pub const PREFILTER_REASON_PREFIX: &str = "structural pre-filter: ";

/// The precondition a tier's remit structurally needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Precondition {
    /// Tier 1 ("outages, data loss, active breach, payments failing")
    /// presupposes a deployed system.
    Deployment,
    /// Tier 3 ("CI red, main can't deploy") presupposes a build or a
    /// pipeline.
    BuildOrCi,
    /// Tiers 6-8 (tests for code, defects in code, refactoring code)
    /// presuppose code.
    SourceCode,
}

/// Tier -> precondition, derived from the tier remits in
/// [`Tier::description`]. Tiers absent here (2, 4, 5, 9, 10, 11) have no
/// structural precondition a file listing can refute and are always
/// dispatched.
const PREFILTER_RULES: [(Tier, Precondition, &str); 5] = [
    (
        Tier::ProductionDown,
        Precondition::Deployment,
        "no deployment configuration in the repo (container, orchestration, PaaS/serverless, \
         infrastructure-as-code, service unit, or deploy workflow), so there is no production \
         system for this tier to cover",
    ),
    (
        Tier::BrokenBuild,
        Precondition::BuildOrCi,
        "no build manifest or CI configuration in the repo, so there is no build or pipeline to \
         break",
    ),
    (
        Tier::TestCoverage,
        Precondition::SourceCode,
        "no source code in the repo, so there is no code to cover with tests",
    ),
    (
        Tier::NonCriticalBugs,
        Precondition::SourceCode,
        "no source code in the repo, so there is no code to carry defects",
    ),
    (
        Tier::TechDebt,
        Precondition::SourceCode,
        "no source code in the repo, so there is no code to refactor",
    ),
];

/// The tiers whose precondition is structurally absent from `inventory`,
/// in tier order. Deterministic: a pure function of the file listing.
pub fn structural_prefilter(inventory: &RepoInventory) -> Vec<TierPrefilterSkip> {
    let has = |pred: fn(&str) -> bool| inventory.files.iter().any(|f| pred(f));
    let deployment = has(is_deployment_marker);
    let build_or_ci = has(is_build_manifest) || has(is_ci_config);
    let source = has(is_source_file);
    PREFILTER_RULES
        .iter()
        .filter(|(_, precondition, _)| match precondition {
            Precondition::Deployment => !deployment,
            Precondition::BuildOrCi => !build_or_ci,
            Precondition::SourceCode => !source,
        })
        .map(|(tier, _, why)| TierPrefilterSkip {
            tier: *tier,
            reason: format!("{PREFILTER_REASON_PREFIX}{why}"),
        })
        .collect()
}

// --- Digest -------------------------------------------------------------------------

const MAX_LAYOUT_LINES: usize = 60;
const MAX_MARKERS_PER_KIND: usize = 8;
const MAX_EXTENSIONS: usize = 12;
const MAX_LOG_COMMITS: &str = "10";
const MANIFEST_EXCERPT_CHARS: usize = 1_500;
const README_EXCERPT_CHARS: usize = 2_500;
const EXCERPT_MAX_LINES: usize = 60;
const ROOT_MANIFESTS: [&str; 12] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "mix.exs",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
];
const READMES: [&str; 5] = [
    "README.md",
    "README.rst",
    "README.txt",
    "README",
    "readme.md",
];

/// Truncate `text` to `max` characters on a char boundary, marking the cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n[... truncated]");
    out
}

/// The first `EXCERPT_MAX_LINES` lines / `max_chars` chars of a root file.
fn excerpt(repo_root: &Path, rel: &str, max_chars: usize) -> Option<String> {
    let text = std::fs::read_to_string(repo_root.join(rel)).ok()?;
    let head: Vec<&str> = text.lines().take(EXCERPT_MAX_LINES).collect();
    let body = clip(&head.join("\n"), max_chars);
    (!body.trim().is_empty()).then_some(body)
}

/// Two-level directory layout with per-directory file counts.
fn layout_lines(inventory: &RepoInventory) -> Vec<String> {
    let mut top: BTreeMap<String, usize> = BTreeMap::new();
    let mut top_files: Vec<String> = Vec::new();
    let mut second: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for file in &inventory.files {
        let parts: Vec<&str> = file.split('/').collect();
        if parts.len() == 1 {
            top_files.push(file.clone());
            continue;
        }
        *top.entry(parts[0].to_owned()).or_default() += 1;
        if parts.len() > 2 {
            *second
                .entry(parts[0].to_owned())
                .or_default()
                .entry(parts[1].to_owned())
                .or_default() += 1;
        }
    }
    let mut lines = Vec::new();
    for (dir, count) in &top {
        lines.push(format!("  {dir}/ ({count} files)"));
        if let Some(children) = second.get(dir) {
            for (child, n) in children {
                lines.push(format!("    {dir}/{child}/ ({n})"));
            }
        }
    }
    lines.extend(top_files.iter().map(|f| format!("  {f}")));
    if lines.len() > MAX_LAYOUT_LINES {
        let omitted = lines.len() - MAX_LAYOUT_LINES;
        lines.truncate(MAX_LAYOUT_LINES);
        lines.push(format!("  [... {omitted} more entries]"));
    }
    lines
}

fn extension_histogram(inventory: &RepoInventory) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in &inventory.files {
        if let Some(ext) = extension(file) {
            *counts.entry(ext).or_default() += 1;
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(MAX_EXTENSIONS)
        .map(|(ext, n)| format!(".{ext} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn marker_line(inventory: &RepoInventory, label: &str, pred: fn(&str) -> bool) -> String {
    let hits: Vec<&String> = inventory.files.iter().filter(|f| pred(f)).collect();
    if hits.is_empty() {
        return format!("  {label}: none found");
    }
    let shown: Vec<&str> = hits
        .iter()
        .take(MAX_MARKERS_PER_KIND)
        .map(|s| s.as_str())
        .collect();
    let more = hits.len().saturating_sub(MAX_MARKERS_PER_KIND);
    let suffix = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    format!("  {label}: {}{suffix}", shown.join(", "))
}

/// Build the shared, bounded repo digest for one cycle.
///
/// Deterministic for a given repo state: the file layout and type
/// histogram from `inventory`, the structural markers the pre-filter keys
/// on, root manifest and README excerpts, and the last commits. Capped at
/// [`DIGEST_MAX_CHARS`].
pub fn build_repo_digest(
    repo_root: &Path,
    inventory: &RepoInventory,
    head: Option<&str>,
) -> String {
    let mut out = String::new();
    let head_text = head.map_or_else(|| "not a git repository".to_owned(), short_sha);
    out.push_str(&format!(
        "Repository digest (HEAD {head_text}; {} files, excluding .flywheel/)\n",
        inventory.files.len()
    ));
    if head.is_some()
        && let Some(log) = git_text(
            repo_root,
            &[
                "log",
                "-n",
                MAX_LOG_COMMITS,
                "--no-decorate",
                "--format=%h %s",
            ],
        )
        && !log.is_empty()
    {
        out.push_str("Recent commits:\n");
        for line in log.lines() {
            out.push_str(&format!("  {}\n", clip(line, 120)));
        }
    }
    out.push_str("Layout:\n");
    for line in layout_lines(inventory) {
        out.push_str(&line);
        out.push('\n');
    }
    let histogram = extension_histogram(inventory);
    if !histogram.is_empty() {
        out.push_str(&format!("File types: {histogram}\n"));
    }
    out.push_str("Structural markers:\n");
    out.push_str(&marker_line(inventory, "deployment", is_deployment_marker));
    out.push('\n');
    out.push_str(&marker_line(inventory, "ci", is_ci_config));
    out.push('\n');
    out.push_str(&marker_line(
        inventory,
        "build manifests",
        is_build_manifest,
    ));
    out.push('\n');
    let present: BTreeSet<&str> = inventory.files.iter().map(String::as_str).collect();
    for manifest in ROOT_MANIFESTS
        .iter()
        .filter(|m| present.contains(**m))
        .take(2)
    {
        if let Some(body) = excerpt(repo_root, manifest, MANIFEST_EXCERPT_CHARS) {
            out.push_str(&format!("{manifest} (excerpt):\n{body}\n"));
        }
    }
    if let Some(readme) = READMES.iter().find(|r| present.contains(**r))
        && let Some(body) = excerpt(repo_root, readme, README_EXCERPT_CHARS)
    {
        out.push_str(&format!("{readme} (excerpt):\n{body}\n"));
    }
    clip(out.trim_end(), DIGEST_MAX_CHARS)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Append the shared digest to an agent prompt.
///
/// The prompt above it is unchanged; the digest rides as a trailing
/// section so a tier subagent or authoring agent starts from the cycle's
/// shared survey instead of re-reading the tree.
pub fn prompt_with_digest(prompt: String, digest: Option<&str>) -> String {
    match digest.filter(|d| !d.trim().is_empty()) {
        None => prompt,
        Some(digest) => format!(
            "{prompt}\n\n\
             SHARED REPOSITORY DIGEST (built once this cycle by autopilot; use it for the \
             repo's layout, manifests, and recent history instead of re-surveying them, and \
             open files only to confirm specific evidence):\n\
             <repo-digest>\n{digest}\n</repo-digest>"
        ),
    }
}

// --- Repo state (the cache key) -----------------------------------------------------

/// The repo state a cached verdict is valid for: HEAD plus a fingerprint
/// of the uncommitted working tree (outside `.flywheel/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub head: String,
    pub tree_fingerprint: String,
}

impl RepoState {
    /// Probe `repo_root`; `Err` with a reason when it has no git HEAD.
    pub fn probe(repo_root: &Path) -> Result<Self, String> {
        let head = git_text(repo_root, &["rev-parse", "--verify", "HEAD"])
            .filter(|h| !h.is_empty())
            .ok_or_else(|| "repository has no git HEAD".to_owned())?;
        let status = git_bytes(
            repo_root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--",
                ".",
                ":(exclude).flywheel",
            ],
        )
        .ok_or_else(|| "git status failed".to_owned())?;
        Ok(Self {
            head,
            tree_fingerprint: tree_fingerprint(repo_root, &status),
        })
    }

    /// The digest/pre-filter key: this state under the current digest and
    /// pre-filter versions.
    pub fn state_key(&self) -> String {
        sha256_hex(
            &json!({
                "cache_schema": DISCOVERY_CACHE_SCHEMA_VERSION,
                "digest_format": DIGEST_FORMAT_VERSION,
                "prefilter": PREFILTER_VERSION,
                "head": self.head,
                "tree": self.tree_fingerprint,
            })
            .to_string(),
        )
    }
}

/// Hash `git status -z` plus each listed path's size and mtime, so editing
/// an already-dirty file still changes the fingerprint.
fn tree_fingerprint(repo_root: &Path, status: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(status);
    for token in status.split(|b| *b == 0).filter(|t| !t.is_empty()) {
        let text = String::from_utf8_lossy(token);
        let path = if text.len() > 3 && text.as_bytes()[2] == b' ' {
            &text[3..]
        } else {
            &text[..]
        };
        hasher.update(path.as_bytes());
        match std::fs::metadata(repo_root.join(path)) {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                hasher.update(meta.len().to_le_bytes());
                hasher.update(mtime.to_le_bytes());
            }
            Err(_) => hasher.update(b"absent"),
        }
    }
    hex::encode(hasher.finalize())
}

pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

// --- The durable verdict cache ------------------------------------------------------

/// Bumped whenever the cache file shape changes; an unrecognized version
/// reads as empty.
pub const DISCOVERY_CACHE_SCHEMA_VERSION: u64 = 1;

/// At most this many repo states are retained (most recent first).
pub const DISCOVERY_CACHE_MAX_ENTRIES: usize = 8;

/// The cache file, beside the daemon's activity record (gitignored with the
/// rest of `.flywheel/logs/`).
pub fn default_discovery_cache_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".flywheel")
        .join("logs")
        .join("autopilot")
        .join("discovery-cache.json")
}

/// One tier verdict, valid only under `key` (the rendered prompt and agent
/// configuration it was produced with).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedVerdict {
    pub key: String,
    pub verdict: TierVerdict,
}

/// Everything cached for one repo state.
///
/// `consumed` records the finding ids already authored (emitted or
/// dropped) at this state, so a reused verdict never re-authors the same
/// finding into a duplicate task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryCacheEntry {
    pub state_key: String,
    pub head: String,
    pub created_at: f64,
    pub digest: String,
    pub prefiltered: Vec<TierPrefilterSkip>,
    #[serde(default)]
    pub verdicts: Vec<CachedVerdict>,
    #[serde(default)]
    pub consumed: BTreeSet<String>,
}

impl DiscoveryCacheEntry {
    /// The cached verdict for `tier` when it was produced under `key`.
    pub fn verdict_for(&self, tier: Tier, key: &str) -> Option<&TierVerdict> {
        self.verdicts
            .iter()
            .find(|c| c.verdict.tier == tier && c.key == key)
            .map(|c| &c.verdict)
    }

    /// Store `verdict` under `key`, replacing any earlier one for its tier.
    pub fn put_verdict(&mut self, key: String, verdict: TierVerdict) {
        self.verdicts.retain(|c| c.verdict.tier != verdict.tier);
        self.verdicts.push(CachedVerdict { key, verdict });
        self.verdicts.sort_by_key(|c| c.verdict.tier);
    }
}

/// How one refill pass used the discovery-verdict cache.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiscoveryCacheStatus {
    /// The pass never reached discovery (e.g. the queue was already full).
    #[default]
    NotConsulted,
    /// The cache was off for this pass, with the reason.
    Disabled(String),
    /// Every dispatchable tier was re-discovered.
    Miss,
    /// Some tiers reused cached verdicts; the rest were re-discovered.
    Partial { reused: Vec<Tier> },
    /// Every dispatchable tier reused its cached verdict; no agent ran.
    Hit,
}

impl std::fmt::Display for DiscoveryCacheStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryCacheStatus::NotConsulted => write!(f, "not consulted"),
            DiscoveryCacheStatus::Disabled(reason) => write!(f, "disabled ({reason})"),
            DiscoveryCacheStatus::Miss => write!(f, "miss"),
            DiscoveryCacheStatus::Partial { reused } => {
                let tiers: Vec<String> = reused.iter().map(ToString::to_string).collect();
                write!(f, "partial (reused tiers {})", tiers.join(", "))
            }
            DiscoveryCacheStatus::Hit => write!(f, "hit"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DiscoveryCacheFile {
    schema_version: u64,
    entries: Vec<DiscoveryCacheEntry>,
}

/// The on-disk verdict cache: bounded by [`DISCOVERY_CACHE_MAX_ENTRIES`]
/// and a TTL measured from each entry's creation.
#[derive(Debug)]
pub struct DiscoveryCache {
    path: PathBuf,
    ttl_seconds: f64,
    entries: Vec<DiscoveryCacheEntry>,
}

impl DiscoveryCache {
    /// Load the cache at `path`, dropping entries older than `ttl_seconds`
    /// at `now` (epoch seconds). A missing, malformed, or other-version
    /// file loads empty.
    pub fn load(path: impl Into<PathBuf>, ttl_seconds: f64, now: f64) -> Self {
        let path = path.into();
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<DiscoveryCacheFile>(&text).ok())
            .filter(|file| file.schema_version == DISCOVERY_CACHE_SCHEMA_VERSION)
            .map(|file| file.entries)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| now - e.created_at < ttl_seconds && e.created_at <= now)
            .collect();
        Self {
            path,
            ttl_seconds,
            entries,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ttl_seconds(&self) -> f64 {
        self.ttl_seconds
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The live entry for `state_key`, if any.
    pub fn entry(&self, state_key: &str) -> Option<&DiscoveryCacheEntry> {
        self.entries.iter().find(|e| e.state_key == state_key)
    }

    /// Insert or replace the entry for its state, most recent first,
    /// evicting beyond [`DISCOVERY_CACHE_MAX_ENTRIES`].
    pub fn upsert(&mut self, entry: DiscoveryCacheEntry) {
        self.entries.retain(|e| e.state_key != entry.state_key);
        self.entries.insert(0, entry);
        self.entries.truncate(DISCOVERY_CACHE_MAX_ENTRIES);
    }

    /// Persist atomically (write a sibling temp file, then rename).
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&DiscoveryCacheFile {
            schema_version: DISCOVERY_CACHE_SCHEMA_VERSION,
            entries: self.entries.clone(),
        })
        .map_err(std::io::Error::other)?;
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, format!("{body}\n"))?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn git_repo(tmp: &TempDir) -> PathBuf {
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        write(&root, "Cargo.toml", "[package]\nname = \"lib\"\n");
        write(&root, "src/lib.rs", "pub fn f() {}\n");
        write(&root, "README.md", "# lib\n\nA pure library.\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial commit"]);
        root
    }

    fn skipped(inventory: &RepoInventory) -> Vec<u8> {
        structural_prefilter(inventory)
            .iter()
            .map(|s| s.tier.value())
            .collect()
    }

    #[test]
    fn library_repo_skips_only_production_down() {
        let inv = RepoInventory::from_files(["Cargo.toml", "src/lib.rs", "README.md"]);
        let skips = structural_prefilter(&inv);
        assert_eq!(skipped(&inv), vec![1]);
        assert!(skips[0].reason.starts_with(PREFILTER_REASON_PREFIX));
        assert!(skips[0].reason.contains("no deployment configuration"));
    }

    #[test]
    fn deployment_evidence_keeps_production_down() {
        for marker in [
            "Dockerfile",
            "docker/Dockerfile.prod",
            "deploy/run.sh",
            "infra/main.tf",
            "k8s/app.yaml",
            "fly.toml",
            ".github/workflows/deploy-prod.yml",
            "ops/app.service",
        ] {
            let inv = RepoInventory::from_files(["Cargo.toml", "src/lib.rs", marker]);
            assert!(
                skipped(&inv).is_empty(),
                "{marker} should count as deployment"
            );
        }
    }

    #[test]
    fn docs_only_repo_skips_build_and_code_tiers() {
        let inv = RepoInventory::from_files(["README.md", "docs/guide.md"]);
        assert_eq!(skipped(&inv), vec![1, 3, 6, 7, 8]);
    }

    #[test]
    fn ci_config_alone_satisfies_the_broken_build_tier() {
        let inv = RepoInventory::from_files([".github/workflows/ci.yml", "notes.md"]);
        assert_eq!(skipped(&inv), vec![1, 6, 7, 8]);
    }

    #[test]
    fn inventory_excludes_flywheel_state_and_sorts() {
        let inv = RepoInventory::from_files(["b.rs", ".flywheel/tasks/x.json", "a.rs", "a.rs"]);
        assert_eq!(inv.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn scan_walks_outside_git_skipping_heavy_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.py", "print(1)\n");
        write(root, "node_modules/x/index.js", "");
        write(root, ".flywheel/tasks/t.json", "{}");
        let inv = RepoInventory::scan(root);
        assert_eq!(inv.files, vec!["src/main.py"]);
    }

    #[test]
    fn digest_carries_layout_markers_excerpts_and_log_within_the_cap() {
        let tmp = TempDir::new().unwrap();
        let root = git_repo(&tmp);
        let state = RepoState::probe(&root).unwrap();
        let inv = RepoInventory::scan(&root);
        let digest = build_repo_digest(&root, &inv, Some(&state.head));
        assert!(digest.starts_with(&format!(
            "Repository digest (HEAD {}; 3 files, excluding .flywheel/)",
            &state.head[..12]
        )));
        assert!(digest.contains("Recent commits:\n"));
        assert!(digest.contains("initial commit"));
        assert!(digest.contains("  src/ (1 files)"));
        assert!(digest.contains("  deployment: none found"));
        assert!(digest.contains("  build manifests: Cargo.toml"));
        assert!(digest.contains("Cargo.toml (excerpt):\n[package]"));
        assert!(digest.contains("README.md (excerpt):\n# lib"));
        assert!(digest.chars().count() <= DIGEST_MAX_CHARS + "\n[... truncated]".len());
        // Deterministic for the same state.
        assert_eq!(digest, build_repo_digest(&root, &inv, Some(&state.head)));
    }

    #[test]
    fn digest_is_capped() {
        let files: Vec<String> = (0..5_000).map(|i| format!("d{i}/f{i}.rs")).collect();
        let inv = RepoInventory::from_files(files);
        let digest = build_repo_digest(Path::new("/nonexistent"), &inv, None);
        assert!(digest.contains("more entries]"));
        assert!(digest.chars().count() <= DIGEST_MAX_CHARS + "\n[... truncated]".len());
    }

    #[test]
    fn prompt_with_digest_appends_and_none_is_identity() {
        assert_eq!(prompt_with_digest("P".to_owned(), None), "P");
        assert_eq!(prompt_with_digest("P".to_owned(), Some("  ")), "P");
        let with = prompt_with_digest("P".to_owned(), Some("D"));
        assert!(with.starts_with("P\n\nSHARED REPOSITORY DIGEST"));
        assert!(with.ends_with("<repo-digest>\nD\n</repo-digest>"));
    }

    #[test]
    fn state_key_tracks_head_and_dirty_tree_but_not_flywheel_state() {
        let tmp = TempDir::new().unwrap();
        let root = git_repo(&tmp);
        let clean = RepoState::probe(&root).unwrap();
        assert_eq!(clean, RepoState::probe(&root).unwrap());

        // Autopilot's own task emission never changes the key.
        write(&root, ".flywheel/tasks/active/autopilot/t.json", "{}");
        assert_eq!(
            clean.state_key(),
            RepoState::probe(&root).unwrap().state_key()
        );

        // An uncommitted edit does, and so does a further edit to the same file.
        write(&root, "src/lib.rs", "pub fn g() {}\n");
        let dirty = RepoState::probe(&root).unwrap();
        assert_ne!(clean.state_key(), dirty.state_key());
        write(&root, "src/lib.rs", "pub fn g() { let _ = 1; }\n");
        assert_ne!(
            dirty.state_key(),
            RepoState::probe(&root).unwrap().state_key()
        );

        // A new commit moves HEAD.
        git(&root, &["commit", "-qam", "second"]);
        let moved = RepoState::probe(&root).unwrap();
        assert_ne!(moved.head, clean.head);
    }

    #[test]
    fn probe_outside_git_reports_why() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            RepoState::probe(tmp.path()).unwrap_err(),
            "repository has no git HEAD"
        );
    }

    fn entry(state_key: &str, created_at: f64) -> DiscoveryCacheEntry {
        DiscoveryCacheEntry {
            state_key: state_key.to_owned(),
            head: "h".to_owned(),
            created_at,
            digest: "d".to_owned(),
            prefiltered: vec![TierPrefilterSkip {
                tier: Tier::ProductionDown,
                reason: "r".to_owned(),
            }],
            verdicts: Vec::new(),
            consumed: BTreeSet::new(),
        }
    }

    #[test]
    fn cache_round_trips_and_honours_ttl_and_entry_bound() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("logs/discovery-cache.json");
        let mut cache = DiscoveryCache::load(&path, 100.0, 1_000.0);
        assert!(cache.is_empty());
        let mut e = entry("s0", 1_000.0);
        e.consumed.insert("t5-f1".to_owned());
        cache.upsert(e.clone());
        cache.save().unwrap();

        let reloaded = DiscoveryCache::load(&path, 100.0, 1_050.0);
        assert_eq!(reloaded.entry("s0"), Some(&e));
        // Past the TTL the entry is gone.
        assert!(DiscoveryCache::load(&path, 100.0, 1_100.0).is_empty());

        let mut bounded = DiscoveryCache::load(&path, 1e9, 1_000.0);
        for i in 0..(DISCOVERY_CACHE_MAX_ENTRIES + 3) {
            bounded.upsert(entry(&format!("n{i}"), 1_000.0));
        }
        assert_eq!(bounded.len(), DISCOVERY_CACHE_MAX_ENTRIES);
        assert!(bounded.entry("s0").is_none());
        assert!(
            bounded
                .entry(&format!("n{}", DISCOVERY_CACHE_MAX_ENTRIES + 2))
                .is_some()
        );
    }

    #[test]
    fn malformed_or_other_version_cache_loads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(DiscoveryCache::load(&path, 100.0, 0.0).is_empty());
        std::fs::write(&path, r#"{"schema_version": 999, "entries": []}"#).unwrap();
        assert!(DiscoveryCache::load(&path, 100.0, 0.0).is_empty());
    }

    #[test]
    fn verdicts_are_keyed_per_tier() {
        let mut e = entry("s", 0.0);
        let verdict = TierVerdict {
            tier: Tier::Documentation,
            relevant: true,
            reason: "docs".to_owned(),
            findings: Vec::new(),
            errored: false,
        };
        e.put_verdict("k1".to_owned(), verdict.clone());
        assert_eq!(e.verdict_for(Tier::Documentation, "k1"), Some(&verdict));
        assert_eq!(e.verdict_for(Tier::Documentation, "k2"), None);
        e.put_verdict("k2".to_owned(), verdict.clone());
        assert_eq!(e.verdicts.len(), 1);
        assert_eq!(e.verdict_for(Tier::Documentation, "k2"), Some(&verdict));
    }
}
