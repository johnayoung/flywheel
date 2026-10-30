//! The autopilot scoring engine: the tier model, findings, and the legible
//! score breakdown that sequences them (`docs/autopilot.md`).
//!
//! Pure: no file I/O, no agent calls, no subprocess, no clock. Weights arrive
//! as a [`ScoreWeights`] from the policy layer; this module never reads config.

use crate::policy::AutopilotWeights;
use serde::{Deserialize, Serialize};
use std::fmt;

// --- Tier model (docs/autopilot.md) -----------------------------------------------

/// The inclusive upper bound of the preemptive band; tiers `<= 3` preempt.
pub const PREEMPTIVE_MAX_TIER: u8 = 3;

/// The 11 autopilot tiers, in priority order (1 highest).
///
/// Tiers 1-3 are *preemptive interrupts*: while any such finding is `ready`
/// it floats above everything below. Tiers 4-11 are *scheduled* by weighted
/// score, never by strict tier order. [`Tier::is_preemptive`] is the single
/// boundary every layer keys on, so the `<= 3` rule lives in exactly one
/// place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Tier {
    ProductionDown = 1,
    ImminentSevereRisk = 2,
    BrokenBuild = 3,
    CommittedDeliverables = 4,
    CoreFeatureWork = 5,
    TestCoverage = 6,
    NonCriticalBugs = 7,
    TechDebt = 8,
    ObservabilityTooling = 9,
    Documentation = 10,
    Polish = 11,
}

impl Tier {
    /// Every tier in priority order (1 first).
    pub const ALL: [Tier; 11] = [
        Tier::ProductionDown,
        Tier::ImminentSevereRisk,
        Tier::BrokenBuild,
        Tier::CommittedDeliverables,
        Tier::CoreFeatureWork,
        Tier::TestCoverage,
        Tier::NonCriticalBugs,
        Tier::TechDebt,
        Tier::ObservabilityTooling,
        Tier::Documentation,
        Tier::Polish,
    ];

    /// The tier's numeric value (1-11).
    pub const fn value(self) -> u8 {
        self as u8
    }

    /// The tier for a numeric value, `None` outside 1-11.
    pub fn from_value(value: i64) -> Option<Tier> {
        Tier::ALL
            .into_iter()
            .find(|tier| i64::from(tier.value()) == value)
    }

    /// True for tiers 1-3 (the preemptive interrupt band).
    pub const fn is_preemptive(self) -> bool {
        self.value() <= PREEMPTIVE_MAX_TIER
    }

    /// Per-tier base weight (`TierWeight[Tier]` in the docs formula):
    /// `12 - tier`. A higher tier (lower number) carries more weight; the
    /// scheduled band uses tiers 4-11 (weights 8..1). Defined for all 11
    /// tiers so a non-ready preemptive finding still scores under the
    /// scheduled branch with a sensible weight.
    pub const fn weight(self) -> u32 {
        12 - self.value() as u32
    }

