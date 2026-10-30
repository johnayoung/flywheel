//! Default-on, operator-overridable wall-clock deadline ceilings (spec 00066).
//!
//! Each of the five external-call classes flywheel issues -- the working
//! agent iteration, the rubric judge stream, command graders, docker
//! *management* calls, and the autopilot discovery/authoring agent --
//! resolves a finite, non-null wall-clock ceiling here with no operator
//! override required (D-1: default-on, generous, overridable). An operator
//! may raise/lower the ceiling per class, or opt a class out (unbounded) by
//! overriding with `0` (or `None` through the typed API).
//!
//! This module only *resolves* ceilings; it applies none of them. Each
//! dependent call-site layer reads the resolved value and feeds it to the
//! shared [`crate::deadline`] primitive. A resolved value is `Option<f64>`:
//! a positive, finite number of seconds is the ceiling and `None` is the
//! explicit unbounded opt-out.
//!
//! See `docs/configuration.md` (`[deadlines]`) for the on-disk reference.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::time::Duration;

/// The five external-call classes governed by a wall-clock deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineClass {
    AgentIteration,
    RubricJudge,
    CommandGrader,
    DockerManagement,
    AutopilotAgent,
}

impl DeadlineClass {
    /// Every class, in declaration order.
    pub const ALL: [DeadlineClass; 5] = [
        DeadlineClass::AgentIteration,
        DeadlineClass::RubricJudge,
        DeadlineClass::CommandGrader,
        DeadlineClass::DockerManagement,
        DeadlineClass::AutopilotAgent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DeadlineClass::AgentIteration => "agent_iteration",
            DeadlineClass::RubricJudge => "rubric_judge",
            DeadlineClass::CommandGrader => "command_grader",
            DeadlineClass::DockerManagement => "docker_management",
            DeadlineClass::AutopilotAgent => "autopilot_agent",
        }
    }

    /// The `[deadlines]` TOML key that overrides this class.
    ///
    /// An omitted key keeps the default; a key set to `0` opts the class
    /// out (unbounded). TOML cannot express a null, so `0` is the on-disk
    /// spelling of the unbounded opt-out.
    pub const fn table_key(self) -> &'static str {
        match self {
            DeadlineClass::AgentIteration => "agent_iteration_seconds",
            DeadlineClass::RubricJudge => "rubric_judge_seconds",
            DeadlineClass::CommandGrader => "command_grader_seconds",
            DeadlineClass::DockerManagement => "docker_management_seconds",
            DeadlineClass::AutopilotAgent => "autopilot_agent_seconds",
        }
    }

    /// The generous, finite default ceiling (seconds) for this class.
    pub const fn default_seconds(self) -> f64 {
        match self {
            DeadlineClass::AgentIteration => DEFAULT_AGENT_ITERATION_SECONDS,
            DeadlineClass::RubricJudge => DEFAULT_RUBRIC_JUDGE_SECONDS,
            DeadlineClass::CommandGrader => DEFAULT_COMMAND_GRADER_SECONDS,
            DeadlineClass::DockerManagement => DEFAULT_DOCKER_MANAGEMENT_SECONDS,
            DeadlineClass::AutopilotAgent => DEFAULT_AUTOPILOT_AGENT_SECONDS,
        }
    }
}

impl fmt::Display for DeadlineClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Generous, finite default ceilings (seconds). Magnitudes are chosen to clear
// a legitimately slow call of each class with headroom -- the requirement is
// finite + default-on, not a "correct" number (spec 00066 non-goal). They
// align with existing bounds where one exists (docker `start_container` is
// 120s; the container `exec_timeout` default is 1800s).
pub const DEFAULT_AGENT_ITERATION_SECONDS: f64 = 3600.0;
pub const DEFAULT_RUBRIC_JUDGE_SECONDS: f64 = 600.0;
pub const DEFAULT_COMMAND_GRADER_SECONDS: f64 = 900.0;
pub const DEFAULT_DOCKER_MANAGEMENT_SECONDS: f64 = 120.0;
pub const DEFAULT_AUTOPILOT_AGENT_SECONDS: f64 = 1800.0;

