# Hub

`fw hub` is a web workspace for AI coding agents (Claude Code, Codex, shells) whose server owns every terminal, so browsers, phones, and `fw hub attach` come and go without disturbing a session. Workspaces group folders; agents run there or in sibling git worktrees, with live status, exact-copy suggested commands, review, and verified fan-outs. Crate: `crates/flywheel-hub`.

## Security first

- **A hub is a shell over HTTP.** Anyone holding its token, a login cookie, a peer credential for it, or a shell in one of its sessions has a shell as you.
- **Agents are not sandboxed.** They run as you and can read the state dir: the token, and `machines.json`'s credentials for every registered machine. Run untrusted agents under another user or in a container.
- **Bind:** `127.0.0.1` by default. For other devices use `tailscale serve` (recommended) or `--bind 0.0.0.0 --tls auto` (built-in HTTPS, name-constrained local CA). Never plain `--bind 0.0.0.0` on an untrusted network: plain HTTP, and phones lose clipboard, notifications, and install.
- **Trust model, one line each:**
  - token (`<state-dir>/token`, 0600) and login cookies: full control.
  - peer credential (`fw hub peer issue`): equivalent to the token; it can start sessions that read the token file.
  - share link (`fw hub share`): read-only, but shows everything that terminal shows, typed secrets included.
  - plugins (`<state-dir>/plugins/`): your executables, run as you, unsandboxed; disabled until you enable them.
  - local CA (`--tls auto`): installing it trusts this hub's names only; check the fingerprint (`fw hub tls ca`).
  - agent status reports: advisory UI state; they cannot start, stop, or change a session.

Details: [hub-security](hub/security.md).

## 60-second quickstart

```bash
cargo install --path crates/flywheel           # fw + flywheel on PATH (~/.cargo/bin)
fw hub doctor                                  # optional: PATH, claude/codex, hooks, port, tailscale
fw hub start                                   # detached on 127.0.0.1:7420; prints one-time login URLs
fw hub open                                    # this machine's browser, signed in
fw hub import ~/code/*.code-workspace          # VS Code multi-root workspaces -> hub workspaces
id=$(fw hub new personal --kind claude --prompt "Fix the failing test")   # or the UI's New session (Alt N)
fw hub wait "$id" --until done --timeout 900
fw hub commands "$id" --last --copy            # newest suggested shell block, exact text, OSC 52 copy
```

Phone or another machine:

```bash
tailscale serve --bg http://127.0.0.1:7420     # recommended: https on the tailnet
fw hub url --qr                                # scan the login QR

fw hub stop; fw hub start --bind 0.0.0.0 --tls auto   # or: built-in https on the LAN
fw hub url --qr                                # first QR installs the CA (/ca), second logs in over https
```

After a rebuild use `fw hub upgrade` (live handoff, no session restarts), not stop/start.

## Feature map

Docs column: `topic: section`; read it with `fw docs <topic>`.