    /// The tier's class name from `docs/autopilot.md`.
    pub const fn name(self) -> &'static str {
        match self {
            Tier::ProductionDown => "Production down / active harm",
            Tier::ImminentSevereRisk => "Imminent severe risk",
            Tier::BrokenBuild => "Broken build / blocked pipeline",
            Tier::CommittedDeliverables => "Committed deliverables at risk",
            Tier::CoreFeatureWork => "Core feature work",
            Tier::TestCoverage => "Test coverage (shipped/in-flight)",
            Tier::NonCriticalBugs => "Non-critical bugs",
            Tier::TechDebt => "Tech debt / refactoring",
            Tier::ObservabilityTooling => "Observability & tooling",
            Tier::Documentation => "Documentation",
            Tier::Polish => "Polish / nice-to-have",
        }
    }

    /// The tier's one-line remit from `docs/autopilot.md`, handed to its
    /// relevance agent.
    pub const fn description(self) -> &'static str {
        match self {
            Tier::ProductionDown => {
                "Outages, in-progress data loss, active breach, payments failing. \
                 Nothing below advances while open."
            }
            Tier::ImminentSevereRisk => {
                "Actively/trivially exploitable vuln, spreading corruption, \
                 hard-deadline compliance violation, dependency about to break prod."
            }
            Tier::BrokenBuild => {
                "CI red, main can't deploy, team blocked from shipping. Blocks \
                 everyone's forward motion."
            }
            Tier::CommittedDeliverables => {
                "Work with external commitments (customer deadlines, contracts, \
                 dependent teams) about to slip."
            }
            Tier::CoreFeatureWork => {
                "The roadmap -- new functionality delivering primary value. Default \
                 steady state."
            }
            Tier::TestCoverage => {
                "Tests for code that exists or is being written. Untested code \
                 becomes tomorrow's Tier 1."
            }
            Tier::NonCriticalBugs => "Known defects, not blocking or severe. System functions.",
            Tier::TechDebt => "Cleanup that improves velocity and reduces risk.",
            Tier::ObservabilityTooling => {
                "Logging, metrics, dashboards, dev ergonomics. Compounding \
                 dividends, rarely urgent."
            }
            Tier::Documentation => {
                "READMEs, API docs, runbooks, onboarding. Valuable, almost never \
                 time-critical."
            }
            Tier::Polish => "Cosmetic tweaks, minor optimizations, \"wouldn't it be nice.\"",
        }
    }
}

impl TryFrom<u8> for Tier {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Tier::from_value(i64::from(value)).ok_or_else(|| format!("{value} is not a valid Tier"))
    }
}

impl From<Tier> for u8 {
    fn from(tier: Tier) -> u8 {
        tier.value()
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value())
    }
}

/// `TierWeight[Tier]` from the docs formula (see [`Tier::weight`]).
pub fn tier_weight(tier: Tier) -> u32 {
    tier.weight()
}

/// The tier's remit handed to its relevance agent: the class name and the
/// one-line description from `docs/autopilot.md`.
pub fn tier_description(tier: Tier) -> (&'static str, &'static str) {
    (tier.name(), tier.description())
}

// --- Score weights ------------------------------------------------------------------

/// `INTERRUPT_BASE` from the docs formula -- large enough that any ready
/// preemptive finding's `INTERRUPT_BASE + Urgency` dominates every possible
/// scheduled score, so the preemptive override is absolute by arithmetic as
/// well as by partition.
pub const INTERRUPT_BASE: f64 = 10_000.0;
pub const W_TIER: f64 = 10.0;
pub const W_URGENCY: f64 = 3.0;
pub const W_IMPORTANCE: f64 = 3.0;
pub const W_UNBLOCK: f64 = 2.0;
pub const W_EFFORT: f64 = 1.0;

/// The tunable weights of the `docs/autopilot.md` scheduled-score formula:
/// the policy's [`AutopilotWeights`] (defined once in [`crate::policy`];
/// its `Default` is the shipped weights above).
pub type ScoreWeights = AutopilotWeights;

/// The shipped weights (the module constants).
pub fn default_weights() -> ScoreWeights {
    ScoreWeights::default()
}

// --- Findings -----------------------------------------------------------------------

/// One concrete unit of work a tier relevance agent surfaced.
///
/// Carries an identity, its tier, the four score axes, and a `ready` status
/// (the `Status == ready` gate the preemptive override keys on), plus the
/// descriptive fields the authoring layer compiles a task from
/// (`title`/`detail`/`evidence`). Downstream layers import this shape and
/// add no field to it.
///
/// `urgency`/`importance`/`blocks`/`effort` are agent-supplied estimates;
/// the *final score* is computed here from them and is never an
/// agent-reported number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub tier: Tier,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub urgency: u32,
    #[serde(default)]
    pub importance: u32,
    #[serde(default)]
    pub blocks: u32,
    #[serde(default)]
    pub effort: u32,
    #[serde(default = "default_ready")]
    pub ready: bool,
}

