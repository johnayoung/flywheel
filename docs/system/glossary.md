# Glossary of Terms — Agentic Work System

A reusable, stack-agnostic vocabulary for systems that turn **human intent** into **machine execution** (e.g. coding-agent orchestration, task runners, automation platforms).

The organizing principle: **intent and execution are fundamentally different kinds of "work."** Intent is durable, declarative, and human-owned. Execution is ephemeral, stateful, and machine-owned. Most architectural confusion comes from naming them the same thing. This glossary keeps them — and everything around them — distinct.

Terms are grouped by **plane**, because the grouping *is* the architecture. Each plane has a different rate of change, a different owner, and a different failure mode.

---

## Plane 1 — Intent (the planning layer)

> Durable · human-authored · declarative · slow-changing · source of truth is the human.

| Term                                | Definition                                                                                                               | Discipline it enforces                                                                                                                           |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| **WorkItem**                        | The atomic unit of intent: a title, a description, a desired outcome. The thing a human points at and says "do this."    | The noun your users actually talk about. Give it *one* name; never reuse that name for execution state.                                          |
| **Board / Stream**                  | An ordered, filtered *view* of WorkItems. Pure projection — owns no data of its own, only arrangement and grouping.      | A board is a lens, not a table. If a board stores item data, you've leaked.                                                                      |
| **Stage**                           | A user-defined position a WorkItem occupies (e.g. kanban columns). Data, created by the user — *not* a fixed enum.       | Hardcoding statuses as an enum means users can never reorganize. Make stages data.                                                               |
| **Relation**                        | A typed edge between WorkItems: blocks, relates-to, duplicates, parent-of.                                               | Relations are their own concept, not fields on the item. "Parent" as a foreign key is fine; "blocks" as a foreign key is a trap.                 |
| **Tenancy / Workspace (container)** | The top-level ownership boundary that owns Boards, WorkItems, Stages, members. The root of every authorization question. | Everything below scopes to this. ⚠️ Name collision: this "workspace" (tenancy) is **not** the execution "Run" some systems also call a workspace. |

---

## Plane 2 — Binding (the bridge layer)

> The thin, deliberate translation between intent and execution. The most under-appreciated plane — and the one people skip, to their cost.

| Term                       | Definition                                                                                                                           | Discipline it enforces                                                                                                                   |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| **Assignment**             | The moment a WorkItem becomes actionable: someone (human or agent) is bound to it.                                                   | Intent existing ≠ work happening. Make the transition explicit; don't let "assigned" be implied by an execution record's existence.      |
| **Draft / Staging record** | Mutable scratch state *before* something becomes real — the half-filled form, the unsent prompt. Distinct from the committed entity. | Drafts are first-class, not just frontend state. A typed envelope for "not yet committed" enables honest optimistic UI.                  |
| **Mapping**                | The explicit code that translates an Intent concept to an Execution concept and back.                                                | If the planes are properly separated, mapping code *must exist*. Its presence is proof of a clean boundary, not boilerplate to DRY away. |
| **Integration reference**  | A pointer to something owned by *another* system of record — a PR number, a host project ID, an external URL.                        | What you hold is a reference + a synced snapshot, never the master copy. (See Code Host, Plane 4.)                                       |

---

## Plane 3 — Execution (the doing layer)

> Ephemeral · machine-owned · stateful · disposable · created on demand, destroyed routinely · source of truth is the runtime.

| Term              | Definition                                                                                                           | Discipline it enforces                                                                                                                  |
| ----------------- | -------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| **Run / Attempt** | One bounded effort to satisfy a WorkItem. Has an isolated environment, a lifecycle, a result. Disposable by design.  | The key word is *disposable*. A Run can fail, be killed, retried, or thrown away — and the WorkItem above it is untouched.              |
| **Session**       | A continuous line of interaction within a Run — one conversation, one actor, resumable. A Run may contain several.   | Separates "the sandbox" from "the conversation," enabling multiple agents/approaches against one effort.                                |
| **Turn / Step**   | One request→response cycle inside a Session. The finest-grained durable unit; the audit grain.                       | Where "has the human seen this?" and "what was the prompt?" live.                                                                       |
| **Process / Job** | An actual OS-level or queue-level execution unit, with a hard status: `running` / `completed` / `failed` / `killed`. | The *only* place where machine-failure vocabulary ("killed") is legitimate. Never let it climb up into WorkItem status.                 |
| **Environment**   | The isolated context a Run executes in — a branch, a sandbox, a working directory, a container handle.               | Abstract behind an interface from day one. "Local sandbox" and "remote sandbox" must be swappable. Highest-leverage seam in the system. |

---

## Plane 4 — Capability (the verbs)

> Single-responsibility services that *do one thing* and know nothing about your domain nouns.

| Term                             | Definition                                                                                                                                             | Plane note                              | Discipline it enforces                                                                                         |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| **Capability service**           | A single-responsibility unit: version control, agent runtime, review/diff engine, file system, notifications.                                          | —                                       | A capability must not know what a WorkItem is. If it imports your domain types, it's domain logic in disguise. |
| **Version control (capability)** | Local, mechanical git operations: branch, commit, diff, merge, worktree. Talks to a `.git` dir.                                                        | Pure capability                         | No concept of the network, the code host, or your domain. Usable by any project.                               |
| **Code host (adapter)**          | The remote *forge* — GitHub, GitLab, Bitbucket — behind one normalizing interface. PRs, reviews, host-side merge, tokens.                              | Capability/Adapter (provider-swappable) | Many hosts → one interface. The domain says "open a PR," never "call the GitHub API."                          |
| **Adapter**                      | A capability that wraps an external, swappable provider (a specific agent vendor, a specific git host).                                                | —                                       | Normalizes many providers to one interface (e.g. 10+ coding agents behind one runtime interface).              |
| **Profile / Preset**             | A saved, named selection of a capability + its configuration.                                                                                          | —                                       | Separates "which tool, configured how" from "the work being done." Swap tools without touching items.          |
| **Merge**                        | The act of integrating a Run's output. *Direct merge* = version-control capability acting locally. *PR merge* = code-host integration acting remotely. | Spans Capability ↔ Integration          | Same word, two planes. Keep the two paths distinct.                                                            |

