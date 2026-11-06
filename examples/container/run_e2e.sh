#!/usr/bin/env bash
# Live end-to-end run of the flywheel container sandbox backend (spec 00044).
#
# Runs one real task with the agent executing INSIDE a Docker container against
# a bind-mounted worktree, authenticated with your Claude subscription session
# (no API key). Proves the whole path: container start -> agent CLI runs
# headless in the container -> stream-json parsed into the loop envelope ->
# host-side grader sees the agent's edits -> DONE -> landing -> teardown.
#
# It builds a throwaway git repo under $FW_E2E_DIR (default /tmp/fw-e2e/repo)
# with its own flywheel home ($FW_E2E_DIR/home), seeds one task into the
# project's tasks dir, and drains it with
# `fw worker --once --sandbox container --image $FW_IMAGE`.
#
# Prerequisites
#   1. Build the agent image (from examples/container/), matching your uid/gid:
#        docker build -t flywheel-agent:latest \
#          --build-arg AGENT_UID=$(id -u) --build-arg AGENT_GID=$(id -g) .
#   2. Mint a subscription OAuth token (printed once, not saved) and export it:
#        claude setup-token
#        export CLAUDE_CODE_OAUTH_TOKEN="<paste>"
#      Do NOT also set ANTHROPIC_API_KEY: the CLI prefers it over the
#      subscription, and a subscription auth mode refuses to run beside it.
#      Fallbacks, in order: an existing `claude /login` (~/.claude session),
#      then ANTHROPIC_API_KEY.
#   3. Run from anywhere:
#        examples/container/run_e2e.sh
#
# Success looks like
#   - the script ends with "[run_e2e] PASS" and exits 0;
#   - DONE.txt is committed on the scratch repo's main branch, and the repo
#     tree holds nothing else (state lives under the scratch home);
#   - `docker ps -a | grep flywheel-hello` shows nothing (container torn down).
#
# Environment
#   FW_IMAGE      agent image          (default flywheel-agent:latest)
#   FW_MODEL      agent model          (default claude-sonnet-5-5)
#   FW_E2E_DIR    scratch root         (default /tmp/fw-e2e; wiped each run)
#   FLYWHEEL_BIN  flywheel binary      (default: `flywheel` on PATH, else a
#                                       release build of this checkout)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${FW_IMAGE:-flywheel-agent:latest}"
MODEL="${FW_MODEL:-claude-sonnet-5-5}"
BASE="${FW_E2E_DIR:-/tmp/fw-e2e}"
SCRATCH="$BASE/repo"
TASK_ID="hello"

log() { printf '[run_e2e] %s\n' "$*"; }
die() { printf '[run_e2e] FAIL: %s\n' "$*" >&2; exit 1; }

resolve_binary() {
    if [[ -n "${FLYWHEEL_BIN:-}" ]]; then
        printf '%s\n' "$FLYWHEEL_BIN"
    elif command -v flywheel >/dev/null 2>&1; then
        command -v flywheel
    else
        log "building flywheel (release) from $REPO_ROOT" >&2
        cargo build --quiet --release -p flywheel --manifest-path "$REPO_ROOT/Cargo.toml"
        printf '%s\n' "${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/flywheel"
    fi
}

# Name the auth mode the container backend will detect (it prefers a
# subscription token, then a ~/.claude session, then an API key).
resolve_auth() {
    if [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
        echo oauth
    elif [[ -f "$HOME/.claude/.credentials.json" ]]; then
        log "CLAUDE_CODE_OAUTH_TOKEN unset; using ~/.claude session" >&2
        echo session
    elif [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
        log "using ANTHROPIC_API_KEY (no subscription token found)" >&2
        echo api_key
    else
        die "no Claude auth found. Run \`claude setup-token\` and export CLAUDE_CODE_OAUTH_TOKEN (see this file's header)."
    fi
}

docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || die "image $IMAGE not found; build it first (see this file's header)."

BIN="$(resolve_binary)"
[[ -x "$BIN" ]] || die "flywheel binary not executable: $BIN"
AUTH="$(resolve_auth)"

rm -rf "$BASE"
mkdir -p "$SCRATCH"
export FLYWHEEL_HOME="$BASE/home"
cd "$SCRATCH"
git init --quiet --initial-branch=main
git config user.name "flywheel e2e"
git config user.email "e2e@flywheel.invalid"
echo "# e2e" > README.md
git add -A
git commit --quiet -m "chore: seed container e2e scratch repo"

# The agent model, from the user config (the container backend needs one).
mkdir -p "$FLYWHEEL_HOME"
printf 'model = "%s"\n' "$MODEL" > "$FLYWHEEL_HOME/config.toml"

TASKS="$("$BIN" project path --tasks)"
mkdir -p "$TASKS/active/01-phase"
cat > "$TASKS/active/01-phase/$TASK_ID.json" <<EOF
{
  "id": "$TASK_ID",
  "goal": "Create a file named DONE.txt containing the text: ok. Commit it with the message 'chore: add DONE.txt'.",
  "graders": [{"type": "command", "run": "test -f DONE.txt"}]
}
EOF

log "image=$IMAGE model=$MODEL auth=$AUTH bin=$BIN"
log "scratch repo: $SCRATCH"
status=0
"$BIN" worker --once --sandbox container --image "$IMAGE" --max-turns 8 --max-retries 0 \
    || status=$?
log "worker exited $status"

"$BIN" status || true

leftover="$(docker ps -a --filter "name=flywheel-$TASK_ID-" --format '{{.Names}}')"
[[ -z "$leftover" ]] || die "container not torn down: $leftover"
git cat-file -e "main:DONE.txt" 2>/dev/null \
    || die "DONE.txt did not land on main (see \`$BIN show $TASK_ID\` in $SCRATCH)"
[[ "$status" -eq 0 ]] || die "worker exited $status"
[[ -z "$(git status --porcelain --ignored)" ]] || die "the run wrote into the repo tree"
log "DONE.txt on main, repo tree clean, container torn down"
log "PASS"
