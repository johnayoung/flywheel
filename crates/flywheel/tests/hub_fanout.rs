//! Verified fan-out end to end through the real `fw` binary: a detached
//! hub on 127.0.0.1:0, a real git repo, scripted `claude` / `codex`
//! stand-ins on PATH whose model picks what they do (commit a passing or
//! failing answer, a larger diff, never finish, fix on request), and real
//! grader commands run by flywheel-core's runner in each worktree. Then
//! the board's ranking, ask-to-fix, a grading cut short by `fw hub stop`
//! running again after `fw hub start`, a rebase offer when the base moved,
//! the winner's merge, and loser cleanup refused on unique work until
//! forced.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    repo: PathBuf,
}

const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Fan Out"),
    ("GIT_AUTHOR_EMAIL", "fanout@example.invalid"),
    ("GIT_COMMITTER_NAME", "Fan Out"),
    ("GIT_COMMITTER_EMAIL", "fanout@example.invalid"),
];

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = root.join("state");
        let repo = root.join("code").join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        let env = Self {
            _dir: dir,
            root,
            state,
            repo,
        };
        env.git(&env.repo, &["init", "-q", "-b", "main"]);
        std::fs::write(env.repo.join("README.md"), "demo\n").unwrap();
        env.git(&env.repo, &["add", "-A"]);
        env.git(&env.repo, &["commit", "-qm", "initial"]);
        env
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(IDENTITY)
            .env("HOME", &self.root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut argv: Vec<&str> = vec!["hub", args[0]];
        let mut rest = args[1..].to_vec();
        if args[0] == "fanout" {
            // `fw hub fanout VERB --state-dir DIR ...`
            argv.push(rest.remove(0));
        }
        argv.extend(["--state-dir", self.state.to_str().unwrap()]);
        argv.extend(rest);
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("FW_BIN", env!("CARGO_BIN_EXE_fw"))
            .env("FANOUT_ROOT", &self.root)
            .envs(IDENTITY)
            .env_remove("XDG_STATE_HOME")
            .env_remove("FW_HUB_TOKEN");
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.fw(args);
        assert!(
            output.status.success(),
            "fw hub {args:?}: {:?}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn board(&self, id: &str) -> Value {
        serde_json::from_str(&self.ok(&["fanout", "show", id, "--json"])).unwrap()
    }

    /// Poll the board until `done` holds for it.
    fn board_until(&self, id: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let board = self.board(id);
            if done(&board) {
                return board;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                serde_json::to_string_pretty(&board).unwrap()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("hub.log")).unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

fn candidate<'a>(board: &'a Value, label: &str) -> &'a Value {
    board["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == label)
        .unwrap_or_else(|| panic!("no candidate {label}"))
}

fn order(board: &Value) -> Vec<String> {
    board["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["label"].as_str().unwrap().to_owned())
        .collect()
}

fn why(board: &Value, label: &str) -> String {
    candidate(board, label)["why"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn graded(board: &Value, label: &str, gradings: u64) -> bool {
    let c = candidate(board, label);
    c["gradings"].as_u64() >= Some(gradings) && c["pending"].is_null()
}

/// `claude` and `codex` stand-ins. `--model` picks the behaviour:
/// `pass` commits answer 42, `fail` commits 41, `bigpass` commits 42 plus
/// a 30-line file, `hang` starts working and never finishes (leaving an
/// untracked file), `fixable` commits 41 and, once a prompt asking it to
/// fix arrives, commits 42; `sidebranch` commits 42 on a branch of its
/// own, `fifo` commits 42 and leaves an untracked symlink to a FIFO,
/// `noop` changes nothing, `skiphide` commits 41 and rewrites the tracked
/// `check.sh` to pass behind `--skip-worktree`. A turn ends with the hook report the real CLI
/// would send (claude `Stop`, codex `notify`); a resume does nothing.
fn fake_agents(env: &Env) {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let agent = r#"#!/bin/sh
kind=$(basename "$0")
model=""; resume=""; sid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --model|-m) model="$2"; shift 2 ;;
    --resume) resume=1; shift 2 ;;
    resume) resume=1; shift ;;
    --session-id) sid="$2"; shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
if [ -n "$sid" ]; then
  dir="$CLAUDE_CONFIG_DIR/projects/$(printf %s "$PWD" | sed 's/[^A-Za-z0-9]/-/g')"
  mkdir -p "$dir" && echo '{}' > "$dir/$sid.jsonl"
fi
echo "$kind ready ($model)"
if [ -n "$resume" ]; then exec cat; fi
tid=0199a213-81c0-7800-8aa1-$(printf %s "$PWD" | cksum | cut -c1-12)
working() {
  if [ "$kind" = claude ]; then "$FW_BIN" hub report --source claude --event UserPromptSubmit; fi
}
finished() {
  if [ "$kind" = claude ]; then
    "$FW_BIN" hub report --source claude --event Stop
  else
    "$FW_BIN" hub report --source codex --event notify "{\"type\":\"agent-turn-complete\",\"thread-id\":\"$tid\"}"
  fi
}
answer() {
  echo "$1" > answer.txt
  if [ -n "$2" ]; then seq 1 "$2" > extra.txt; fi
  git add -A && git commit -qm "answer $1"
}
case "$model" in
  pass) working; answer 42; finished ;;
  fail) working; answer 41; finished ;;
  bigpass) working; answer 42 30; finished ;;
  hang) working; echo wip > wip.txt ;;
  sidebranch) working; git switch -q -c "alt-$$"; answer 42; finished ;;
  fifo)
    working; answer 42
    mkfifo "$FANOUT_ROOT/fifo-$$" && ln -s "$FANOUT_ROOT/fifo-$$" pipe
    finished ;;
  noop) working; finished ;;
  skiphide)
    working; answer 41
    git update-index --skip-worktree check.sh && echo 'exit 0' > check.sh
    finished ;;
  fixable)
    working; answer 41; finished
    while IFS= read -r line; do
      case "$line" in
        *"Fix the code"*) working; answer 42; finished ;;
      esac
    done ;;