fn default_ready() -> bool {
    true
}

impl Finding {
    /// A ready finding with every axis at zero and no detail/evidence.
    pub fn new(id: impl Into<String>, tier: Tier, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            title: title.into(),
            detail: String::new(),
            evidence: Vec::new(),
            urgency: 0,
            importance: 0,
            blocks: 0,
            effort: 0,
            ready: true,
        }
    }
}

// --- Score breakdown (legible, recorded) --------------------------------------------

/// The legible score recorded on every sequenced finding.
///
/// Records the five components (`tier_weight`, `urgency`, `importance`,
/// `blocks`, `effort`) plus the `final_score` so the recommendation is
/// auditable: a test recomputes the documented formula from the recorded
/// components and asserts it equals the final. `preemptive` records which
/// formula branch produced it (the `INTERRUPT_BASE + Urgency` interrupt
/// branch, or the weighted scheduled branch), so the recompute is
/// unambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub tier: Tier,
    pub tier_weight: u32,
    pub urgency: u32,
    pub importance: u32,
    pub blocks: u32,
    pub effort: u32,
    #[serde(rename = "final")]
    pub final_score: f64,
    pub preemptive: bool,
}

/// A [`Finding`] paired with the [`ScoreBreakdown`] it earned.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredFinding {
    pub finding: Finding,
    pub breakdown: ScoreBreakdown,
}

/// Compute the [`ScoreBreakdown`] for one finding.
///
/// Implements the `docs/autopilot.md` formula verbatim:
///
/// * a ready Tier 1-3 finding scores `INTERRUPT_BASE + Urgency * w_urg`
///   (preemptive branch -- it floats above everything);
/// * everything else scores
///   `TierWeight*w_tier + Urgency*w_urg + Importance*w_imp
///   + BlocksCount*w_unblock - Effort*w_effort` (scheduled branch).
pub fn score_finding(finding: &Finding, weights: &ScoreWeights) -> ScoreBreakdown {
    let tier_weight = finding.tier.weight();
    let preemptive = finding.tier.is_preemptive() && finding.ready;
    let final_score = compute_final(
        tier_weight,
        finding.urgency,
        finding.importance,
        finding.blocks,
        finding.effort,
        preemptive,
        weights,
    );
    ScoreBreakdown {
        tier: finding.tier,
        tier_weight,
        urgency: finding.urgency,
        importance: finding.importance,
        blocks: finding.blocks,
        effort: finding.effort,
        final_score,
        preemptive,
    }
}

/// The two-branch scoring formula, applied to raw components.
fn compute_final(
    tier_weight: u32,
    urgency: u32,
    importance: u32,
    blocks: u32,
    effort: u32,
    preemptive: bool,
    weights: &ScoreWeights,
) -> f64 {
    if preemptive {
        return weights.interrupt_base + f64::from(urgency) * weights.urgency;
    }
    f64::from(tier_weight) * weights.tier
        + f64::from(urgency) * weights.urgency
        + f64::from(importance) * weights.importance
        + f64::from(blocks) * weights.unblock
        - f64::from(effort) * weights.effort
}

/// Re-evaluate the documented formula from a breakdown's own components.
///
/// The audit primitive behind criterion #3: feeding a recorded breakdown's
/// five components (and its `preemptive` branch flag) back through the
/// formula must reproduce its final. A breakdown whose final ignores a
/// component fails this round-trip.
pub fn recompute_final(breakdown: &ScoreBreakdown, weights: &ScoreWeights) -> f64 {
    compute_final(
        breakdown.tier_weight,
        breakdown.urgency,
        breakdown.importance,
        breakdown.blocks,
        breakdown.effort,
        breakdown.preemptive,
        weights,
    )
}

