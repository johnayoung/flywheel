"""Live end-to-end run of the flywheel container sandbox backend (spec 00044).

Runs one real task with the agent executing INSIDE a Docker container against a
bind-mounted directory, authenticated with your Claude subscription session (no
API key). Proves the whole novel path: container start -> agent CLI runs headless
in the container -> stream-json parsed into the loop envelope -> host-side grader
sees the agent's edits -> DONE -> teardown.

This is the manual "driver" (there is no `flywheel worker --container` yet). It
delegates landing to a no-op inner strategy, so it skips the git merge; for the
full landing variant, swap PlainInner for flywheel_worktree's merge submitter.

--------------------------------------------------------------------------------
Prerequisites
--------------------------------------------------------------------------------
1. Build the agent image (from examples/container/), matching your uid/gid:

     docker build -t flywheel-agent:latest \
       --build-arg AGENT_UID=$(id -u) --build-arg AGENT_GID=$(id -g) .

2. Mint a subscription OAuth token from your Claude Pro/Max login (prints a
   ~1-year token; it is NOT saved, so copy it), then export it:

     claude setup-token
     export CLAUDE_CODE_OAUTH_TOKEN="<paste>"

   (Do NOT also set ANTHROPIC_API_KEY — the CLI prefers it and would ignore the
   subscription; ClaudeAuth.oauth_token enforces this and will raise if it sees
   one in the container env.)

3. Run from the repo root so the workspace venv is used:

     uv run python examples/container/run_e2e.py

--------------------------------------------------------------------------------
Success looks like
--------------------------------------------------------------------------------
- prints  [('hello', <Status.DONE: ...>)]
- /tmp/fw-e2e/work/hello/DONE.txt exists on the host, owned by you
    (stat -c '%U %n' /tmp/fw-e2e/work/hello/DONE.txt)
- `docker ps -a | grep flywheel-hello` shows nothing (container torn down)

Override the image / model via FW_IMAGE / FW_MODEL env vars.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from pathlib import Path

from flywheel_container import ClaudeAuth, ClaudeCliAgent, ContainerSubmitStrategy
from flywheel_orchestrator import SandboxRequest, SubmitRequest, orchestrate

IMAGE = os.environ.get("FW_IMAGE", "flywheel-agent:latest")
MODEL = os.environ.get("FW_MODEL", "claude-sonnet-4-6")
BASE = Path(os.environ.get("FW_E2E_DIR", "/tmp/fw-e2e"))


class PlainInner:
    """A minimal inner strategy: provision a plain directory per task, no git
    landing. The container bind-mounts this dir; the agent writes through it to
    the host, where the command grader then runs."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def prepare_sandbox(self, request: SandboxRequest) -> Path:
        d = self.root / request.task_id
        d.mkdir(parents=True, exist_ok=True)
        return d

    def submit(self, request: SubmitRequest) -> None:
        return None


def _write_task() -> None:
    phase = BASE / "tasks" / "active" / "01-phase"
    phase.mkdir(parents=True, exist_ok=True)
    (phase / "hello.json").write_text(
        json.dumps(
            {
                "id": "hello",
                "goal": "Create a file named DONE.txt containing the text: ok",
                "graders": [{"type": "command", "run": "test -f DONE.txt"}],
            }
        )
    )


def _auth() -> ClaudeAuth:
    token = os.environ.get("CLAUDE_CODE_OAUTH_TOKEN")
    if token:
        return ClaudeAuth.oauth_token(token)
    # Fallbacks: an existing `claude /login` on this host, or a raw key.
    if Path("~/.claude/.credentials.json").expanduser().exists():
        print("[run_e2e] CLAUDE_CODE_OAUTH_TOKEN unset; using ~/.claude session")
        return ClaudeAuth.session()
    if os.environ.get("ANTHROPIC_API_KEY"):
        print("[run_e2e] using ANTHROPIC_API_KEY (no subscription token found)")
        return ClaudeAuth.api_key(os.environ["ANTHROPIC_API_KEY"])
    sys.exit(
        "No Claude auth found. Run `claude setup-token` and export "
        "CLAUDE_CODE_OAUTH_TOKEN (see this file's header)."
    )


def main() -> int:
    _write_task()
    strategy = ContainerSubmitStrategy(
        PlainInner(BASE / "work"),
        image=IMAGE,
        agent=ClaudeCliAgent(model=MODEL),
        auth=_auth(),
        exec_timeout=600,  # bound the in-container agent (host hang-watchdog
        # cannot see container liveness; this is the bound)
    )
    report = asyncio.run(
        orchestrate(
            tasks_dir=BASE / "tasks",
            db_path=BASE / "flywheel.sqlite",
            sandbox_root=BASE / "sandboxes",
            max_retries=0,
            max_turns=8,
            stream=sys.stdout,
            strategy=strategy,  # invoke stays None — the container supplies it
        )
    )
    results = [(r.task_id, r.status) for r in report.runs]
    print(results)
    produced = BASE / "work" / "hello" / "DONE.txt"
    print(f"[run_e2e] {produced} exists: {produced.exists()}")
    return 0 if results and results[0][1].name == "DONE" else 1


if __name__ == "__main__":
    raise SystemExit(main())
