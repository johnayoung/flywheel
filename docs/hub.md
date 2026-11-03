# Hub

`fw hub` is a web workspace for AI coding agents (Claude Code, Codex, plain shells): the hub server owns every terminal as a real PTY, and browsers, phones, and `fw hub attach` attach and detach without disturbing it. Workspaces group folders (git repos or plain directories); agents run in them or in sibling git worktrees, with live status, exact-copy suggested commands, review, and verified fan-outs beside the terminals. Crate: `crates/flywheel-hub`.

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

Details: [hub/security.md](hub/security.md).

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

| Feature | What | Docs |
|---|---|---|
| Sessions | server-owned PTYs; snapshot replay on attach; many viewers | [internals](hub/internals.md#model) |
| Workspaces | name + 1..32 folders; VS Code `*.code-workspace` import | [reference](hub/reference.md#verbs), [internals](hub/internals.md#model) |
| Lifecycle | `start` / `stop` / `upgrade` (live handoff) / restore on restart / systemd unit | [internals](hub/internals.md#lifecycle-and-restore) |
| Agent status | working / blocked / done / idle per client, hooks + screen manifests, `explain` | [internals](hub/internals.md#agent-status) |
| Agent launcher | first prompt, model, permissions, per-workspace templates | [workflows](hub/workflows.md#agent-launcher) |
| Suggested commands | agent code blocks exactly as written; Copy / Send / Send + Run | [workflows](hub/workflows.md#suggested-commands) |
| Scripting | `new` / `send` / `wait` / `read` / `commands` from scripts and agents | [workflows](hub/workflows.md#scripting-agents) |
| Terminal client | `fw hub attach`: any terminal, local or over SSH | [internals](hub/internals.md#terminal-client-fw-hub-attach) |
| Layout | tabs and split panes per workspace, shared by every browser | [internals](hub/internals.md#layout-tabs-and-split-panes) |
| Phone | key bar, quick answers, compose, PWA, Needs-you start screen | [internals](hub/internals.md#phone) |
| Worktrees | sibling `<repo>.worktrees/<task>` per agent; companion shells | [workflows](hub/workflows.md#worktrees-and-companion-shells) |
| Review | branch diff, commit / push / PR / rebase / ff-merge with exact-command plans | [workflows](hub/workflows.md#review) |
| Handoff, peer review | send a selection / diff / command to another agent; Claude <-> Codex review | [workflows](hub/workflows.md#handoff-and-peer-review) |
| Verified fan-out | one prompt to 2-6 agents, graded by commands the hub runs; pick the winner | [workflows](hub/workflows.md#verified-fan-out) |
| Flywheel panel | the verified loop's tasks, runs, workers beside the terminals | [workflows](hub/workflows.md#flywheel-panel) |
| Search | one query over scrollback, transcripts, commands | [internals](hub/internals.md#search) |
| Usage and cost | tokens and list-price cost per session, workspace, day, week | [workflows](hub/workflows.md#usage-and-cost) |
| Notifications | browser alerts; ntfy-style push on blocked / done | [internals](hub/internals.md#push-notifications) |
| Remote access | tailscale serve, built-in TLS, LAN | [security](hub/security.md#remote-access) |
| Machines | one UI over several hubs, proxied with peer credentials | [security](hub/security.md#machines-and-peers) |
| Share links | read-only, expiring live views of a session | [security](hub/security.md#share-links) |
| Recording | asciinema v2 `.cast` per session; in-browser replay | [internals](hub/internals.md#recording) |
| Settings | `hub.toml` (server) + synced UI settings and rebindable keys | [reference](hub/reference.md#hubtoml) |
| Plugins | local executables on hub events and as palette actions | [reference](hub/reference.md#plugintoml), [api](hub/api.md#plugins) |
| Doctor | checks this machine for the hub, each finding with a fix | [internals](hub/internals.md#doctor) |
| API | REST + terminal and events WebSockets | [api](hub/api.md) |
<!-- Prompt queue (hub/35): add its row here. -->

## Verbs

Full syntax, flags, and files: [hub/reference.md](hub/reference.md). `fw hub <verb> --help` is authoritative.

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

- [hub/reference.md](hub/reference.md) -- every verb and flag, serve flags, `hub.toml`, `notify.toml`, `plugin.toml`, state-dir files, keyboard shortcuts.
- [hub/api.md](hub/api.md) -- REST routes, terminal and events WebSockets, plugin payloads.
- [hub/security.md](hub/security.md) -- auth, remote access, TLS, machines and peers, share links, connection bounds.
- [hub/workflows.md](hub/workflows.md) -- scripting, launcher, suggested commands, usage, worktrees, review, handoff, fan-out, Flywheel panel.
- [hub/internals.md](hub/internals.md) -- model, terminal emulation, lifecycle, restore, live handoff, status detection, layout, phone, search, recording, push, doctor.

Embedded in the binary: `fw docs hub`, `fw docs hub-reference`, `fw docs hub-api`, `fw docs hub-security`, `fw docs hub-workflows`, `fw docs hub-internals`.