/// The published default table, one entry per class in [`DeadlineClass::ALL`]
/// order.
pub const DEFAULT_DEADLINE_SECONDS: [(DeadlineClass, f64); 5] = [
    (
        DeadlineClass::AgentIteration,
        DEFAULT_AGENT_ITERATION_SECONDS,
    ),
    (DeadlineClass::RubricJudge, DEFAULT_RUBRIC_JUDGE_SECONDS),
    (DeadlineClass::CommandGrader, DEFAULT_COMMAND_GRADER_SECONDS),
    (
        DeadlineClass::DockerManagement,
        DEFAULT_DOCKER_MANAGEMENT_SECONDS,
    ),
    (
        DeadlineClass::AutopilotAgent,
        DEFAULT_AUTOPILOT_AGENT_SECONDS,
    ),
];

/// A `[deadlines]` table value that is not a number.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("deadlines.{key} must be a number, got {value}")]
pub struct DeadlineConfigError {
    pub key: &'static str,
    pub value: String,
}

/// Map a raw per-class override to a resolved ceiling.
///
/// `None` and any value that is not a finite positive number are the
/// unbounded opt-out and resolve to `None`, as is a value too large to be a
/// [`Duration`] (past ~1.8e19 seconds -- unbounded in every practical sense,
/// and converting it would panic); any other positive value resolves to
/// itself. This is the single place that distinguishes "operator opted
/// this class out" from "operator gave no override" -- the latter never
/// reaches here (the caller keys on presence and uses the default instead).
fn normalize_override(value: Option<f64>) -> Option<f64> {
    value.filter(|v| *v > 0.0 && Duration::try_from_secs_f64(*v).is_ok())
}

/// Resolved wall-clock ceilings for the five external-call classes.
///
/// Each field is `Option<f64>`: a positive float is the ceiling in seconds,
/// `None` is the explicit unbounded opt-out. Built via
/// [`resolve_deadlines`]; the default-constructed instance carries the
/// finite, non-null defaults for every class (default-on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeadlineConfig {
    pub agent_iteration_seconds: Option<f64>,
    pub rubric_judge_seconds: Option<f64>,
    pub command_grader_seconds: Option<f64>,
    pub docker_management_seconds: Option<f64>,
    pub autopilot_agent_seconds: Option<f64>,
}

impl Default for DeadlineConfig {
    fn default() -> Self {
        DeadlineConfig {
            agent_iteration_seconds: Some(DEFAULT_AGENT_ITERATION_SECONDS),
            rubric_judge_seconds: Some(DEFAULT_RUBRIC_JUDGE_SECONDS),
            command_grader_seconds: Some(DEFAULT_COMMAND_GRADER_SECONDS),
            docker_management_seconds: Some(DEFAULT_DOCKER_MANAGEMENT_SECONDS),
            autopilot_agent_seconds: Some(DEFAULT_AUTOPILOT_AGENT_SECONDS),
        }
    }
}

impl DeadlineConfig {
    /// Return the resolved ceiling for `deadline_class`.
    ///
    /// `None` means unbounded (the operator opted this class out); a
    /// positive float is the ceiling in seconds.
    pub fn for_class(&self, deadline_class: DeadlineClass) -> Option<f64> {
        match deadline_class {
            DeadlineClass::AgentIteration => self.agent_iteration_seconds,
            DeadlineClass::RubricJudge => self.rubric_judge_seconds,
            DeadlineClass::CommandGrader => self.command_grader_seconds,
            DeadlineClass::DockerManagement => self.docker_management_seconds,
            DeadlineClass::AutopilotAgent => self.autopilot_agent_seconds,
        }
    }

    /// The resolved ceiling as a [`Duration`], ready to hand to
    /// [`crate::deadline::Deadline::new`]. `None` is the unbounded opt-out,
    /// which also covers a directly built or deserialized ceiling that is
    /// not a representable duration.
    #[must_use]
    pub fn duration_for_class(&self, deadline_class: DeadlineClass) -> Option<Duration> {
        self.for_class(deadline_class)
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
    }

    fn set(&mut self, deadline_class: DeadlineClass, ceiling: Option<f64>) {
        match deadline_class {
            DeadlineClass::AgentIteration => self.agent_iteration_seconds = ceiling,
            DeadlineClass::RubricJudge => self.rubric_judge_seconds = ceiling,
            DeadlineClass::CommandGrader => self.command_grader_seconds = ceiling,
            DeadlineClass::DockerManagement => self.docker_management_seconds = ceiling,
            DeadlineClass::AutopilotAgent => self.autopilot_agent_seconds = ceiling,
        }
    }
}

