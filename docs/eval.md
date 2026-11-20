# fw eval

Measure whether a change to the agent setup (CLAUDE.md, skills, MCP config, prompts, model, flywheel version) makes agents better or worse. Crate: `crates/flywheel-eval`.

**Key points**
- Oracles decide: a trial passes only if the task's held-out oracle passes on its final commit. The agent's claims are never used.
- A frozen set never changes. To fix one, freeze a new version.
- A difference inside the measured noise is reported as `tie`.

## Sets

```bash
fw eval set build core               # mine past runs into core@draft (--include landed,hard; --since; --max; --tasks)
fw eval set validate core@draft      # V1-V7 per task (holds the machine's test-suite lock)
fw eval set freeze core              # -> core@v1: hashed, refs pinned, read-only (--split dev=.6,val=.2,test=.2)
fw eval set ls | show core@v1 | diff core@v1 core@v2
```

Which tasks qualify: only those with a held-out registration. The miner labels each one with a source.

| Source | Meaning |
| --- | --- |
| `landed` | Landed. |
| `hard-parked` | The held-out gate parked it, and later work landed. |
| `hard-retried` | An earlier attempt failed validation. |
| `hard-failed` | Never landed. Its reference is bisected at validation. |

Standing-verify parks, and parks that re-landed unchanged, are flakes and are not counted as hard.

**Base:** the parent of the landed commit's `Flywheel-Run` trailer chain.

| Check | Proves |
| --- | --- |
| V1 / V2 | The oracle fails at the base and passes at the reference, twice each. It runs with `CARGO_TARGET_DIR` outside the tree and an empty `FLYWHEEL_HOME`. |
| V3 | Every prerequisite's landed commit is in the base. If not, the base moves to the reference's parent. |
| V4 | The base mirror holds no later commit. |
| V5 | Two copies of the oracle pass at once. If it fails while `fw` processes run beside it, it is kept but graded alone. |
| V6 / V7 | No oracle file or task names a live flywheel home. The task loads and has no prerequisites. |

Layout: `$FLYWHEEL_HOME/projects/<id>/evals/sets/<name>/{draft,vN}/` holds `set.json`, `tasks/`, `oracles/<task>/`, `specs/<task>/`, and `proofs/`. Project-home paths appear as `@EVAL_PROJECT_HOME@`. Index: the `eval_*` tables ([persistence-tables.md](persistence-tables.md#eval-tables)).

## Variants

A variant is a declared agent setup: `evals/variants/NAME.toml`. Before a trial runs, it is resolved into content, kept in `evals/blobs/`, and hashed. The hash covers everything except the variant's name.

```toml
agent = "claude"                     # claude | codex
model = "opus"
fw = "current"                       # or a path to an fw binary (hashed)
permission_mode = "bypassPermissions"
settings = "settings/x.json"         # -> .claude/settings.local.json (hooks, env, permissions)
[env]                                # set on the worker; the agent inherits it
[[repo]]                             # one overlay commit per task base
path = "CLAUDE.md"
op = "strip"                         # replace | append | ensure | strip | delete | replace_dir
pattern = '(?ms)^\*\*Exception: flywheel worker runs\*\*.*?\n\n'
[user]
claude_md = "inherit"                # none | inherit | PATH -> CLAUDE.local.md
skills = ["fw-plan"]                 # none | inherit | [names] | DIR -> .claude/skills/
mcp_servers = ["context7"]           # from ~/.claude.json -> .mcp.json
```

**Claude Code variants**
- Trials never load user-level layers. The worker runs with `--setting-sources project,local`, so the user's CLAUDE.md, skills, and MCP servers reach the agent only through the `[user]` overlays.
- No separate config dir is used, so credentials stay where Claude Code refreshes them.
- Before each replay, a no-cost canary checks that this Claude Code build still honors the flag. It plants a skill, an MCP server, and a hook in a scratch config dir and runs once with `user` and once without.

**Codex variants (v1)** take only `model`, `env`, and `[[repo]]` overlays. The user's Codex files are hashed, so a change between runs shows up.

```bash
fw eval variant ls | show NAME | diff A B | pin NAME
```