---

## Plane 5 — Contract & Composition (the wiring)

| Term                   | Definition                                                                                                   | Discipline it enforces                                                                                                                                                             |
| ---------------------- | ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Wire contract**      | The serializable types crossing a boundary (client↔server, local↔remote). Lives in a dependency-free module. | **The most important rule.** Wire types depend on *nothing*, which frees the intent and execution layers to use different vocabularies without leaking into each other.            |
| **Composition root**   | The single trait/interface where all capabilities and services are assembled and handed to the application.  | One accessor bundle. The application is written against *this*, never against concrete implementations.                                                                            |
| **Deployment profile** | A concrete implementation of the composition root for one runtime target (local app, hosted service).        | Lets the *same* application core ship as a desktop app and a cloud service. Different profile, identical core. Also decides which Store is canonical and which Sinks are attached. |
| **Entrypoint**         | A front door into the core (HTTP API, agent/MCP interface, CLI). Peers, not a hierarchy.                     | Multiple entrypoints depend on the *core*, never on each other. "API for humans" and "API for agents" are siblings.                                                                |

---

## Cross-cutting concern: Persistence

> Not a layer — cuts *across* every plane. The domain must never know it's being persisted.

| Term                                  | Definition                                                                                                        | Discipline it enforces                                                                                                                |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| **Store / Gateway**                   | The interface for loading/saving a domain entity. Not the entity, not the SQL.                                    | ⚠️ Avoid calling this "Repository" if git repos also exist in the system. The domain depends on this interface, never on the database. |
| **Record / Row type**                 | The storage-shaped representation of an entity — *allowed to differ* from both the domain type and the wire type. | One concept can legitimately have three shapes: wire, domain, record. Each serves a boundary.                                         |
| **Migration**                         | A versioned, ordered, forward-only transformation of the storage schema.                                          | First-class, in source control, never edited after shipping. The migration list *is* the schema's history.                            |
| **Canonical store vs. derived store** | One source of truth; everything else (caches, search indexes, projections) is rebuildable from it.                | If deleting a store loses only speed, it's derived. If it loses data, it was canonical — label it correctly.                          |
| **Local-first vs. remote-of-record**  | *Which* store is authoritative when both exist — plus a written reconciliation policy for disagreement.           | Last-write-wins is an answer; silence is not.                                                                                         |

---

## Cross-cutting concern: Observability

> One stream of facts; the number of things watching it is a deployment decision, not a domain decision.

| Term                                  | Definition                                                                                                         | Discipline it enforces                                                                                                          |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| **Event (domain)**                    | A past-tense fact that something happened, emitted by the domain. "WorkItemAssigned," "RunCompleted."              | The domain emits facts; it does not know who listens. One emission feeds analytics, notifications, real-time UI, and audit.     |
| **Sink / Consumer**                   | A subscriber to events: analytics pipeline, notifier, websocket pusher, audit log.                                 | Sinks are pluggable and optional. If removing analytics breaks a feature, analytics wasn't a sink — it was hidden domain logic. |
| **Telemetry vs. analytics vs. audit** | Three *purposes*: telemetry = system health (ops); analytics = usage (product); audit = who did what (compliance). | Same event may feed all three, but they differ in retention, privacy, and access. Don't merge them by source.                   |
| **Privacy / consent gate**            | The single checkpoint deciding whether an observation may leave the system.                                        | One gate, not a consent check scattered across every call site.                                                                 |

---

## Cross-cutting concern: Configuration

| Term                              | Definition                                                                                    | Discipline it enforces                                                                                           |
| --------------------------------- | --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| **Configuration**                 | The resolved settings a deployment runs with (ports, hosts, feature toggles, API keys).       | Read at the composition root and injected downward — never read ad-hoc via global env access deep in the domain. |
| **Build-time vs. runtime config** | Build-time is baked and fixed per build; runtime is read at startup and can vary per install. | A function reaching for an environment variable is config leaking into a plane it doesn't belong in.             |

---

## Cross-cutting concern: Identity & Access

| Term                       | Definition                                                                    | Discipline it enforces                                                                                                        |
| -------------------------- | ----------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| **Authentication (authn)** | Establishing *who* the actor is.                                              | Enforced at the edge (entrypoint), then flows inward as trusted context.                                                      |
| **Authorization (authz)**  | Deciding *what* the actor may do; scopes every query to the Tenancy boundary. | Decided at the edge; the domain operates on "the current authorized actor" and never re-litigates permissions at every layer. |

---

## Quick disambiguation of dangerous collisions

| Word             | Could mean                                                               | Keep distinct because                                                                   |
| ---------------- | ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| **Workspace**    | (a) Tenancy container [Plane 1] · (b) an execution Run/sandbox [Plane 3] | Completely different lifespans and owners. Pick one meaning per system, or rename one.  |
| **Repository**   | (a) Persistence Store · (b) a git repo                                   | Both common; collision causes real confusion. Prefer "Store" for persistence.           |
| **Issue / Task** | The same concept at the wire layer (Issue) vs. the storage layer (Task)  | Allowed to diverge *because* they sit on opposite sides of a boundary. Not duplication. |
| **Merge**        | (a) local direct merge [capability] · (b) PR merge [integration]         | One acts locally on git; the other coordinates with an external host.                   |