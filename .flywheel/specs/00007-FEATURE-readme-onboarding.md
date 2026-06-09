# Feature: README + onboarding for public OSS visitors

## Summary

Populate the currently-empty `README.md` with a full, OSS-quality onboarding page aimed at strangers landing on the GitHub repo, and add a minimal `CONTRIBUTING.md` stub that the README links to. License selection is deferred — the README marks it as TBD.

## Background

`README.md` is currently 0 bytes. The project already has rich `docs/` content (`vision.md`, `task-schema.md`, `task-lifecycle.md`, `loop.md`, `strategy.md`) and a strong one-line pitch in `pyproject.toml`: "Production-grade orchestration loop for AI coding agents." Public visitors arriving at `https://github.com/johnayoung/flywheel` currently see nothing — no pitch, no install instructions, no pointer to docs. This spec fixes that with a single onboarding pass.

## Scope

### In Scope
- Populate `README.md` with a full OSS-style README aimed at public visitors.
- Create `CONTRIBUTING.md` as a minimal stub linked from the README.
- An explicit pre-release / WIP status banner at the top of the README.
- A copy-pasteable quickstart that walks through: install, write a minimal Task JSON, run it via `python -m flywheel.workflow run`.
- Links from the README into `docs/vision.md`, `docs/task-schema.md`, `docs/task-lifecycle.md`, `docs/loop.md`, and `docs/strategy.md`.
- Repo URL `https://github.com/johnayoung/flywheel` woven into clone commands and the contributing footer.

### Out of Scope
- Creating a `LICENSE` file. The README states "License: TBD".
- Project-layout / directory-tree section in the README (explicitly skipped).
- Badges (CI, version, license) — premature without a public CI pipeline or license.
- Tutorials, design rationale, or architecture deep-dives — those live in `docs/`.
- Editing existing `docs/*.md` files.
- A full contributing guide. `CONTRIBUTING.md` is a stub only.

## Requirements

### Functional Requirements

1. **FR-1: README contains the required top-level sections, in order.**
   - Title (`# Flywheel`) and one-line pitch derived from `pyproject.toml`'s description.
   - **Status banner**: explicit pre-release / WIP notice immediately under the title, calling out that APIs are unstable and breaking changes should be expected.
   - **Quickstart**: install (`uv sync`, Python 3.13 required), minimal `task.json` example, `python -m flywheel.workflow run task.json` invocation.
   - **Documentation**: bulleted links into each file under `docs/` with a one-line description per link.
   - **Contributing**: short prose pointing to `CONTRIBUTING.md`.
   - **License**: one line stating "License: TBD".
   - Acceptance: section headers are present in the order above; grep finds the keywords `pre-release`/`WIP`, `uv sync`, `python -m flywheel.workflow run`, `docs/vision.md`, `CONTRIBUTING.md`, and `License: TBD`.

2. **FR-2: README's quickstart is self-contained and runs end-to-end.**
   - The Task JSON shown must parse against `flywheel.loaders.load_task_from_path`.
   - The CLI command shown must be the actual `python -m flywheel.workflow run` form including any required flags (`--db`, sandbox path, etc.) so a reader copy-pasting it does not hit an immediate flag error.
   - Acceptance: copying the JSON and command into a fresh shell from a clone produces either a `done` outcome or a clearly diagnosable grader failure — not a parse error or CLI usage error.

3. **FR-3: `CONTRIBUTING.md` exists at the repo root as a minimal stub.**
   - Sections: dev setup (`uv sync`, Python 3.13), running tests (`uv run pytest`), commit style (Conventional Commits matching existing history), link back to `docs/vision.md` for project context.
   - Length target: ~30 lines.
   - Acceptance: file exists at `/CONTRIBUTING.md`; grep finds `uv sync`, `uv run pytest`, `Conventional Commits`, and a link to `docs/vision.md`.

