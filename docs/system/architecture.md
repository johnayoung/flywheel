# Systems Architecture — Separation of Responsibilities

A stack-agnostic reference for architecting systems that turn **human intent** into **machine execution**. The focus is *natural separation of concepts*, not technology choices. Pair this with the companion **Glossary of Terms**, which defines every noun used below.

---

## The one idea everything hangs off

There are two fundamentally different kinds of "work":

- **Intent** — what a human decided should happen. Durable, declarative, reviewable, slow-changing. Owned by the human.
- **Execution** — a machine actually doing it. Ephemeral, stateful, messy, disposable. Owned by the runtime.

Most architectural rot comes from naming these the same thing and letting them bleed together. The entire structure below is a disciplined refusal to let that happen. The "WorkItem → Run → Session → Turn → Process" ladder is one concept expressed at five altitudes — each with a different lifespan, owner, and failure mode.

---

## Two shapes of separation

The architecture has **two perpendicular kinds of boundary**, and they obey *opposite* rules.

### Horizontal — Layers

Stacked planes. **Dependencies point only downward. Vocabulary is allowed to change at every line**, because each line is a real boundary between owners and rates-of-change.

> Discipline for layers: *keep vocabulary separate.* A `Task` (storage) and an `Issue` (wire) describing the same thing is correct, not duplication.

### Vertical — Cross-cutting concerns

Persistence, observability, configuration, identity. These cut *through* every layer. They are **not** another horizontal band.

> Discipline for cross-cutting concerns: *keep the concern out of the domain entirely,* behind a single interface, so the domain never knows it's there. A WorkItem must not know it's being persisted, measured, or permission-checked.

This perpendicularity is the part most designs get wrong: infrastructure feels "beneath" the architecture, so it gets scattered inline — and then "swap the database" or "add analytics" becomes a six-month project because the concern leaked into a thousand call sites.

---

## The map

```
                    │ PERSIST │ OBSERVE │  AUTH  │ CONFIG │   ← cross-cutting bars:
                    │ (Store) │ (Events)│        │        │     cut through ALL layers,
 ───────────────────┼─────────┼─────────┼────────┼────────┤     stay OUT of the domain
 INTENT             │ WorkItem · Stage · Board · Relation · [Tenancy]
 ───────────────────┤
 BINDING            │ Assignment · Draft · Mapping · Integration-ref (PR#, host ID)
 ───────────────────┤
 EXECUTION          │ Run · Session · Turn · Process · Environment
 ───────────────────┤
 CAPABILITY         │ VersionControl · AgentRuntime · CodeHost(adapter) · Review · FileSystem
 ───────────────────┴───────────────────────────────────────
 COMPOSITION          [ Root ]   ← Wire Contract (depends on NOTHING)
                         │          Config resolved here, injected downward
 DEPLOYMENT           Local profile / Hosted profile
                         │          (also picks: which Store is canonical, which Sinks attach)
 ENTRYPOINTS          HTTP API  ·  Agent/MCP  ·  CLI    ← peers; auth enforced here
```

Read top-to-bottom: **dependencies only ever point downward.**
Each horizontal line: **vocabulary may change** (real boundary).
Each vertical bar: **must stay invisible to the domain.**

---

## The layers, top to bottom

### Intent
The planning surface. WorkItems arranged on Boards, moving through user-defined Stages, connected by typed Relations, all scoped to a Tenancy. This is the layer your users describe in their own words. It changes slowly and is the human's source of truth.

### Binding
The deliberately thin bridge between intent and execution. An **Assignment** is the explicit moment a WorkItem becomes actionable. A **Draft** holds not-yet-committed state as a first-class typed entity. **Mapping** code translates intent concepts into execution concepts and back — and its *existence is a feature*, the proof your boundary is real. **Integration references** point to entities owned by external systems.

Skipping this layer is the classic mistake: when intent and execution touch directly, "this WorkItem exists" silently becomes "this work is running," and you lose the ability to plan without executing.

### Execution
Where machines actually do the work, in **disposable** units. A **Run** is one isolated, throw-away-able effort. A **Session** is a resumable conversation within it. A **Turn** is the finest durable audit grain. A **Process** is the real OS/queue unit with hard machine-status. The **Environment** is the swappable sandbox it all runs inside.

If destroying a Run is scary, this layer isn't isolated enough — disposability is the entire point.

### Capability
Domain-blind verbs. Version control, agent runtime, code host, review engine, file system, notifications. Each does one thing and **must not import a domain type**. A capability that knows what a WorkItem is has stopped being a capability.

---

## The wiring, bottom to top

### Composition root
One interface (an accessor bundle / service trait) where every capability and service is assembled. The application layer is written against this — never against concrete implementations. This single indirection is what makes the system both testable (swap the root for a fake) and portable (swap the root for a different runtime target).

