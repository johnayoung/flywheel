# fw eval

Measure whether a change to the agent setup (CLAUDE.md, skills, MCP config, prompts, model, flywheel version) makes agents better or worse. Crate: `crates/flywheel-eval`.

**Key points**
- Oracles decide: a trial passes only if the task's held-out oracle passes on its final commit. The agent's claims are never used.
- A frozen set never changes. To fix one, freeze a new version.
- A difference inside the measured noise is reported as `tie`.

## Sets

```bash
fw eval set build core               # mine past runs into core@draft (--include landed,hard; --since; --max; --tasks)
fw eval set validate core@draft      # V1-V7 per task, holding the test-suite lock (see below)
fw eval set calibrate core --variant BASE -k 3 --budget 100   # tiers from how often the baseline passed
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

**Hub sessions** become tasks only when sound:
- the change branch merged;
- one operator prompt carries the requirements, and later prompts only approve or continue;
- the change touches code.

`fw eval set from-session NAME SESSION --hub-state-dir DIR` adds one, with no oracle. Blind-author its oracle (`/fw-verify`) into the printed paths, then validate. Until then, validation fails naming the paths.

**Tiers are measured, not guessed:** `calibrate` runs the baseline on the validated draft and sets each task's tier:

| Tier | Baseline result | Use |
| --- | --- | --- |
| `frontier` | never passed | headroom |
| `discriminating` | sometimes passed | where success differences show |
| `regression` | always passed | regressions and efficiency |

`--from-run RUN` reuses a finished run instead of running one. `--split dev=.6,val=.2,test=.2` at freeze fixes the splits, seeded.

**Base:** the parent of the landed commit's `Flywheel-Run` trailer chain.

| Check | Proves |
| --- | --- |
| V1 / V2 | The oracle fails at the base and passes at the reference, twice each. It runs with `CARGO_TARGET_DIR` outside the tree and an empty `FLYWHEEL_HOME`. |
| V3 | Every prerequisite's landed commit is in the base. If not, the base moves to the reference's parent. |
| V4 | The base mirror holds no later commit. |
| V5 | Two copies of the oracle pass at once. If it fails while `fw` processes run beside it, it is kept but graded alone. |
| V6 / V7 | No oracle file or task names a live flywheel home. The task loads, has no prerequisites, and the set carries every home file its own graders run. |

**Test-suite lock:** oracle runs take turns with every other test suite on the machine through `scripts/check.sh`'s flock (`$XDG_RUNTIME_DIR/flywheel-check.lock`).
- `FLYWHEEL_EVAL_LOCK=PATH` names a different lock; `FLYWHEEL_EVAL_LOCK=0` takes none.
- No lock is taken inside a check.sh test phase (`FLYWHEEL_CHECK_PHASE=tests`) or with `FLYWHEEL_CHECK_LOCK=0`.
- Waiting for the lock counts against `--timeout`.

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

## Trials

A trial runs one set task under one variant, starting from the task's historical base. Steps:

1. **Clone.** Clone a base mirror (the base and its past only, no remote) into `evals/runs/<run>/trials/<trial>/work/`.
2. **Overlay.** Commit the variant's overlay as branch `eval/trial`.
3. **Fresh home.** Create a fresh `FLYWHEEL_HOME` holding the task, its specs, and a held-out registration that points at the run's oracle, which lives outside that home.
4. **Run the worker.** Run `fw worker --task ID --base eval/trial --setting-sources project,local --permission-mode M [--model M]` with the variant's `fw` and env, and `FLYWHEEL_BATCH=eval:<run>:<trial>`.
5. **Grade.** Run the oracle out of band on the final commit, in a clean checkout, under the machine's test-suite lock. The final commit is the landed one if there is one, else the parked work branch, else the base.
6. **Clean up.** Copy Claude Code's per-path state for the trial's directories (`~/.claude/projects/<key>/`: transcripts, auto-memory) into the artifacts, then remove it. Delete the work dir. Keep the final commit in `evals/runs/<run>/results.git`.

```bash
fw eval trial core@v1 TASK --variant rule-off    # one trial, recorded as a one-cell run
```

Artifacts in each trial dir: `worker.log`, `attempts.json`, `transcripts/`, `overlay.json`, `final.diff`, `oracle.log`, `checks.json`, `trial.json`, `claude-projects/`.

## Replays

```bash
fw eval estimate core@v1 --variant rule-on --variant rule-off -k 3      # $ mean/P90, wall hours, smallest detectable difference
fw eval replay core@v1 --variant rule-on --variant rule-off -k 3 --budget 160 [--parallel 3] [--primary success,cost] [--hub-state-dir DIR]
fw eval replay --resume RUN [--budget USD]                              # after a budget pause (exit 75)
fw eval runs [--json]
```

- **Order:** tasks in a seeded order; for each task, k rounds; in each round, every variant. A run that stops early is still paired.
- **Parallelism:** `--parallel` runners claim cells with a lease, and each cell runs as its own `fw eval trial --plan` process.
- **Lone oracles:** a task whose oracle fails beside `fw` processes (V5) runs with no other cell in flight.
- **Budget:** a cell starts only if spent + in-flight priors + its prior stays within the cap. Running trials are never cut. The default cap is P90 x 1.1, and an explicit `--budget` is required above $50.
- **Priors:** the median cost of this task under this variant, else the median under any variant, else the original run's cost, else the set median.
- **Errors and hub:** machinery errors are retried once. With `--hub-state-dir`, the hub's capture hooks label every trial `fw.eval=<run>`, and each cell holds the shared lease `eval:<run>`.

## Reports

```bash
fw eval report RUN [--baseline LABEL] [--tier regression,discriminating] [--per-task] [--json]
```

- **Which trials count:** only graded trials. A contaminated or voided pass counts as a fail. A suspect pass counts as a pass until the operator decides. Infra errors are left out but counted.
- **Per task and variant:** success rate, unbiased pass@k (`1 - C(n-c,k)/C(n,k)`) and pass^k (`C(c,k)/C(n,k)`), cost and turns per success (failed trials' cost included).
- **Overall:** macro means over build tasks. Don't-act tasks are reported separately as abstain accuracy.
- **Variant vs baseline, paired by task:** per-task effects (points for success; log ratios shown as % for cost, turns, and time). Each comes with:
  - a 95% CI from a seeded paired bootstrap (10k resamples, trials resampled within each task);
  - the MDE, `(z.975 + z.8) * sd(effects) / sqrt(tasks)`;
  - a verdict: `better` or `worse` when the CI excludes 0, `tie` when it holds 0, and `no information` when every task is saturated for success.
- **Metric labels:** the metrics named in `--primary` are starred; the rest are exploratory.
- **Fastest passes:** listed as worth a read, since that is where fake passes hide.
- **JSON:** the output is the hub view's data (`schema: 1`).

## Trace checks

The deterministic checks run over each trial's harness transcript (flywheel-agents' normalized events, so Claude Code and Codex read alike):
- `check_sh_runs` counts runs of `scripts/check.sh`. A mention in text, a grep, or a pgrep does not count.
- Also counted: unscoped test runs, Skill, MCP and subagent calls, test edits, `LOOP_STATUS` envelopes per attempt, and the longest streak of tool errors.
- A tool input naming the operator's project home (held-out, verification, specs, tasks, eval sets) or the run's oracles marks the trial **contaminated**, which counts as a fail.

`fw eval trace DIR [--forbid PATH]` prints them for any transcript dir.

## Transcript review

```bash
fw eval review RUN [--model haiku]          # one read-only reader per trial, then a grouped summary (runs/RUN/review.md)
fw eval decide RUN TRIAL --confirm|--clear  # a suspect pass: void it, or let it stand
```

- **Coverage:** readers read every failure. They read every pass while a run has at most 200; beyond that, a seeded 25% of each (variant, task) cell.
- **Readers:** Claude Code with `Read,Grep,Glob` only, no user layers, and the frozen brief `crates/flywheel-eval/analysis.md`, whose hash is recorded on the run.
- **Verdicts:** `clean`, `fake_pass`, `discovery_bug`, `access_bug`, `spec_ambiguity`, `harness_bug`, `gave_up`.
- **Suspect passes:** a `fake_pass` at high confidence makes a pass **suspect**. A reader never changes a verdict; only `--confirm` voids a pass.
- **Cost:** review is a separate, paid step, so a replay does not start it on its own.

## Don't-act tasks

```bash
fw eval set dont-act core two,three     # adds two-dont-act, three-dont-act to core@draft; then validate
```

A don't-act task is a validated landed task, replayed at its own reference: the work is already done. It passes only if all three hold:
1. **No change:** nothing is committed past the overlay (the first oracle command).
2. **Still green:** the original oracle passes.
3. **Inspected:** the harness transcript shows a read of a file the task names and a build or test command (the `inspected` check).

Validation is inverted: the oracle passes at the base and fails once a commit is made on top of it. The report shows these tasks as abstain accuracy, apart from success.

## Where results live

| What | Where |
| --- | --- |
| Runs, resolved variants, trials, scores, reviews, artifact digests | the project store's `eval_*` tables (SQLite or Postgres) |
| Frozen sets, variant blobs | `evals/sets/`, `evals/blobs/` (content-addressed) |
| Trial artifacts, plans, `review.md`, final commits (`results.git`) | `evals/runs/<run>/` |
| Live traces | the hub's `telemetry.db` (`--hub-state-dir`), labelled `fw.eval=<run>` and kept past retention |

For a later harness-promotion loop:
- **Splits:** a proposer reads `dev` results only; `test` is for promotion.
- **Frozen inputs:** each run records the set hash, the review brief's hash, and every variant hash, so a changed evaluator shows.
- **Stable contract:** the report JSON (`schema`).
