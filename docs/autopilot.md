# Autopilot Mode - Tier Hierarchy & Scoring

## Tier Hierarchy

**Tiers 1–3 are preemptive interrupts**: whenever one is `ready`, it floats above everything else. **Tiers 4–11 are scheduled by weighted score**, not strict ordering.

| Tier | Class                             | Mode       | Description                                                                                                                    |
| ---- | --------------------------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------ |
| 1    | Production down / active harm     | Preemptive | Outages, in-progress data loss, active breach, payments failing. Nothing below advances while open.                            |
| 2    | Imminent severe risk              | Preemptive | Actively/trivially exploitable vuln, spreading corruption, hard-deadline compliance violation, dependency about to break prod. |
| 3    | Broken build / blocked pipeline   | Preemptive | CI red, main can't deploy, team blocked from shipping. Blocks everyone's forward motion.                                       |
| 4    | Committed deliverables at risk    | Scheduled  | Work with external commitments (customer deadlines, contracts, dependent teams) about to slip.                                 |
| 5    | Core feature work                 | Scheduled  | The roadmap — new functionality delivering primary value. Default steady state.                                                |
| 6    | Test coverage (shipped/in-flight) | Scheduled  | Tests for code that exists or is being written. Untested code becomes tomorrow's Tier 1.                                       |
| 7    | Non-critical bugs                 | Scheduled  | Known defects, not blocking or severe. System functions.                                                                       |
| 8    | Tech debt / refactoring           | Scheduled  | Cleanup that improves velocity and reduces risk.                                                                               |
| 9    | Observability & tooling           | Scheduled  | Logging, metrics, dashboards, dev ergonomics. Compounding dividends, rarely urgent.                                            |
| 10   | Documentation                     | Scheduled  | READMEs, API docs, runbooks, onboarding. Valuable, almost never time-critical.                                                 |
| 11   | Polish / nice-to-have             | Scheduled  | Cosmetic tweaks, minor optimizations, "wouldn't it be nice."                                                                   |

---

## Scoring

```
if Tier <= 3 and Status == ready:
    Score = INTERRUPT_BASE + Urgency          // always floats above everything

else:
    Score = (TierWeight[Tier] * w_tier)
          + (Urgency          * w_urg)
          + (Importance       * w_imp)
          + (BlocksCount      * w_unblock)     // doing it frees others
          - (Effort           * w_effort)      // cheap wins surface sooner

    // deadline escalation:
    if Deadline != null:
        Urgency rises as (Deadline - now) shrinks
        // lets a normally-low item (e.g. a Tier-10 doc) legitimately
        // jump the queue when its window closes
```

---

## Logic Behind the Split

**Why preemptive vs. scheduled.** A pure top-down hierarchy — always work the highest non-empty tier — starves the lower tiers forever, because there is always *some* feature or bug sitting at Tier 4–7. Documentation and tech debt then get touched *never*. That is exactly how real projects accumulate crippling debt and zero docs.

Strict priority ordering is correct for **interrupts** but wrong for **steady-state allocation**:

- **Tiers 1–3 (preemptive):** these genuinely should preempt everything. "If production is down, fix it" is absolute. While any of these is `ready`, nothing below advances.
- **Tiers 4–11 (scheduled):** below the interrupt line, work is allocated by *weighted score*, not by draining one tier before touching the next. This keeps lower tiers alive instead of leaving them as dead weight.

**Why urgency and importance are separate axes.** A one-dimensional hierarchy can't express "low urgency, but do it *now* because the window closes" — e.g. the only engineer who understands a subsystem leaves Friday. Importance = value/risk-reduction if done, independent of *when*. Urgency = how fast cost grows if untouched. Deadline-driven urgency escalation is what lets a normally-low item rise without hard-coding exceptions.

**Why the score must be legible.** Expose the breakdown (tier, urgency, importance, unblock contributions), not just the final number. A subtly miscalibrated single score steers the whole project wrong and nobody notices until the damage is done. The weights are tuning knobs and will be wrong at first — treat the score as an inspectable recommendation, not ground truth.