esac
exec cat
"#;
    for name in ["claude", "codex"] {
        script::write_script(&bin.join(name), agent);
    }
    // A held-out style grader outside the repo: agents never see it. It
    // stalls while `slow` exists (to be caught mid-run by a stop).
    script::write_script(
        &env.root.join("check-answer.sh"),
        "#!/bin/sh\nwhile [ -e \"$FANOUT_ROOT/slow\" ]; do sleep 0.1; done\n\
         a=$(cat answer.txt 2>/dev/null); echo \"got $a\"; test \"$a\" = 42\n",
    );
}

#[test]
fn fan_out_grades_ranks_fixes_survives_restart_and_merges_the_winner() {
    let env = Env::new();
    fake_agents(&env);
    env.ok(&["start", "--port", "0"]);
    // A workspace over the repo (and a shell in it).
    let repo = env.repo.to_str().unwrap().to_owned();
    env.ok(&["new", "demo", "--create", "--folder", &repo, "--name", "sh"]);
    let check = env.root.join("check-answer.sh");
    let check_grader = format!("answer={}", check.display());

    // Refused before anything starts: one agent, no grader.
    let one = env.fw(&[
        "fanout", "new", "demo", "--prompt", "x", "--agent", "claude", "--grader", "true",
    ]);
    assert!(!one.status.success());
    assert!(String::from_utf8_lossy(&one.stderr).contains("2 to 6"));

    let id = env
        .ok(&[
            "fanout",
            "new",
            "demo",
            "--name",
            "answer",
            "--prompt",
            "Write the answer to answer.txt",
            "--agent",
            "claude:pass",
            "--agent",
            "claude:fail",
            "--agent",
            "claude:hang",
            "--agent",
            "codex:bigpass",
            "--agent",
            "claude:fixable",
            "--grader",
            "exists=test -f answer.txt",
            "--grader",
            &check_grader,
            "--timeout",
            "60",
        ])
        .trim()
        .to_owned();
    assert_eq!(id.len(), 32, "{id}");

    // Every candidate got its own sibling worktree cut from one commit.
    let base = env.git(&env.repo, &["rev-parse", "main"]);
    let board = env.board(&id);
    assert_eq!(board["base"], "main");
    assert_eq!(board["base_sha"], base.as_str());
    assert!(
        board["sent_prompt"]
            .as_str()
            .unwrap()
            .contains("commit all of your changes")
    );
    assert!(
        !board["sent_prompt"]
            .as_str()
            .unwrap()
            .contains("check-answer"),
        "agents are never shown the graders"
    );
    for c in board["candidates"].as_array().unwrap() {
        let path = PathBuf::from(c["path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), env.root.join("code/demo.worktrees"));
        assert!(path.is_dir());
    }

    // The four that finish are graded by the hub; the hung one never is.
    let board = env.board_until(&id, "four gradings", |b| {
        ["c1", "c2", "c4", "c5"].iter().all(|l| graded(b, l, 1))
    });
    assert_eq!(order(&board), ["c1", "c4", "c2", "c5", "c3"]);
    let c1 = candidate(&board, "c1");
    assert_eq!(c1["verdict"], "pass");
    assert_eq!(c1["rank"], 1);
    assert_eq!(c1["pickable"], true);
    assert_eq!(c1["grade"]["diff"]["insertions"], 1);
    assert!(why(&board, "c1").contains("passes all 2 graders"));
    assert!(why(&board, "c1").contains("ahead of c4: smaller diff (1 vs 31 lines)"));
    assert!(why(&board, "c4").contains("below c1: larger diff (31 vs 1 lines)"));
    assert_eq!(candidate(&board, "c4")["kind"], "codex");
    let c2 = candidate(&board, "c2");
    assert_eq!(c2["verdict"], "fail");
    assert_eq!(c2["grade"]["graders"][0]["status"], "passed");
    assert_eq!(c2["grade"]["graders"][1]["status"], "failed");
    assert_eq!(c2["grade"]["graders"][1]["exit_code"], 1);
    assert_eq!(
        c2["grade"]["graders"][1]["stdout"].as_str().unwrap().trim(),
        "got 41"
    );
    assert!(why(&board, "c2").contains("fails `answer` (exit 1)"));
    let c3 = candidate(&board, "c3");
    assert_eq!(c3["verdict"], "none");
    assert_eq!(c3["phase"], "working");
    assert!(why(&board, "c3").contains("still working"));
    assert_eq!(board["leader"], "c1");
    assert_eq!(board["settled"], false, "c3 is still at work");
    let shown = env.ok(&["fanout", "show", &id]);
    assert!(shown.contains("#1 c1"), "{shown}");
    assert!(shown.contains("fails `answer` (exit 1)"), "{shown}");

    // A failing candidate cannot be picked.
    let refused = env.fw(&["fanout", "pick", &id, "c2", "--yes"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not pass"));

    // Ask c5 to fix: the grader's output goes to its agent, it commits a
    // fix, finishes, and is graded again by the hub.
    let fix = env.ok(&["fanout", "fix", &id, "c5", "--yes"]);
    assert!(fix.contains("got 41"), "{fix}");
    let board = env.board_until(&id, "c5 graded again", |b| {
        graded(b, "c5", 2) && candidate(b, "c5")["verdict"] == "pass"
    });
    assert_eq!(candidate(&board, "c5")["fixes"], 1);
    assert_eq!(order(&board), ["c1", "c5", "c4", "c2", "c3"]);
    assert!(why(&board, "c5").contains("below c1: same diff and cost (launch order)"));

    // A grading cut short by a stop runs again on the next start.
    std::fs::write(env.root.join("slow"), "").unwrap();
    env.ok(&["fanout", "grade", &id, "c4"]);
    env.board_until(&id, "c4 grading", |b| {
        candidate(b, "c4")["phase"] == "grading"
    });
    env.ok(&["stop"]);
    std::fs::remove_file(env.root.join("slow")).unwrap();
    env.ok(&["start", "--port", "0"]);
    let board = env.board_until(&id, "c4 graded after the restart", |b| graded(b, "c4", 2));
    assert_eq!(candidate(&board, "c4")["verdict"], "pass");
    assert!(env.log().contains("grading again"), "{}", env.log());
    // Everything else came back as it was.
    assert_eq!(order(&board), ["c1", "c5", "c4", "c2", "c3"]);
    assert_eq!(candidate(&board, "c2")["verdict"], "fail");
    assert_eq!(candidate(&board, "c3")["verdict"], "none");

    // The base moved: picking offers a rebase, which grades c1 again.
    std::fs::write(env.repo.join("other.txt"), "elsewhere\n").unwrap();
    env.git(&env.repo, &["add", "other.txt"]);
    env.git(&env.repo, &["commit", "-qm", "base moves on"]);
    let needs = env.fw(&["fanout", "pick", &id, "c1", "--yes"]);
    assert!(!needs.status.success());
    assert!(String::from_utf8_lossy(&needs.stderr).contains("--rebase"));
    env.ok(&["fanout", "pick", &id, "c1", "--rebase", "--yes"]);
    let board = env.board_until(&id, "c1 graded after its rebase", |b| {
        graded(b, "c1", 2) && candidate(b, "c1")["verdict"] == "pass"
    });
    assert_eq!(
        candidate(&board, "c1")["grade"]["diff"]["insertions"],
        1,
        "its own work only, not the base's new commit"
    );

    // Pick the winner: the base fast-forwards to exactly what passed.
    let graded_head = candidate(&board, "c1")["grade"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    let picked = env.ok(&["fanout", "pick", &id, "c1", "--yes"]);
    assert!(picked.contains("cleanup"), "{picked}");
    assert_eq!(env.git(&env.repo, &["rev-parse", "main"]), graded_head);
    assert_eq!(
        std::fs::read_to_string(env.repo.join("answer.txt")).unwrap(),
        "42\n"
    );
    let board = env.board(&id);
    assert_eq!(board["winner"]["label"], "c1");
    assert_eq!(candidate(&board, "c1")["winner"], true);
    let again = env.fw(&["fanout", "pick", &id, "c5", "--yes"]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("picked already"));

    // Stop the hung agent, keeping its worktree.
    env.ok(&["fanout", "cleanup", &id, "c3", "--stop-only", "--yes"]);
    // Its agent ended without a finished turn: the hub grades what it left.
    let board = env.board_until(&id, "c3 graded once it ended", |b| {
        candidate(b, "c3")["session_state"] == "exited" && graded(b, "c3", 1)
    });
    let c3 = candidate(&board, "c3");
    assert_eq!(c3["grade"]["trigger"], "exit");
    assert_eq!(c3["verdict"], "fail");
    assert!(
        why(&board, "c3").contains("fails `exists` (exit 1)"),
        "{}",
        why(&board, "c3")
    );

    // Cleanup refuses worktrees holding unique work...
    let refused = env.fw(&["fanout", "cleanup", &id, "--yes", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&refused.stdout).unwrap();
    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "every loser: {report}");
    for item in items {
        assert_eq!(item["done"], false, "{item}");
        assert!(item["refusal"].as_str().is_some(), "{item}");
        assert!(Path::new(item["path"].as_str().unwrap()).is_dir());
    }
    let c3 = items.iter().find(|i| i["candidate"] == "c3").unwrap();
    assert!(
        c3["refusal"].as_str().unwrap().contains("untracked"),
        "{c3}"
    );
    let c2 = items.iter().find(|i| i["candidate"] == "c2").unwrap();
    assert!(c2["refusal"].as_str().unwrap().contains("not in"), "{c2}");
    // ...until forced: every loser's worktree and session go.
    env.ok(&["fanout", "cleanup", &id, "--force", "--yes"]);
    let board = env.board(&id);
    for label in ["c2", "c3", "c4", "c5"] {
        let c = candidate(&board, label);
        assert_eq!(c["phase"], "removed", "{c}");
        assert!(c["session"].is_null());
        assert!(!Path::new(c["path"].as_str().unwrap()).exists());
    }
    // The winner's worktree is merged: removable without force.
    env.ok(&["fanout", "cleanup", &id, "c1", "--yes"]);
    assert_eq!(candidate(&env.board(&id), "c1")["phase"], "removed");
    assert_eq!(env.board(&id)["settled"], true);

    // Forget it: the record goes, the merged work stays.
    env.ok(&["fanout", "rm", "answer"]);
    let listed: Value = serde_json::from_str(&env.ok(&["fanout", "ls", "--json"])).unwrap();
    assert!(listed["fanouts"].as_array().unwrap().is_empty());
    assert_eq!(env.git(&env.repo, &["rev-parse", "main"]), graded_head);
}

/// What the hub must not be talked into: merging a commit that was not
/// graded, grading through files hidden from git status or a grader script
/// changed after launch, ranking a no-op first, hanging on a FIFO, or
/// grading a worktree it is removing.
#[test]
fn fan_out_lands_only_what_was_graded() {
    let env = Env::new();
    fake_agents(&env);
    env.ok(&["start", "--port", "0"]);
    let repo = env.repo.to_str().unwrap().to_owned();
    env.ok(&["new", "demo", "--create", "--folder", &repo, "--name", "sh"]);
    let check = env.root.join("check-answer.sh");
    let check_grader = format!("answer={}", check.display());
    let new = |name: &str, agents: &[&str], graders: &[&str]| -> String {
        let mut args = vec![
            "fanout", "new", "demo", "--name", name, "--prompt", "Answer",
        ];
        for a in agents {
            args.extend(["--agent", a]);
        }
        for g in graders {
            args.extend(["--grader", g]);
        }
        args.extend(["--timeout", "60"]);
        env.ok(&args).trim().to_owned()
    };

    let id = new(
        "guard",
        &[
            "claude:sidebranch",
            "claude:fifo",
            "claude:hang",
            "claude:pass",
        ],
        &[&check_grader],
    );
    let board = env.board(&id);
    assert!(
        board["candidates"][0]["session"]
            .as_str()
            .unwrap()
            .starts_with("c1-"),
        "the label leads the session name: {board}"
    );
    let board = env.board_until(&id, "three gradings", |b| {
        ["c1", "c2", "c4"].iter().all(|l| graded(b, l, 1))
    });
    // The FIFO did not hang the grading; its symlink is untracked work.
    assert_eq!(candidate(&board, "c2")["verdict"], "pass");
    assert_eq!(candidate(&board, "c2")["pickable"], false);
    // c1's passing commit is on a branch of its own: its fan-out branch
    // (what a merge lands) was never graded.
    assert_eq!(candidate(&board, "c1")["verdict"], "pass");
    let refused = env.fw(&["fanout", "pick", &id, "c1", "--yes"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("not its fan-out branch"), "{stderr}");

    // The pinned out-of-repo grader changes: grading c4 again fails it,
    // and picking is refused; restored, it passes and lands.
    let original = std::fs::read_to_string(&check).unwrap();
    std::fs::write(&check, "#!/bin/sh\nexit 0\n").unwrap();
    env.ok(&["fanout", "grade", &id, "c4"]);
    let board = env.board_until(&id, "c4 graded with the changed script", |b| {
        graded(b, "c4", 2)
    });
    let c4 = candidate(&board, "c4");
    assert_eq!(c4["verdict"], "fail", "{c4}");
    assert!(
        c4["grade"]["error"]
            .as_str()
            .unwrap()
            .contains("changed (or went) since the fan-out started"),
        "{c4}"
    );
    std::fs::write(&check, original).unwrap();
    env.ok(&["fanout", "grade", &id, "c4"]);
    let board = env.board_until(&id, "c4 passes again", |b| {
        graded(b, "c4", 3) && candidate(b, "c4")["verdict"] == "pass"
    });
    let head = candidate(&board, "c4")["grade"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    env.ok(&["fanout", "pick", &id, "c4", "--yes"]);
    assert_eq!(env.git(&env.repo, &["rev-parse", "main"]), head);

    // Removing the never-finished c3 hangs up its agent without grading it.
    env.ok(&["fanout", "cleanup", &id, "c3", "--force", "--yes"]);
    std::thread::sleep(Duration::from_secs(2));
    let c3 = candidate(&env.board(&id), "c3").clone();
    assert_eq!(c3["phase"], "removed", "{c3}");
    assert!(c3["grade"].is_null(), "{c3}");
    assert_eq!(c3["gradings"], 0, "{c3}");

    // A grader that passes on the base cannot tell candidates apart: the
    // no-op ranks last, is not pickable, and the board says why.
    let id = new(
        "noop",
        &["claude:noop", "claude:bigpass"],
        &["readme=test -f README.md"],
    );
    let board = env.board_until(&id, "both graded", |b| {
        graded(b, "c1", 1) && graded(b, "c2", 1)
    });
    assert_eq!(order(&board), ["c2", "c1"]);
    assert_eq!(candidate(&board, "c1")["verdict"], "pass");
    assert_eq!(candidate(&board, "c1")["pickable"], false);
    assert!(why(&board, "c1").contains("nothing to merge"), "{board}");
    assert!(
        board["warnings"][0]
            .as_str()
            .unwrap()
            .contains("pass on main unchanged"),
        "{board}"
    );
    let refused = env.fw(&["fanout", "pick", &id, "c1", "--yes"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("changes nothing"));
    // The list is summaries: no grader output.
    let listed: Value = serde_json::from_str(&env.ok(&["fanout", "ls", "--json"])).unwrap();
    assert!(
        listed["fanouts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["summary"] == true)
    );

    // A grader script in the repo is protected; hiding an edit to it with
    // --skip-worktree fails the grading instead of passing it.
    std::fs::write(
        env.repo.join("check.sh"),
        "test \"$(cat answer.txt 2>/dev/null)\" = 42\n",
    )
    .unwrap();
    env.git(&env.repo, &["add", "check.sh"]);
    env.git(&env.repo, &["commit", "-qm", "a check"]);
    let id = new(
        "hidden",
        &["claude:skiphide", "claude:pass"],
        &["sh ./check.sh"],
    );
    let board = env.board_until(&id, "both graded", |b| {
        graded(b, "c1", 1) && graded(b, "c2", 1)
    });
    assert_eq!(board["protect"], serde_json::json!(["check.sh"]));
    let c1 = candidate(&board, "c1");
    assert_eq!(c1["verdict"], "fail", "{c1}");
    assert!(
        c1["grade"]["error"]
            .as_str()
            .unwrap()
            .contains("hidden from git status"),
        "{c1}"
    );
    assert_eq!(candidate(&board, "c2")["verdict"], "pass");
}