/// Resolve the five ceilings, applying per-class overrides over defaults.
///
/// A class absent from `overrides` keeps its finite, non-null default
/// (default-on). A class present with a positive value takes that value
/// exactly; present with `Some(0)`, a negative number, a non-finite number,
/// or `None` resolves to the unbounded opt-out. Presence -- not value --
/// distinguishes "kept the default" from "opted out", so an explicit
/// unbounded override never snaps back to the default. When a class appears
/// more than once the last entry wins.
pub fn resolve_deadlines<I>(overrides: I) -> DeadlineConfig
where
    I: IntoIterator<Item = (DeadlineClass, Option<f64>)>,
{
    let mut config = DeadlineConfig::default();
    for (deadline_class, value) in overrides {
        config.set(deadline_class, normalize_override(value));
    }
    config
}

/// Resolve a [`DeadlineConfig`] from a TOML `[deadlines]` table.
///
/// Each recognized key (see [`DeadlineClass::table_key`]) must be a number;
/// an omitted key keeps the default and a key set to `0` opts the class out
/// (TOML has no null, so `0` is the on-disk unbounded spelling). Unknown
/// keys are ignored so a future key does not hard-fail an older reader. A
/// non-numeric value (including a boolean) is a configuration error.
pub fn deadline_config_from_mapping(
    table: Option<&Map<String, Value>>,
) -> Result<DeadlineConfig, DeadlineConfigError> {
    let Some(table) = table else {
        return Ok(DeadlineConfig::default());
    };
    let mut overrides = Vec::new();
    for deadline_class in DeadlineClass::ALL {
        let key = deadline_class.table_key();
        let Some(value) = table.get(key) else {
            continue;
        };
        let Some(numeric) = value.as_f64() else {
            return Err(DeadlineConfigError {
                key,
                value: value.to_string(),
            });
        };
        overrides.push((deadline_class, Some(numeric)));
    }
    Ok(resolve_deadlines(overrides))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn is_finite_positive(value: Option<f64>) -> bool {
        value.is_some_and(|v| v.is_finite() && v > 0.0)
    }

    fn table(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn enum_covers_exactly_the_five_call_classes() {
        let keys: Vec<DeadlineClass> = DEFAULT_DEADLINE_SECONDS.iter().map(|(c, _)| *c).collect();
        assert_eq!(keys, DeadlineClass::ALL.to_vec());
        for (class, seconds) in DEFAULT_DEADLINE_SECONDS {
            assert_eq!(class.default_seconds(), seconds);
        }
    }

    #[test]
    fn default_config_yields_finite_non_null_ceiling_for_every_class() {
        let resolved = resolve_deadlines([]);
        let constructed = DeadlineConfig::default();
        for class in DeadlineClass::ALL {
            assert!(is_finite_positive(resolved.for_class(class)), "{class}");
            assert!(is_finite_positive(constructed.for_class(class)), "{class}");
            assert_eq!(resolved.for_class(class), Some(class.default_seconds()));
            assert_eq!(
                resolved.duration_for_class(class),
                Some(Duration::from_secs_f64(class.default_seconds()))
            );
        }
        assert_eq!(resolved, constructed);
    }

    #[test]
    fn explicit_override_is_reflected_exactly_per_class() {
        for class in DeadlineClass::ALL {
            let config = resolve_deadlines([(class, Some(42.5))]);
            assert_eq!(config.for_class(class), Some(42.5));
            for other in DeadlineClass::ALL.into_iter().filter(|o| *o != class) {
                assert_eq!(config.for_class(other), Some(other.default_seconds()));
            }
        }
    }

    #[test]
    fn overriding_all_five_changes_every_resolved_value() {
        let overrides: Vec<(DeadlineClass, Option<f64>)> = DeadlineClass::ALL
            .into_iter()
            .enumerate()
            .map(|(i, c)| (c, Some((i + 1) as f64)))
            .collect();
        let config = resolve_deadlines(overrides.clone());
        for (class, expected) in overrides {
            assert_eq!(config.for_class(class), expected);
        }
    }

    #[test]
    fn zero_none_negative_and_non_finite_opt_the_class_out() {
        for class in DeadlineClass::ALL {
            for raw in [
                Some(0.0),
                None,
                Some(-1.0),
                Some(f64::NAN),
                Some(f64::INFINITY),
            ] {
                let config = resolve_deadlines([(class, raw)]);
                assert_eq!(config.for_class(class), None, "{class} {raw:?}");
                assert_eq!(config.duration_for_class(class), None);
                // The opt-out never snaps back to the finite default.
                assert_ne!(config.for_class(class), Some(class.default_seconds()));
            }
        }
    }

    #[test]
    fn override_past_the_duration_range_opts_out_instead_of_panicking() {
        for class in DeadlineClass::ALL {
            let config = resolve_deadlines([(class, Some(1e20))]);
            assert_eq!(config.for_class(class), None, "{class}");
            assert_eq!(config.duration_for_class(class), None, "{class}");
        }
        let mapped =
            deadline_config_from_mapping(Some(&table(json!({"agent_iteration_seconds": 1e300}))))
                .unwrap();
        assert_eq!(mapped.agent_iteration_seconds, None);
        // A directly built (or deserialized) config bypasses normalization;
        // the Duration accessor still never panics.
        let raw = DeadlineConfig {
            rubric_judge_seconds: Some(1e300),
            ..DeadlineConfig::default()
        };
        assert_eq!(raw.duration_for_class(DeadlineClass::RubricJudge), None);
    }

    #[test]
    fn last_duplicate_override_wins() {
        let config = resolve_deadlines([
            (DeadlineClass::RubricJudge, Some(5.0)),
            (DeadlineClass::RubricJudge, Some(9.0)),
        ]);
        assert_eq!(config.rubric_judge_seconds, Some(9.0));
    }

    #[test]
    fn mapping_path_with_no_overrides_matches_defaults() {
        assert_eq!(
            deadline_config_from_mapping(None).unwrap(),
            DeadlineConfig::default()
        );
        assert_eq!(
            deadline_config_from_mapping(Some(&Map::new())).unwrap(),
            DeadlineConfig::default()
        );
    }

    #[test]
    fn mapping_path_applies_per_class_overrides() {
        let t = table(json!({"agent_iteration_seconds": 120, "command_grader_seconds": 0}));
        let config = deadline_config_from_mapping(Some(&t)).unwrap();
        assert_eq!(config.for_class(DeadlineClass::AgentIteration), Some(120.0));
        assert_eq!(config.for_class(DeadlineClass::CommandGrader), None);
        assert_eq!(
            config.for_class(DeadlineClass::RubricJudge),
            Some(DEFAULT_RUBRIC_JUDGE_SECONDS)
        );
    }

    #[test]
    fn mapping_path_accepts_floats_and_negatives() {
        let t = table(json!({"rubric_judge_seconds": 2.5, "docker_management_seconds": -3}));
        let config = deadline_config_from_mapping(Some(&t)).unwrap();
        assert_eq!(config.rubric_judge_seconds, Some(2.5));
        assert_eq!(config.docker_management_seconds, None);
    }

    #[test]
    fn mapping_path_ignores_unknown_keys() {
        let t = table(json!({"not_a_class_seconds": 7}));
        assert_eq!(
            deadline_config_from_mapping(Some(&t)).unwrap(),
            DeadlineConfig::default()
        );
    }

    #[test]
    fn mapping_path_rejects_non_numeric_and_boolean_values() {
        let t = table(json!({"agent_iteration_seconds": "soon"}));
        let err = deadline_config_from_mapping(Some(&t)).unwrap_err();
        assert_eq!(err.key, "agent_iteration_seconds");
        assert_eq!(
            err.to_string(),
            "deadlines.agent_iteration_seconds must be a number, got \"soon\""
        );
        // A TOML `true` must not be read as `1`.
        let t = table(json!({"rubric_judge_seconds": true}));
        assert!(deadline_config_from_mapping(Some(&t)).is_err());
        let t = table(json!({"autopilot_agent_seconds": null}));
        assert!(deadline_config_from_mapping(Some(&t)).is_err());
    }

    #[test]
    fn class_serde_and_display_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&DeadlineClass::DockerManagement).unwrap(),
            "\"docker_management\""
        );
        let back: DeadlineClass = serde_json::from_str("\"autopilot_agent\"").unwrap();
        assert_eq!(back, DeadlineClass::AutopilotAgent);
        assert_eq!(DeadlineClass::RubricJudge.to_string(), "rubric_judge");
        assert_eq!(
            DeadlineClass::RubricJudge.table_key(),
            "rubric_judge_seconds"
        );
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = resolve_deadlines([(DeadlineClass::CommandGrader, None)]);
        let json = serde_json::to_string(&config).unwrap();
        let back: DeadlineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }
}
