# Agent image for the flywheel container sandbox backend (spec 00044).
#
# Contract: the container stays alive (`sleep infinity`) so the orchestrator can
# `docker exec` the agent CLI into it; the worktree is bind-mounted at
# /home/agent/workspace and the agent (the `claude` CLI) runs there in headless
# stream-json mode. UID/GID are build args defaulted to the host user so
# bind-mounted files come back host-owned (the strategy pre-flights this).
#
# Build (from this directory), matching your host uid/gid:
#   docker build -t flywheel-agent:latest \
#     --build-arg AGENT_UID=$(id -u) --build-arg AGENT_GID=$(id -g) .
#
# For a task whose graders run the project's tooling (e.g. `uv run pytest`), add
# Python 3.13 + uv + the project deps to this image. The bundled run_e2e.py uses
# a trivial `test -f` grader, so this minimal image is enough for it.

FROM node:22-bookworm

RUN apt-get update && apt-get install -y git curl jq \
  && rm -rf /var/lib/apt/lists/*

# Install the Claude Code CLI globally (as root, before dropping privileges) so
# `claude` is on PATH for the agent user.
RUN npm install -g @anthropic-ai/claude-code

# Align the image's user to the host's uid/gid (the #1 bind-mount gotcha).
ARG AGENT_UID=1000
ARG AGENT_GID=1000
RUN groupmod -o -g "$AGENT_GID" node \
  && usermod  -o -u "$AGENT_UID" -g "$AGENT_GID" -d /home/agent -m -l agent node

USER ${AGENT_UID}:${AGENT_GID}
WORKDIR /home/agent
ENTRYPOINT ["sleep", "infinity"]