### Deployment profile
A concrete implementation of the composition root per runtime target. The *same* application core ships as a local desktop app **and** a hosted service by changing only the profile. The profile also resolves config and decides which Store is canonical and which observability Sinks attach.

### Entrypoints
Front doors — HTTP, agent/MCP, CLI — that are **peers**, all depending on the core and never on each other. "An API for humans" and "an API for agents" are siblings sharing a core, not one built atop the other. Identity and authorization are enforced here, at the edge.

---

## The cross-cutting bars

### Persistence
Four distinct concepts, not one "storage":

1. **Store / Gateway** — the interface the domain talks to (never the SQL, never the DB).
2. **Record / Row type** — the storage shape, allowed to differ from domain and wire shapes.
3. **Migration** — versioned, forward-only, in source control.
4. **Canonical vs. derived** — one source of truth; caches/indexes/projections are rebuildable from it.

When you have both local and remote stores, declare *which is authoritative* and write the reconciliation policy explicitly. Last-write-wins is an answer; silence is not.

### Observability
The domain emits **one stream of past-tense Events**. The set of **Sinks** watching that stream (analytics, notifications, real-time UI, audit) is a *deployment* decision. This is why analytics can be a config toggle: it's a sink you attach or don't, and the domain is byte-for-byte identical either way. Keep telemetry (ops), analytics (product), and audit (compliance) separate by *purpose* even when they share a source. Gate egress through one privacy/consent checkpoint.

### Configuration
Resolved at the composition root and **injected downward**. Build-time config is baked; runtime config is read at startup. A function deep in the domain reaching for an environment variable is a leak.

### Identity & Access
Authentication establishes *who*; authorization decides *what* and scopes every query to the Tenancy boundary. Both are enforced at the edge and flow inward as trusted context — the domain never re-litigates permissions per layer.

---

## External systems: reference, not record

Anything owned by an external system — a pull request, a host project, a remote URL — is held as a **reference plus a cached snapshot**, never as a canonical record. Your stored row about a PR is a *read-model* of the host's truth. Build for staleness, give it a sync mechanism, and never let the UI imply you can edit fields you don't own. This is why "version control" (local, yours) and "code host" (remote, theirs) are two separate capabilities even though both are "git."

---

## The rules

Ordered by how much pain they save.

1. **Separate planes by rate-of-change and ownership, not by feature.** Ask "who changes this, and how often?" — not "what feature is this?"
2. **Never let execution-failure vocabulary climb into intent.** "Killed/crashed/timed-out" belong to Processes. "Done/cancelled/in-review" belong to WorkItems. A "failed" WorkItem means two planes have fused.
3. **The wire contract depends on nothing.** A zero-dependency contract module is cheap insurance with enormous optionality — it's what lets the two ends evolve independently.
4. **Mapping code is a feature, not debt.** Translation between planes is *evidence* the boundary is real. DRY applies *within* a plane, never *across* one.
5. **Abstract the execution environment behind an interface on day one.** "Where work physically runs" is the most likely thing to change and the most expensive to abstract late.
6. **One composition root, many entrypoints.** Assemble dependencies in exactly one place; make every front door a thin caller.
7. **Drafts and projections are first-class but own no canonical data.** A board, a view, a half-typed form deserve real types — but the moment a view owns data, every other view is wrong.
8. **Disposability is the point of the execution layer.** If throwing away a Run is frightening, it isn't isolated enough.
9. **Cross-cutting concerns stay invisible to the domain.** Persistence, observability, auth, and config each get one interface, injected from the root. Test: delete the analytics sink — does the domain still compile and run unchanged? If not, it wasn't cross-cutting.
10. **Anything an external system owns is a reference + snapshot, never a record.** Your store is downstream of their truth.
11. **One concept may have three shapes — wire, domain, storage — and that's correct.** The conversions are healthy boundary code. DRY within a shape, never across the boundaries between them.

---

## How to apply this without over-building

This much separation is correct for a system that genuinely has **both** the intent/execution duality **and** a local/remote (or multi-target) duality. If you only have one of those tensions, collapse the planes that don't earn their keep.

The glossary and these rules are a **menu of seams, not a mandate to cut all of them.** The real skill is distinguishing the dualities your system actually has from the ones you're imagining because the architecture looks impressive.

A practical sequencing for a greenfield build:

1. Name your **WorkItem** and never overload that name.
2. Draw the **intent ↔ execution** line first; make the Binding layer explicit even if it's tiny.
3. Put your **wire contract** in a dependency-free module immediately.
4. Stand up the **composition root** before you have a second entrypoint or a second deployment target — it's cheap now, expensive later.
5. Abstract the **execution environment** and the **store** behind interfaces before you have two of either.
6. Treat **observability, config, auth** as injected sinks/context from the start — even if the first sink is a no-op.

Everything else can wait until a second concrete case forces the abstraction. Don't abstract on the strength of one example; abstract when the second one arrives and disagrees with the first.