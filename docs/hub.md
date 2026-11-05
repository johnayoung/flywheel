# Hub

`fw hub` is a web workspace for AI coding agents (Claude Code, Codex, shells) whose server owns every terminal: browsers, phones, and `fw hub attach` come and go without disturbing a session. Workspaces group folders; agents run there or in sibling worktrees, with live status, exact-copy commands, review, and verified fan-outs. Crate: `crates/flywheel-hub`.

## Security first

**A hub is a shell over HTTP**: its token, a login cookie, a peer credential, or a shell in one of its sessions is a shell as you; agents and plugins are not sandboxed. Keep the bind on `127.0.0.1`; reach it from other devices via `tailscale serve` or `--tls auto`, never plain HTTP on an untrusted network. Share links show everything. More: [hub-security](hub/security.md#trust-model).

## 60-second quickstart

```bash
cargo install --path crates/flywheel  # fw + flywheel on PATH (~/.cargo/bin)
fw hub doctor  # optional: PATH, agents, hooks, port
fw hub start  # detached on 127.0.0.1:7420; prints one-time login URLs
fw hub open  # this machine's browser, signed in
fw hub import ~/code/*.code-workspace  # VS Code multi-root workspaces -> hub workspaces
id=$(fw hub new myrepo --create --folder ~/code/myrepo --kind claude --prompt "Fix the failing test")  # or the UI (Alt N)
fw hub wait "$id" --until done --timeout 900
fw hub commands "$id" --last --copy  # newest suggested shell block, exact, to clipboard
```

Phone or another machine: `tailscale serve --bg http://127.0.0.1:7420`, then `fw hub url --qr` and scan; without Tailscale see [hub-security](hub/security.md#https-without-tailscale). After a rebuild: `fw hub upgrade` (live handoff, no session restarts), not stop/start.

## Feature map

`fw docs <topic>` prints a page.

| Page | Covers |
|---|---|
| [hub-reference](hub/reference.md) | verbs, flags, config files, UI settings, keys, env, state dir |
| [hub-workflows](hub/workflows.md) | scripting, launcher, commands, cost, worktrees, review, PR + CI, handoff, peer review, fan-out, Flywheel, queue, activity, permissions |
| [hub-internals](hub/internals.md) | model, emulation, restore, upgrade, status, detection, `attach`, layout, UI, phone, search, recording, push, doctor |
| [hub-security](hub/security.md) | trust model, auth, remote access, TLS, machines and peers, share links |
| [hub-api](hub/api.md) | REST routes, WebSockets, plugin protocol and events |

## Verbs

Full syntax, flags, and files: [hub-reference](hub/reference.md). `fw hub <verb> --help` is authoritative.

| Group | Verbs |
|---|---|
| Server | `start` `stop` `upgrade` `status` `logs` `serve` `install-service` `doctor` |
| Sign-in and access | `open` `url` `token` `tls` `peer` `machine` `share` |
| Sessions | `ls` `new` `send` `read` `wait` `attach` `kill` `relaunch` `explain` `import` `template` `queue` |
| Agent output | `commands` `search` `activity` `record` |
| Git and agents together | `review` `merge` `pr` `handoff` `ask-review` `fanout` `autopilot` |
| Settings | `config` `plugin` `perms` |
| Hooks | `report` (agent status hooks) |