4. **FR-4: The repo URL `https://github.com/johnayoung/flywheel` is threaded through README + CONTRIBUTING.**
   - README clone instruction in the Quickstart uses this URL.
   - Acceptance: grep finds `github.com/johnayoung/flywheel` in `README.md`.

5. **FR-5: README respects the project's documentation conventions.**
   - No emojis (project-wide rule in `CLAUDE.md`).
   - Concise prose — engineers scan, they don't read novels (per `~/.claude/CLAUDE.md`).
   - No marketing-speak, no badges, no images.
   - Acceptance: grep for emoji unicode ranges returns nothing; no `<img>` or `![...]` markdown image syntax; no `[![...]]` badge syntax.

### Non-Functional Requirements
- **Performance**: N/A.
- **Security**: N/A.
- **UX**: README must render correctly on GitHub's markdown renderer (CommonMark + GFM tables/code fences).

## Behavior Specification

### Happy Path
1. Stranger lands on `https://github.com/johnayoung/flywheel`.
2. They see the project pitch, the pre-release banner, and a quickstart they can copy-paste.
3. They run the quickstart and either get a `done` outcome from the workflow or a grader output they can debug.
4. They follow the documentation links into `docs/` for deeper context.
5. If they want to contribute, they follow the README link to `CONTRIBUTING.md`.

### Error Handling
| Error Condition | Expected Behavior |
| --- | --- |
| Reader on Python < 3.13 | Quickstart calls out "Python 3.13 required" before any install command runs. |
| Reader without `uv` installed | Quickstart links to the official `uv` install docs rather than reproducing them. |
| Reader's clone is missing the example Task JSON | Task JSON is fully inlined in the README so no separate file is needed. |

### Edge Cases
| Case | Expected Behavior |
| --- | --- |
| Reader runs the quickstart command from a directory that is not the repo root | Command shown uses paths relative to repo root and the README states this assumption explicitly. |
| Reader is looking for a license to assess legal reuse | "License: TBD" is unambiguous; they know not to reuse code yet. |
| `pyproject.toml` description drifts from README pitch | Acceptable for this task — the description is authoritative; README is allowed to elaborate but should not contradict. |

## Technical Context

### Affected Apps
- Repo root (single Python project; no app split).

### Integration Points
- `pyproject.toml`: README pitch derives from its `description` field; no code-level integration.
- `docs/*.md`: README links into these but does not embed their content.

### Relevant Existing Code
- `README.md` — currently 0 bytes; to be populated.
- `pyproject.toml` — source of the one-line pitch.
- `docs/vision.md` — primary doc target; provides the long-form pitch and motivation the README links to.
- `docs/task-schema.md` — referenced for the Task JSON shown in the quickstart.
- `CLAUDE.md` (root) — quickstart commands must match the canonical `uv` invocations documented there.

## Decisions Log

| Decision | Choice | Rationale |
| --- | --- | --- |
| Audience | Public open-source visitors | User-confirmed; drives section list and tone. |
| Scope | Full README (pitch, status, install, usage, docs links, contributing, license) | User-confirmed. |
| Status disclosure | Explicit pre-release / WIP banner at top | User-confirmed; sets expectations for API stability. |
| License declaration | "License: TBD" — no `LICENSE` file in this task | User-confirmed; defers a real licensing decision. |
| Quickstart format | Inline Task JSON + `python -m flywheel.workflow run` CLI | User-confirmed; covers both the data shape and the runner in one example. |
| Contributing | Stub `CONTRIBUTING.md` linked from README | User-confirmed; gives room to grow without bloating README. |
| Project layout section | Skip | User-confirmed; redundant with `docs/` and easy to drift. |
| Repository URL | `https://github.com/johnayoung/flywheel` | User-confirmed. |
| Badges / images | None | Premature pre-release; project convention is no decoration. |

## Open Questions

None. A `LICENSE` file selection is deferred by design; it is not a blocker for this task and is explicitly out of scope.

## Next Steps

Run `/task 00007-FEATURE-readme-onboarding` to generate implementation tasks from this spec.