/// Sequence findings by the tier model: preemptive first, then weighted.
///
/// Every ready Tier 1-3 finding precedes every Tier 4-11 finding regardless
/// of the scheduled scores (the absolute preemptive override). Within each
/// band, order is by descending final score; ties break deterministically
/// by input order (a stable sort), never randomly. An empty input yields an
/// empty sequence.
pub fn sequence_findings(findings: &[Finding], weights: &ScoreWeights) -> Vec<ScoredFinding> {
    let (mut preemptive, mut scheduled): (Vec<ScoredFinding>, Vec<ScoredFinding>) = findings
        .iter()
        .map(|finding| ScoredFinding {
            finding: finding.clone(),
            breakdown: score_finding(finding, weights),
        })
        .partition(|scored| scored.breakdown.preemptive);
    let descending = |a: &ScoredFinding, b: &ScoredFinding| {
        b.breakdown
            .final_score
            .partial_cmp(&a.breakdown.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    preemptive.sort_by(descending);
    scheduled.sort_by(descending);
    preemptive.extend(scheduled);
    preemptive
}

/// Select the top `slots` findings in sequenced order (fill-to-depth).
///
/// The loop layer computes `slots` as `target_depth - current_depth` and
/// asks for that many; this returns the highest-priority findings up to
/// that count (fewer when the input has fewer). A non-positive `slots`
/// selects nothing.
pub fn select_findings(
    findings: &[Finding],
    slots: i64,
    weights: &ScoreWeights,
) -> Vec<ScoredFinding> {
    if slots <= 0 {
        return Vec::new();
    }
    let mut sequenced = sequence_findings(findings, weights);
    sequenced.truncate(usize::try_from(slots).unwrap_or(usize::MAX));
    sequenced
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- helpers -------------------------------------------------------------------

    struct Axes {
        urgency: u32,
        importance: u32,
        blocks: u32,
        effort: u32,
        ready: bool,
    }

    impl Default for Axes {
        fn default() -> Self {
            Self {
                urgency: 0,
                importance: 0,
                blocks: 0,
                effort: 0,
                ready: true,
            }
        }
    }

    fn finding(id: &str, tier: Tier, axes: Axes) -> Finding {
        Finding {
            urgency: axes.urgency,
            importance: axes.importance,
            blocks: axes.blocks,
            effort: axes.effort,
            ready: axes.ready,
            ..Finding::new(id, tier, format!("finding {id}"))
        }
    }

    fn weights() -> ScoreWeights {
        default_weights()
    }

    fn ids(seq: &[ScoredFinding]) -> Vec<&str> {
        seq.iter().map(|s| s.finding.id.as_str()).collect()
    }

    // --- tier model ----------------------------------------------------------------

    #[test]
    fn tier_preemptive_boundary() {
        assert_eq!(
            Tier::ALL.iter().map(|t| t.value()).collect::<Vec<_>>(),
            (1..=11).collect::<Vec<u8>>()
        );
        for tier in Tier::ALL {
            assert_eq!(tier.is_preemptive(), tier.value() <= 3, "{tier:?}");
            assert_eq!(tier.weight(), 12 - u32::from(tier.value()));
            assert_eq!(tier_weight(tier), tier.weight());
            assert_eq!(Tier::from_value(i64::from(tier.value())), Some(tier));
        }
        assert_eq!(Tier::from_value(0), None);
        assert_eq!(Tier::from_value(12), None);
        assert_eq!(PREEMPTIVE_MAX_TIER, 3);
    }

    #[test]
    fn tier_serializes_as_its_number() {
        assert_eq!(serde_json::to_value(Tier::TechDebt).unwrap(), json!(8));
        assert_eq!(
            serde_json::from_value::<Tier>(json!(3)).unwrap(),
            Tier::BrokenBuild
        );
        assert!(serde_json::from_value::<Tier>(json!(12)).is_err());
    }

    #[test]
    fn default_weights_are_the_module_constants() {
        let w = weights();
        assert_eq!(
            (
                w.tier,
                w.urgency,
                w.importance,
                w.unblock,
                w.effort,
                w.interrupt_base
            ),
            (
                W_TIER,
                W_URGENCY,
                W_IMPORTANCE,
                W_UNBLOCK,
                W_EFFORT,
                INTERRUPT_BASE
            )
        );
    }

    // --- criterion #3: legible, recomputable breakdown ------------------------------

    #[test]
    fn breakdown_records_all_five_components_and_recomputes() {
        let f = finding(
            "a",
            Tier::TechDebt,
            Axes {
                urgency: 4,
                importance: 5,
                blocks: 2,
                effort: 3,
                ..Axes::default()
            },
        );
        let bd = score_finding(&f, &weights());
        assert_eq!(bd.tier_weight, Tier::TechDebt.weight());
        assert_eq!(
            (bd.urgency, bd.importance, bd.blocks, bd.effort),
            (4, 5, 2, 3)
        );
        assert_eq!(recompute_final(&bd, &weights()), bd.final_score);
        let w = weights();
        let expected = f64::from(Tier::TechDebt.weight()) * w.tier
            + 4.0 * w.urgency
            + 5.0 * w.importance
            + 2.0 * w.unblock
            - 3.0 * w.effort;
        assert_eq!(bd.final_score, expected);
        assert!(!bd.preemptive);
    }

    #[test]
    fn flipping_any_component_changes_the_final() {
        let base = |urgency, importance, blocks, effort| {
            score_finding(
                &finding(
                    "a",
                    Tier::TechDebt,
                    Axes {
                        urgency,
                        importance,
                        blocks,
                        effort,
                        ready: true,
                    },
                ),
                &weights(),
            )
            .final_score
        };
        let reference = base(1, 1, 1, 1);
        assert_ne!(base(2, 1, 1, 1), reference);
        assert_ne!(base(1, 2, 1, 1), reference);
        assert_ne!(base(1, 1, 2, 1), reference);
        assert_ne!(base(1, 1, 1, 2), reference);
    }

    #[test]
    fn effort_lowers_the_score() {
        let cheap = score_finding(
            &finding("cheap", Tier::TechDebt, Axes::default()),
            &weights(),
        );
        let pricey = score_finding(
            &finding(
                "pricey",
                Tier::TechDebt,
                Axes {
                    effort: 5,
                    ..Axes::default()
                },
            ),
            &weights(),
        );
        assert!(cheap.final_score > pricey.final_score);
    }

    #[test]
    fn preemptive_breakdown_recomputes_under_interrupt_branch() {
        let bd = score_finding(
            &finding(
                "p",
                Tier::BrokenBuild,
                Axes {
                    urgency: 7,
                    ..Axes::default()
                },
            ),
            &weights(),
        );
        assert!(bd.preemptive);
        assert_eq!(bd.final_score, INTERRUPT_BASE + 7.0 * W_URGENCY);
        assert_eq!(recompute_final(&bd, &weights()), bd.final_score);
    }

    // --- criterion #4: absolute preemptive override ---------------------------------

    #[test]
    fn ready_tier_1_3_precede_all_scheduled_regardless_of_score() {
        let findings = vec![
            finding(
                "debt",
                Tier::TechDebt,
                Axes {
                    urgency: 10,
                    importance: 10,
                    blocks: 10,
                    ..Axes::default()
                },
            ),
            finding(
                "feature",
                Tier::CoreFeatureWork,
                Axes {
                    urgency: 10,
                    importance: 10,
                    ..Axes::default()
                },
            ),
            finding("build", Tier::BrokenBuild, Axes::default()),
        ];
        let seq = sequence_findings(&findings, &weights());
        assert_eq!(seq[0].finding.id, "build");
        let first_scheduled = seq.iter().position(|s| !s.breakdown.preemptive).unwrap();
        assert!(
            seq[..first_scheduled]
                .iter()
                .all(|s| s.breakdown.preemptive)
        );
        assert!(
            seq[first_scheduled..]
                .iter()
                .all(|s| !s.breakdown.preemptive)
        );
    }

    #[test]
    fn two_ready_preemptive_both_precede_scheduled_stable() {
        let urgent = || Axes {
            urgency: 1,
            ..Axes::default()
        };
        let findings = vec![
            finding("build-a", Tier::BrokenBuild, urgent()),
            finding(
                "feature",
                Tier::CoreFeatureWork,
                Axes {
                    urgency: 10,
                    importance: 10,
                    ..Axes::default()
                },
            ),
            finding("build-b", Tier::BrokenBuild, urgent()),
        ];
        assert_eq!(
            ids(&sequence_findings(&findings, &weights())),
            vec!["build-a", "build-b", "feature"]
        );
    }

    #[test]
    fn non_ready_preemptive_does_not_float() {
        let findings = vec![
            finding(
                "risk",
                Tier::ImminentSevereRisk,
                Axes {
                    ready: false,
                    ..Axes::default()
                },
            ),
            finding("build", Tier::BrokenBuild, Axes::default()),
        ];
        let seq = sequence_findings(&findings, &weights());
        assert_eq!(seq[0].finding.id, "build");
        let risk = seq.iter().find(|s| s.finding.id == "risk").unwrap();
        assert!(!risk.breakdown.preemptive);
    }

    // --- criterion #5: weighted scheduling, not strict tier order -------------------

    #[test]
    fn higher_scoring_lower_tier_precedes_lower_scoring_higher_tier() {
        let tier8 = finding(
            "debt",
            Tier::TechDebt,
            Axes {
                urgency: 10,
                importance: 10,
                blocks: 10,
                ..Axes::default()
            },
        );
        let tier5 = finding(
            "feature",
            Tier::CoreFeatureWork,
            Axes {
                effort: 10,
                ..Axes::default()
            },
        );
        assert!(
            score_finding(&tier8, &weights()).final_score
                > score_finding(&tier5, &weights()).final_score
        );
        assert_eq!(
            ids(&sequence_findings(&[tier5, tier8], &weights())),
            vec!["debt", "feature"]
        );
    }

    // --- selection to depth ---------------------------------------------------------

    #[test]
    fn select_findings_fills_to_slots_in_sequence_order() {
        let findings = vec![
            finding(
                "t5",
                Tier::CoreFeatureWork,
                Axes {
                    urgency: 1,
                    ..Axes::default()
                },
            ),
            finding("build", Tier::BrokenBuild, Axes::default()),
            finding(
                "t8",
                Tier::TechDebt,
                Axes {
                    urgency: 10,
                    importance: 10,
                    ..Axes::default()
                },
            ),
        ];
        assert_eq!(
            ids(&select_findings(&findings, 2, &weights())),
            vec!["build", "t8"]
        );
        assert_eq!(select_findings(&findings, 0, &weights()), Vec::new());
        assert_eq!(select_findings(&findings, -3, &weights()), Vec::new());
        assert_eq!(select_findings(&findings, 5, &weights()).len(), 3);
    }

    #[test]
    fn empty_finding_set_yields_empty_sequence() {
        assert!(sequence_findings(&[], &weights()).is_empty());
        assert!(select_findings(&[], 4, &weights()).is_empty());
    }

    #[test]
    fn weight_override_changes_ordering() {
        let findings = vec![
            finding(
                "pricey",
                Tier::TechDebt,
                Axes {
                    importance: 10,
                    effort: 10,
                    ..Axes::default()
                },
            ),
            finding(
                "cheap",
                Tier::TechDebt,
                Axes {
                    importance: 6,
                    ..Axes::default()
                },
            ),
        ];
        assert_eq!(
            sequence_findings(&findings, &weights())[0].finding.id,
            "pricey"
        );
        let heavy_effort = ScoreWeights {
            effort: 5.0,
            ..ScoreWeights::default()
        };
        assert_eq!(
            sequence_findings(&findings, &heavy_effort)[0].finding.id,
            "cheap"
        );
    }
}
