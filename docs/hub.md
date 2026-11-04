# Hub

`fw hub` is a web workspace for AI coding agents (Claude Code, Codex, shells) whose server owns every terminal: browsers, phones, and `fw hub attach` come and go without disturbing a session. Workspaces group folders; agents run there or in sibling worktrees, with live status, exact-copy commands, review, and verified fan-outs. Crate: `crates/flywheel-hub`.

## Security first

- **A hub is a shell over HTTP.** Its token, a login cookie, a peer credential, or a shell in one of its sessions is a shell as you.
- **Agents are not sandboxed**: they run as you and can read the state dir. **Bind** stays `127.0.0.1`; other devices via `tailscale serve` or `--tls auto`, never plain HTTP on an untrusted network. Share links show everything; plugins run unsandboxed. More: [hub-security](hub/security.md#trust-model).

## 60-second quickstart

```bash
cargo install --path crates/flywheel  # fw + flywheel on PATH (~/.cargo/bin)
fw hub doctor  # optional: PATH, claude/codex, hooks, port, tailscale
fw hub start  # detached on 127.0.0.1:7420; prints one-time login URLs
fw hub open  # this machine's browser, signed in
fw hub import ~/code/*.code-workspace  # VS Code multi-root workspaces -> hub workspaces
id=$(fw hub new myrepo --create --folder ~/code/myrepo --kind claude --prompt "Fix the failing test")  # or the UI (Alt N)
fw hub wait "$id" --until done --timeout 900
fw hub commands "$id" --last --copy  # newest suggested shell block, exact text, OSC 52 copy
```

Phone or another machine:

```bash
tailscale serve --bg http://127.0.0.1:7420  # recommended: https on the tailnet
fw hub url --qr  # scan the login QR

fw hub stop; fw hub start --bind 0.0.0.0 --tls auto  # or: built-in https on the LAN
fw hub url --qr  # first QR installs the CA (/ca), second logs in over https
```

After a rebuild: `fw hub upgrade` (live handoff, no session restarts), not stop/start.

## Feature map

Read with `fw docs <topic>`; the anchor is the section.

| Feature | What | Docs |
|---|---|---|
| Sessions | server-owned PTYs, snapshot on attach, many viewers | [hub-internals](hub/internals.md#model) |
| Workspaces | 1..32 folders; VS Code import | [hub-reference](hub/reference.md#verbs), [hub-internals](hub/internals.md#model) |
| Lifecycle | start / stop / live upgrade / restore / systemd | [hub-internals](hub/internals.md#lifecycle-and-restore) |
| Agent status | working / blocked / done / idle; hooks + screen rules | [hub-internals](hub/internals.md#agent-status) |
| Agent launcher | prompt, model, permissions, templates | [hub-workflows](hub/workflows.md#agent-launcher) |
| Suggested commands | agent code blocks exactly as written | [hub-workflows](hub/workflows.md#suggested-commands) |
| Scripting | `new` / `send` / `wait` / `read` / `commands` | [hub-workflows](hub/workflows.md#scripting-agents) |
| Terminal client | `fw hub attach`, local or over SSH | [hub-internals](hub/internals.md#terminal-client-fw-hub-attach) |
| Layout | tabs and split panes, shared | [hub-internals](hub/internals.md#layout-tabs-and-split-panes) |
| Phone | key bar, quick answers, compose, PWA | [hub-internals](hub/internals.md#phone) |
| Worktrees | sibling `<repo>.worktrees/<task>`; companions | [hub-workflows](hub/workflows.md#worktrees-and-companion-shells) |
| Review | diff, commit / push / PR / rebase / ff-merge, planned | [hub-workflows](hub/workflows.md#review) |
| Handoff, peer review | send work to another agent; Claude <-> Codex review | [hub-workflows](hub/workflows.md#handoff-and-peer-review) |
| Verified fan-out | 2-6 agents on one prompt, hub-graded; pick the winner | [hub-workflows](hub/workflows.md#verified-fan-out) |
| Flywheel panel | the verified loop beside the terminals | [hub-workflows](hub/workflows.md#flywheel-panel) |
| Search | scrollback, transcripts, commands | [hub-internals](hub/internals.md#search) |
| Usage and cost | tokens and list-price cost | [hub-workflows](hub/workflows.md#usage-and-cost) |
| Notifications | browser alerts; ntfy push | [hub-internals](hub/internals.md#push-notifications) |
| Remote access | tailscale serve, built-in TLS, LAN | [hub-security](hub/security.md#remote-access) |
| Machines | one UI over several hubs | [hub-security](hub/security.md#machines-and-peers) |
| Share links | read-only expiring live views | [hub-security](hub/security.md#share-links) |
| Recording | asciinema `.cast`, in-browser replay | [hub-internals](hub/internals.md#recording) |
| Settings | `hub.toml`, synced UI settings, keys | [hub-reference](hub/reference.md#hubtoml) |
| Plugins | local executables on events and as actions | [hub-reference](hub/reference.md#plugintoml), [hub-api](hub/api.md#plugins) |
| Doctor | machine check with fixes | [hub-internals](hub/internals.md#doctor) |
| API | REST + terminal and events WebSockets | [hub-api](hub/api.md) |
| Prompt queue | prompts typed in once the agent is done | [hub-workflows](hub/workflows.md#prompt-queue) |
| Activity | timeline, "while you were away" | [hub-workflows](hub/workflows.md#activity) |

## Verbs

Full syntax, flags, and files: [hub-reference](hub/reference.md). `fw hub <verb> --help` is authoritative.

| Group | Verbs |
|---|---|
| Server | `start` `stop` `upgrade` `status` `logs` `serve` `install-service` `doctor` |
| Sign-in and access | `open` `url` `token` `tls` `peer` `machine` `share` |
| Sessions | `ls` `new` `send` `read` `wait` `attach` `kill` `relaunch` `explain` `import` `template` `queue` |
| Agent output | `commands` `search` `activity` `record` |
| Git and agents together | `review` `merge` `handoff` `ask-review` `fanout` |
| Settings | `config` `plugin` |
| Hooks | `report` (agent status hooks) |