| Feature | What | Docs |
|---|---|---|
| Sessions | server-owned PTYs; snapshot replay on attach; many viewers | [hub-internals: Model](hub/internals.md#model) |
| Workspaces | name + 1..32 folders; VS Code `*.code-workspace` import | [hub-reference: Verbs](hub/reference.md#verbs), [hub-internals: Model](hub/internals.md#model) |
| Lifecycle | `start` / `stop` / `upgrade` (live handoff) / restore on restart / systemd unit | [hub-internals: Lifecycle and restore](hub/internals.md#lifecycle-and-restore) |
| Agent status | working / blocked / done / idle per client, hooks + screen manifests, `explain` | [hub-internals: Agent status](hub/internals.md#agent-status) |
| Agent launcher | first prompt, model, permissions, per-workspace templates | [hub-workflows: Agent launcher](hub/workflows.md#agent-launcher) |
| Suggested commands | agent code blocks exactly as written; Copy / Send / Send + Run | [hub-workflows: Suggested commands](hub/workflows.md#suggested-commands) |
| Scripting | `new` / `send` / `wait` / `read` / `commands` from scripts and agents | [hub-workflows: Scripting agents](hub/workflows.md#scripting-agents) |
| Terminal client | `fw hub attach`: any terminal, local or over SSH | [hub-internals: Terminal client: fw hub attach](hub/internals.md#terminal-client-fw-hub-attach) |
| Layout | tabs and split panes per workspace, shared by every browser | [hub-internals: Layout: tabs and split panes](hub/internals.md#layout-tabs-and-split-panes) |
| Phone | key bar, quick answers, compose, PWA, Needs-you start screen | [hub-internals: Phone](hub/internals.md#phone) |
| Worktrees | sibling `<repo>.worktrees/<task>` per agent; companion shells | [hub-workflows: Worktrees and companion shells](hub/workflows.md#worktrees-and-companion-shells) |
| Review | branch diff, commit / push / PR / rebase / ff-merge with exact-command plans | [hub-workflows: Review](hub/workflows.md#review) |
| Handoff, peer review | send a selection / diff / command to another agent; Claude <-> Codex review | [hub-workflows: Handoff and peer review](hub/workflows.md#handoff-and-peer-review) |
| Verified fan-out | one prompt to 2-6 agents, graded by commands the hub runs; pick the winner | [hub-workflows: Verified fan-out](hub/workflows.md#verified-fan-out) |
| Flywheel panel | the verified loop's tasks, runs, workers beside the terminals | [hub-workflows: Flywheel panel](hub/workflows.md#flywheel-panel) |
| Search | one query over scrollback, transcripts, commands | [hub-internals: Search](hub/internals.md#search) |
| Usage and cost | tokens and list-price cost per session, workspace, day, week | [hub-workflows: Usage and cost](hub/workflows.md#usage-and-cost) |
| Notifications | browser alerts; ntfy-style push on blocked / done | [hub-internals: Push notifications](hub/internals.md#push-notifications) |
| Remote access | tailscale serve, built-in TLS, LAN | [hub-security: Remote access](hub/security.md#remote-access) |
| Machines | one UI over several hubs, proxied with peer credentials | [hub-security: Machines and peers](hub/security.md#machines-and-peers) |
| Share links | read-only, expiring live views of a session | [hub-security: Share links](hub/security.md#share-links) |
| Recording | asciinema v2 `.cast` per session; in-browser replay | [hub-internals: Recording](hub/internals.md#recording) |
| Settings | `hub.toml` (server) + synced UI settings and rebindable keys | [hub-reference: hub.toml](hub/reference.md#hubtoml) |
| Plugins | local executables on hub events and as palette actions | [hub-reference: plugin.toml](hub/reference.md#plugintoml), [hub-api: Plugins](hub/api.md#plugins) |
| Doctor | checks this machine for the hub, each finding with a fix | [hub-internals: Doctor](hub/internals.md#doctor) |
| API | REST + terminal and events WebSockets | [hub-api](hub/api.md) |
<!-- Prompt queue (hub/35): add its row here. -->

## Verbs

Full syntax, flags, and files: [hub-reference](hub/reference.md). `fw hub <verb> --help` is authoritative.

| Group | Verbs |
|---|---|
| Server | `start` `stop` `upgrade` `status` `logs` `serve` `install-service` `doctor` |
| Sign-in and access | `open` `url` `token` `tls` `peer` `machine` `share` |
| Sessions | `ls` `new` `send` `read` `wait` `attach` `kill` `relaunch` `explain` `import` `template` |
| Agent output | `commands` `search` `record` |
| Git and agents together | `review` `merge` `handoff` `ask-review` `fanout` |
| Settings | `config` `plugin` |
| Hooks only | `report` (run by agent status hooks) |

## More

- [hub-reference](hub/reference.md) -- every verb and flag, serve flags, `hub.toml`, `notify.toml`, `plugin.toml`, state-dir files, keyboard shortcuts.
- [hub-api](hub/api.md) -- REST routes, terminal and events WebSockets, plugin payloads.
- [hub-security](hub/security.md) -- auth, remote access, TLS, machines and peers, share links, connection bounds.
- [hub-workflows](hub/workflows.md) -- scripting, launcher, suggested commands, usage, worktrees, review, handoff, fan-out, Flywheel panel.
- [hub-internals](hub/internals.md) -- model, terminal emulation, lifecycle, restore, live handoff, status detection, layout, phone, search, recording, push, doctor.

Read each with `fw docs <topic>`, e.g. `fw docs hub-workflows`.
