//! The hub's flywheel task seam (`flywheel_hub::fanout::promote`): a
//! verified fan-out written as a task of a repo's directory work source,
//! and a task read back as a fan-out seed -- through flywheel-core's task
//! loader and the orchestrator's directory walk and held-out registry, so
//! what the hub writes is exactly what `fw worker` and `fw status` read.
//!
//! Where the file goes: `<tasks dir>/active/<phase>/<id>.json` of the
//! folder's project (the panel's resolution: the project's central
//! `$FLYWHEEL_HOME/projects/<id>/tasks`, outside the repo tree, so the hub
//! never writes into the checkout; `fw spec export` is how a team commits
//! its specs).
//! An id already defined there (active or archived), or with runs in the
//! store, is refused: the loop keys runs by task id, so a reused id would
//! read as already done.

use crate::hub_flywheel::{HubFlywheel, TaskLayout};
use flywheel_core::loaders::{load_task_data, load_task_file, load_task_str, task_digest};
use flywheel_core::task::{Grader, Task};
use flywheel_hub::fanout::GraderSpec;
use flywheel_hub::fanout::promote::{
    SeedContext, TaskDraft, TaskFilePlan, TaskSeed, TaskWritten, protected_in_constraint, slug,
};
use flywheel_orchestrator::held_out_gate::HeldOutGraderSource;
use flywheel_orchestrator::sources::{
    DirectoryWorkSource, WorkSource, iter_active_phase_dirs, iter_active_task_files,
    task_id_defined,
};
use flywheel_worktree::submitter::protected_path_matches;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The longest task id or phase name a promotion writes.
const MAX_NAME: usize = 100;

/// A single path segment the directory walk lists: starts with a letter or
/// digit (`_` and `.` files are skipped), then `[A-Za-z0-9._-]`.
fn check_segment(what: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && name != "."
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} {name:?} must be 1-{MAX_NAME} of [A-Za-z0-9._-], starting with a letter or \
             digit (one directory or file name)"
        ))
    }
}

fn dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// A new phase after every active and archived one: `NN-<slug>`.
fn next_phase(tasks_dir: &Path, task_id: &str) -> String {
    let mut names = dir_names(&tasks_dir.join("active"));
    names.extend(dir_names(&tasks_dir.join("archive")));
    let last = names
        .iter()
        .filter_map(|n| {
            let digits: String = n.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
        .max()
        .unwrap_or(0);
    let name = slug(task_id, 48);
    let name = if name.is_empty() {
        "fanout".into()
    } else {
        name
    };
    format!("{:02}-{name}", last + 1)
}

/// `draft` in flywheel's task file shape: empty optional fields left out,
/// as hand-written task files are.
fn task_value(draft: &TaskDraft) -> Value {
    let mut context = Map::new();
    for (key, list) in [
        ("relevant", &draft.relevant),
        ("references", &draft.references),
        ("constraints", &draft.constraints),
        ("non_goals", &draft.non_goals),
        ("edge_cases", &draft.edge_cases),
    ] {
        if !list.is_empty() {
            context.insert(key.into(), json!(list));
        }
    }
    if !draft.notes.trim().is_empty() {
        context.insert("notes".into(), json!(draft.notes));
    }
    let graders: Vec<Value> = draft
        .graders
        .iter()
        .map(|g| {
            let mut grader = Map::new();
            grader.insert("type".into(), json!("command"));
            grader.insert("run".into(), json!(g.run));
            if let Some(name) = &g.name {
                grader.insert("name".into(), json!(name));
            }
            Value::Object(grader)
        })
        .collect();
    let mut task = Map::new();
    task.insert("id".into(), json!(draft.task_id));
    task.insert("goal".into(), json!(draft.goal));
    if !draft.tags.is_empty() {
        task.insert("tags".into(), json!(draft.tags));
    }
    if !context.is_empty() {
        task.insert("context".into(), Value::Object(context));
    }
    task.insert("graders".into(), Value::Array(graders));
    Value::Object(task)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Whether git ignores `path` in `root` (`None`: not a git checkout, or
/// git cannot say).
fn git_ignored(root: &Path, path: &Path) -> Option<bool> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--no-index", "--"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// See `FlywheelProvider::plan_task`.
pub fn plan_task(
    provider: &HubFlywheel,
    folder: &Path,
    draft: &TaskDraft,
) -> Result<TaskFilePlan, String> {
    check_segment("task id", &draft.task_id)?;
    let TaskLayout {
        tasks_dir,
        protected_paths,
        ..
    } = provider.task_layout(folder)?;
    let phases: Vec<String> = iter_active_phase_dirs(&tasks_dir)
        .map_err(|e| format!("listing {}: {e}", tasks_dir.display()))?
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let phase = match &draft.phase {
        Some(phase) => {
            check_segment("phase", phase)?;
            if !tasks_dir.join("active").join(phase).is_dir()
                && tasks_dir.join("archive").join(phase).is_dir()
            {
                return Err(format!(
                    "phase {phase} is archived (its tasks are done and landed): name an active \
                     phase or a new one"
                ));
            }
            phase.clone()
        }
        None => next_phase(&tasks_dir, &draft.task_id),
    };
    let phase_dir = tasks_dir.join("active").join(&phase);
    let path = phase_dir.join(format!("{}.json", draft.task_id));
    let shown = relative(folder, &path);
    if path.exists() {
        return Err(format!("{shown} exists already: name another task id"));
    }
    if task_id_defined(&tasks_dir, &draft.task_id)
        .map_err(|e| format!("listing {}: {e}", tasks_dir.display()))?
    {
        return Err(format!(
            "task {} is already defined under {} (active or archived): name another task id",
            draft.task_id,
            relative(folder, &tasks_dir)
        ));
    }
    let runs = provider.task_runs(folder, &draft.task_id)?;
    if runs > 0 {
        return Err(format!(
            "the store already holds {runs} run{} of task {}: the loop keys runs by task id, so \
             a new task needs a new id",
            if runs == 1 { "" } else { "s" },
            draft.task_id
        ));
    }
    // The loop's own loader validates the definition; the bytes written
    // are read back by it to the same task.
    let value = task_value(draft);
    let task = load_task_data(&value, &shown).map_err(|e| e.to_string())?;
    let mut contents = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    contents.push('\n');
    let reread = load_task_str(&contents, &shown).map_err(|e| e.to_string())?;
    if reread != task {
        return Err(format!(
            "{shown}: the written form does not read back as the same task"
        ));
    }
    let mut notes = vec![format!(
        "Written as a new uncommitted file in {} (the hub never commits): `fw worker` there \
         reads the working tree; commit it to keep it with the repo.",
        folder.display()
    )];
    let uncovered: Vec<&str> = draft
        .protect
        .iter()
        .map(String::as_str)
        .filter(|p| {
            // A directory is covered when what is under it is.
            let probe = if folder.join(p).is_dir() {
                format!("{p}/x")
            } else {
                (*p).to_owned()
            };
            !protected_paths
                .iter()
                .any(|pattern| protected_path_matches(&probe, pattern))
        })
        .collect();
    if !uncovered.is_empty() {
        notes.push(format!(
            "Protected in the fan-out, written as constraints: {}. The loop refuses a landing \
             that changes a path only when the protected_paths config key lists it (user or \
             personal config, or a trusted .flywheel.toml); add {} there to keep it enforced.",
            uncovered.join(", "),
            if uncovered.len() == 1 { "it" } else { "them" }
        ));
    }
    if git_ignored(folder, &path) == Some(true) {
        notes.push(format!(
            "{shown} is git-ignored here: it stays local to this checkout."
        ));
    }
    if !phase_dir.is_dir() {
        notes.push(format!(
            "A new phase {phase}: archived once its tasks are done and landed."
        ));
    }
    Ok(TaskFilePlan {
        folder: folder.to_path_buf(),
        new_phase: !phase_dir.is_dir(),
        path,
        relative: shown,
        task_id: draft.task_id.clone(),
        phase,
        phases,
        contents,
        notes,
    })
}

/// See `FlywheelProvider::write_task`.
pub fn write_task(
    provider: &HubFlywheel,
    folder: &Path,
    draft: &TaskDraft,
    contents: &str,
) -> Result<TaskWritten, String> {
    // The checks plan_task makes (no such id yet) hold until the file is
    // in place: one write at a time.
    let _writing = provider.task_write_lock();
    let plan = plan_task(provider, folder, draft)?;
    if plan.contents != contents {
        return Err("the task file would differ from the one reviewed: review it again".into());
    }
    let dir = plan
        .path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", plan.path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    // Written aside (a dot file the walk skips), then linked into place:
    // the listing never sees half a file, and an existing file is never
    // replaced.
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = dir.join(format!(
        ".{}.json.hub-{}-{}",
        plan.task_id,
        std::process::id(),
        STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&staging, contents)
        .map_err(|e| format!("writing {}: {e}", staging.display()))?;
    let linked = std::fs::hard_link(&staging, &plan.path);
    let _ = std::fs::remove_file(&staging);
    linked.map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{} exists already: name another task id", plan.relative)
        }
        _ => format!("writing {}: {e}", plan.path.display()),
    })?;
    let task = load_task_file(&plan.path).map_err(|e| e.to_string())?;
    let TaskLayout { tasks_dir, .. } = provider.task_layout(folder)?;
    let items = DirectoryWorkSource::new(&tasks_dir)
        .list_work()
        .map_err(|e| format!("listing {}: {e}", tasks_dir.display()))?;
    let listed = items
        .iter()
        .any(|item| item.task.id == task.id && item.local_path.as_deref() == Some(&plan.path));
    // What else `fw worker` there would drain: it picks over every active
    // task, not this one only.
    let done = provider.done_tasks(folder)?;
    let mut queue: Vec<String> = Vec::new();
    for item in &items {
        let id = &item.task.id;
        if *id != task.id && !done.contains(id) && !queue.contains(id) {
            queue.push(id.clone());
        }
    }
    Ok(TaskWritten {
        path: plan.path,
        relative: plan.relative,
        task_id: plan.task_id,
        phase: plan.phase,
        listed,
        queue,
    })
}

fn bulleted(heading: &str, items: &[String]) -> Option<String> {
    (!items.is_empty()).then(|| {
        let bullets: Vec<String> = items.iter().map(|i| format!("- {i}")).collect();
        format!("## {heading}\n\n{}", bullets.join("\n"))
    })
}

/// A task as the agents' first turn: its goal and context (the loop's
/// prompt headings), then its visible command graders -- never its
/// held-out ones.
pub fn seed_prompt(task: &Task) -> String {
    let mut sections = vec![format!("# Goal\n\n{}", task.goal.trim())];
    let ctx = &task.context;
    let mut blocks: Vec<String> = [
        bulleted("Relevant files", &ctx.relevant),
        bulleted("References", &ctx.references),
        bulleted("Constraints", &ctx.constraints),
        bulleted("Non-goals", &ctx.non_goals),
        bulleted("Edge cases", &ctx.edge_cases),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !ctx.notes.trim().is_empty() {
        blocks.push(format!("## Notes\n\n{}", ctx.notes.trim()));
    }
    if !blocks.is_empty() {
        sections.push(format!("# Context\n\n{}", blocks.join("\n\n")));
    }
    let checks: Vec<String> = task
        .graders
        .iter()
        .filter_map(|g| match g {
            Grader::Command(c) => Some(match &c.name {
                Some(name) => format!("- `{}` ({name})", c.run),
                None => format!("- `{}`", c.run),
            }),
            _ => None,
        })
        .collect();
    if !checks.is_empty() {
        sections.push(format!(
            "# Graders\n\nYour work is verified by these commands, run from the repository \
             root; each must exit 0:\n\n{}",
            checks.join("\n")
        ));
    }
    let mut prompt = sections.join("\n\n");
    prompt.push('\n');
    prompt
}

/// The held-out command graders the gate would run for `task` at
/// landing: its registration plus the standing oracles matching it.
fn held_out_graders(layout: &TaskLayout, task: &Task) -> Result<Vec<GraderSpec>, String> {
    // The project's held-out dir: absent until an oracle is registered
    // (the gate is off until then).
    let Some(source) = &layout.held_out else {
        return Ok(Vec::new());
    };
    let refused = |e: flywheel_orchestrator::held_out_gate::HeldOutGraderError| {
        format!(
            "the held-out registry cannot be read ({e}): a fan-out of task {} would run \
             without its held-out graders",
            task.id
        )
    };
    let mut graders = source
        .graders_for(&task.id)
        .map_err(refused)?
        .unwrap_or_default();
    graders.extend(source.standing_graders_for(task).map_err(refused)?);
    Ok(graders
        .into_iter()
        .map(|g| GraderSpec {
            run: g.run,
            name: g.name,
            held_out: true,
        })
        .collect())
}

/// The `protected_paths` config key as hub protected paths (a path or a
/// directory, compared by content): a literal path, or `dir/**`, maps to
/// itself / `dir`; a pattern with a glob inside a name cannot, and is
/// returned apart (the loop still enforces it at landing).
fn hub_protect(patterns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut paths = Vec::new();
    let mut skipped = Vec::new();
    for pattern in patterns {
        let mut path = pattern.trim().trim_end_matches('/');
        while let Some(dir) = path.strip_suffix("/**") {
            path = dir;
        }
        let literal = !path.is_empty()
            && !path.starts_with('/')
            && path != "**"
            && !path.contains(['*', '?', '['])
            && !path.split('/').any(|p| p == ".." || p.is_empty());
        if literal {
            if !paths.iter().any(|p| p == path) {
                paths.push(path.to_owned());
            }
        } else {
            skipped.push(pattern.clone());
        }
    }
    (paths, skipped)
}

/// See `FlywheelProvider::task_seed`.
pub fn task_seed(provider: &HubFlywheel, folder: &Path, task_id: &str) -> Result<TaskSeed, String> {
    let layout = provider.task_layout(folder)?;
    let files = iter_active_task_files(&layout.tasks_dir)
        .map_err(|e| format!("listing {}: {e}", layout.tasks_dir.display()))?;
    let (path, task): (PathBuf, Task) = files
        .into_iter()
        .filter_map(|path| load_task_file(&path).ok().map(|task| (path, task)))
        .find(|(_, task)| task.id == task_id)
        .ok_or_else(|| {
            format!(
                "no active task {task_id} under {} (an archived task is done)",
                relative(folder, &layout.tasks_dir)
            )
        })?;
    let phase = path
        .parent()
        .filter(|p| p.parent().is_some_and(|a| a.ends_with("active")))
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned());
    let mut graders = Vec::new();
    let mut not_run = Vec::new();
    for grader in &task.graders {
        match grader {
            Grader::Command(c) => graders.push(GraderSpec {
                run: c.run.clone(),
                name: c.name.clone(),
                held_out: false,
            }),
            other => not_run.push(match other.name() {
                Some(name) => format!("{} {name}", other.grader_type()),
                None => other.grader_type().to_string(),
            }),
        }
    }
    let held_out = held_out_graders(&layout, &task)?;
    let (repo_protect, protect_skipped) = hub_protect(&layout.protected_paths);
    let mut protect = repo_protect.clone();
    for path in task
        .context
        .constraints
        .iter()
        .filter_map(|c| protected_in_constraint(c))
    {
        if !protect.iter().any(|p| p == path) {
            protect.push(path.to_owned());
        }
    }
    let ctx = &task.context;
    Ok(TaskSeed {
        folder: folder.to_path_buf(),
        task_id: task.id.clone(),
        phase,
        goal: task.goal.clone(),
        prompt: seed_prompt(&task),
        digest: task_digest(&task),
        graders,
        held_out,
        not_run,
        context: SeedContext {
            relevant: ctx.relevant.clone(),
            references: ctx.references.clone(),
            constraints: ctx.constraints.clone(),
            non_goals: ctx.non_goals.clone(),
            edge_cases: ctx.edge_cases.clone(),
            notes: ctx.notes.clone(),
        },
        tags: task.tags.clone(),
        protect,
        repo_protect,
        protect_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_hub::flywheel::FlywheelProvider;

    /// A real git repo (`repo/`) whose home project (under `home/`) holds
    /// an active `01-first/existing` task and an archived `04-old` phase.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        tasks: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn provider(&self) -> HubFlywheel {
            HubFlywheel::new("/bin/fw").with_home(&self.home)
        }
    }

    fn repo() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let root = root.canonicalize().unwrap();
        let home = dir.path().join("home");
        let project = flywheel_orchestrator::home::resolve_project(
            &flywheel_orchestrator::home::Home::at(&home),
            &root,
        )
        .unwrap();
        let tasks = project.paths.tasks();
        std::fs::create_dir_all(tasks.join("active/01-first")).unwrap();
        std::fs::create_dir_all(tasks.join("archive/04-old")).unwrap();
        std::fs::write(
            tasks.join("active/01-first/existing.json"),
            r#"{"id":"existing","goal":"Existing work.","graders":[{"type":"command","run":"true"}]}"#,
        )
        .unwrap();
        Fixture {
            _dir: dir,
            root,
            home,
            tasks,
            project: project.paths.root().to_path_buf(),
        }
    }

    fn draft(id: &str) -> TaskDraft {
        TaskDraft {
            task_id: id.into(),
            phase: None,
            goal: "Make answer.txt say 42.".into(),
            notes: "The prompt:\n\nwrite 42".into(),
            relevant: vec!["answer.txt".into()],
            non_goals: Vec::new(),
            edge_cases: Vec::new(),
            constraints: vec!["Leave `check.sh` unchanged.".into()],
            references: vec!["Commit abc passed.".into()],
            tags: vec!["hub-fanout".into()],
            graders: vec![
                GraderSpec {
                    run: "test \"$(cat answer.txt)\" = 42".into(),
                    name: Some("answer".into()),
                    held_out: false,
                },
                GraderSpec {
                    run: "sh check.sh".into(),
                    name: None,
                    held_out: false,
                },
            ],
            protect: vec!["check.sh".into()],
        }
    }

    #[test]
    fn a_promotion_writes_a_task_the_loader_and_the_directory_source_read() {
        let f = repo();
        let root = f.root.clone();
        let provider = f.provider();
        let plan = provider.plan_task(&root, &draft("answer-42")).unwrap();
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("check.sh") && n.contains("protected_paths")),
            "{:?}",
            plan.notes
        );
        assert_eq!(
            plan.phase, "05-answer-42",
            "after every active and archived phase"
        );
        assert!(plan.new_phase);
        assert_eq!(plan.phases, vec!["01-first".to_owned()]);
        assert_eq!(
            PathBuf::from(&plan.relative),
            f.tasks.join("active/05-answer-42/answer-42.json"),
            "the task lands in the project's central tasks dir"
        );
        assert!(!root.join(".flywheel").exists());
        let value: Value = serde_json::from_str(&plan.contents).unwrap();
        assert_eq!(value["graders"][0]["type"], "command");
        assert_eq!(value["graders"][1].get("name"), None);
        assert_eq!(
            value["context"]["constraints"][0],
            "Leave `check.sh` unchanged."
        );
        assert_eq!(value["context"]["relevant"][0], "answer.txt");
        assert!(
            value["context"].get("non_goals").is_none(),
            "empty fields are left out"
        );

        let written = provider
            .write_task(&root, &draft("answer-42"), &plan.contents)
            .unwrap();
        assert!(written.listed);
        assert_eq!(
            std::fs::read_to_string(&written.path).unwrap(),
            plan.contents
        );
        let task = load_task_file(&written.path).unwrap();
        assert_eq!(task.graders.len(), 2);
        assert_eq!(
            task.context.references,
            vec!["Commit abc passed.".to_owned()]
        );
        let staged: Vec<_> = std::fs::read_dir(written.path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(staged.len(), 1, "no staging file left: {staged:?}");

        // Never twice, never an id in use, never an id the walk skips.
        let again = provider.plan_task(&root, &draft("answer-42")).unwrap_err();
        assert!(again.contains("already defined"), "{again}");
        let used = provider.plan_task(&root, &draft("existing")).unwrap_err();
        assert!(used.contains("already defined"), "{used}");
        for bad in ["_hidden", "a/b", "..", "has space", ""] {
            assert!(provider.plan_task(&root, &draft(bad)).is_err(), "{bad:?}");
        }
        let mut into = draft("second");
        into.phase = Some("01-first".into());
        let plan = provider.plan_task(&root, &into).unwrap();
        assert!(!plan.new_phase);
        // A write must match what was reviewed.
        let stale = provider.write_task(&root, &into, "{}").unwrap_err();
        assert!(stale.contains("review it again"), "{stale}");
        // An invalid definition is the loader's to refuse.
        let mut empty = draft("third");
        empty.goal = "   ".into();
        assert!(
            provider
                .plan_task(&root, &empty)
                .unwrap_err()
                .contains("goal")
        );
    }

    #[test]
    fn a_seed_carries_command_graders_and_held_out_ones_apart() {
        let f = repo();
        let root = f.root.clone();
        std::fs::write(
            f.tasks.join("active/01-first/answer.json"),
            serde_json::to_string(&json!({
                "id": "answer",
                "goal": "Make answer.txt say 42.",
                "context": {"relevant": ["answer.txt"], "constraints": ["no new files"]},
                "graders": [
                    {"type": "command", "run": "test -f answer.txt", "name": "exists"},
                    {"type": "rubric", "assertions": ["it is tidy"], "name": "tidy"},
                    {"type": "command", "run": "grep -q 42 answer.txt"},
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        let provider = f.provider();
        // The project's held-out dir not created yet: nothing registered.
        let seed = provider.task_seed(&root, "answer").unwrap();
        assert!(seed.held_out.is_empty());
        assert_eq!(seed.phase.as_deref(), Some("01-first"));
        assert_eq!(seed.not_run, vec!["rubric tidy".to_owned()]);
        assert_eq!(
            seed.graders
                .iter()
                .map(|g| g.run.as_str())
                .collect::<Vec<_>>(),
            ["test -f answer.txt", "grep -q 42 answer.txt"]
        );
        assert!(seed.prompt.starts_with("# Goal\n\nMake answer.txt say 42."));
        assert!(seed.prompt.contains("## Relevant files\n\n- answer.txt"));
        assert!(seed.prompt.contains("- `test -f answer.txt` (exists)"));

        let held = f.project.join("held-out");
        std::fs::create_dir_all(&held).unwrap();
        let oracle = held.join("oracle.sh");
        std::fs::write(&oracle, "#!/bin/sh\ngrep -qx 42 answer.txt\n").unwrap();
        std::fs::write(
            held.join("answer.json"),
            serde_json::to_string(&json!({"graders": [
                {"type": "command", "run": format!("sh {}", oracle.display()), "name": "held-out-oracle:oracle.sh"}
            ]}))
            .unwrap(),
        )
        .unwrap();
        let seed = provider.task_seed(&root, "answer").unwrap();
        assert_eq!(seed.held_out.len(), 1);
        assert!(seed.held_out[0].held_out);
        assert!(!seed.prompt.contains("oracle"), "{}", seed.prompt);
        assert_eq!(seed.graders.len(), 2);

        // A broken registration fails closed.
        std::fs::write(held.join("answer.json"), "{not json").unwrap();
        let err = provider.task_seed(&root, "answer").unwrap_err();
        assert!(err.contains("held-out registry cannot be read"), "{err}");
        assert!(
            provider
                .task_seed(&root, "nope")
                .unwrap_err()
                .contains("no active task")
        );
    }

    #[test]
    fn an_archived_phase_is_refused_and_the_queue_is_named() {
        let f = repo();
        let root = f.root.clone();
        let provider = f.provider();
        let mut into = draft("answer-42");
        into.phase = Some("04-old".into());
        let err = provider.plan_task(&root, &into).unwrap_err();
        assert!(err.contains("phase 04-old is archived"), "{err}");

        let plan = provider.plan_task(&root, &draft("answer-42")).unwrap();
        let written = provider
            .write_task(&root, &draft("answer-42"), &plan.contents)
            .unwrap();
        assert_eq!(
            written.queue,
            vec!["existing".to_owned()],
            "the worker drains the other active task too"
        );
    }

    #[test]
    fn concurrent_promotions_never_define_one_id_twice() {
        let f = repo();
        let root = f.root.clone();
        let provider = std::sync::Arc::new(f.provider());
        let drafts: Vec<TaskDraft> = (0..6)
            .map(|i| {
                let mut d = draft("answer");
                d.phase = Some(format!("0{}-p{i}", i + 5));
                d
            })
            .collect();
        let plans: Vec<String> = drafts
            .iter()
            .map(|d| provider.plan_task(&root, d).unwrap().contents)
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(drafts.len()));
        let handles: Vec<_> = drafts
            .into_iter()
            .zip(plans)
            .map(|(d, contents)| {
                let provider = std::sync::Arc::clone(&provider);
                let barrier = std::sync::Arc::clone(&barrier);
                let root = root.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    provider.write_task(&root, &d, &contents)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        for err in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(err.contains("already defined"), "{err}");
        }
    }

    #[test]
    fn a_seed_holds_candidates_to_the_repo_protected_paths_and_its_constraints() {
        assert_eq!(
            hub_protect(&[
                ".github/**".into(),
                "flywheel.toml".into(),
                "*.lock".into(),
                "src/**/*.rs".into(),
                "**".into(),
            ]),
            (
                vec![".github".to_owned(), "flywheel.toml".to_owned()],
                vec![
                    "*.lock".to_owned(),
                    "src/**/*.rs".to_owned(),
                    "**".to_owned()
                ]
            )
        );
        let f = repo();
        let root = f.root.clone();
        // The protected_paths config key (here the user config).
        std::fs::write(
            f.home.join("config.toml"),
            "protected_paths = [\".github/**\", \"flywheel.toml\", \"*.lock\"]\n",
        )
        .unwrap();
        std::fs::write(
            f.tasks.join("active/01-first/answer.json"),
            serde_json::to_string(&json!({
                "id": "answer",
                "goal": "Make answer.txt say 42.",
                "tags": ["core"],
                "context": {
                    "relevant": ["answer.txt"],
                    "constraints": [flywheel_hub::fanout::promote::constraint_line("README.md"), "no new files"],
                    "edge_cases": ["an empty file"],
                },
                "graders": [{"type": "rubric", "assertions": ["friendly"], "name": "friendly"}],
            }))
            .unwrap(),
        )
        .unwrap();
        let provider = f.provider();
        let seed = provider.task_seed(&root, "answer").unwrap();
        assert_eq!(seed.protect, [".github", "flywheel.toml", "README.md"]);
        assert_eq!(seed.repo_protect, [".github", "flywheel.toml"]);
        assert_eq!(seed.protect_skipped, ["*.lock"]);
        assert_eq!(seed.context.relevant, ["answer.txt"]);
        assert_eq!(seed.context.edge_cases, ["an empty file"]);
        assert_eq!(seed.tags, ["core"]);
        assert_eq!(seed.not_run, ["rubric friendly"]);
    }
}
