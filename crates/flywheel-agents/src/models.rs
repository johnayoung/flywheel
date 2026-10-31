//! Domain models: identity, probe results, run requests, and the fold output.
//!
//! [`CompletedRun`] is the supported product contract: one agent run drained
//! to completion and folded into structured signals. Its field set maps
//! one-to-one onto flywheel-core's invocation signals without this crate
//! importing anything from core: the bridge lives above this crate.

use crate::capabilities::AgentCapabilities;
use crate::config::AgentConfiguration;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// Token counters keyed by the vendor's counter name.
pub type Usage = BTreeMap<String, i64>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterCompatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_tested_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_package_version: Option<String>,
}

/// Stable identity of one adapter. `id` is an open string, never an enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescriptor {
    pub id: String,
    pub display_name: String,
    pub adapter_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default)]
    pub executable_names: Vec<String>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub compatibility: AdapterCompatibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticationKind {
    AccountSession,
    ApiKey,
    Mixed,
    InstallationOnly,
    Unauthenticated,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssuranceLevel {
    Verified,
    StrongIndication,
    BestEffort,
    #[default]
    Unknown,
}

/// Installation and authentication evidence for one agent on this host.
///
/// Never claims more than it can verify: `authentication_assurance`
/// qualifies `authentication_kind`, and evidence strings must not contain
/// secret values (names and paths only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProbeResult {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub authentication_kind: AuthenticationKind,
    #[serde(default)]
    pub authentication_assurance: AssuranceLevel,
    #[serde(default)]
    pub authentication_evidence: Vec<String>,
    #[serde(default)]
    pub config_paths: Vec<PathBuf>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl AgentProbeResult {
    pub fn not_installed(warning: impl Into<String>, evidence: Vec<String>) -> Self {
        Self {
            installed: false,
            executable_path: None,
            version: None,
            authentication_kind: AuthenticationKind::Unknown,
            authentication_assurance: AssuranceLevel::Unknown,
            authentication_evidence: evidence,
            config_paths: Vec::new(),
            warnings: vec![warning.into()],
        }
    }
}

/// One run: a prompt drained to completion in a working directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    pub prompt: String,
    pub working_directory: PathBuf,
    pub configuration: AgentConfiguration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    Completed,
    MaxTokens,
    MaxTurns,
    Cancelled,
    Error,
    #[default]
    Unknown,
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            StopReason::Completed => "completed",
            StopReason::MaxTokens => "max-tokens",
            StopReason::MaxTurns => "max-turns",
            StopReason::Cancelled => "cancelled",
            StopReason::Error => "error",
            StopReason::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<StopReason> {
        match s {
            "completed" => Some(StopReason::Completed),
            "max-tokens" => Some(StopReason::MaxTokens),
            "max-turns" => Some(StopReason::MaxTurns),
            "cancelled" => Some(StopReason::Cancelled),
            "error" => Some(StopReason::Error),
            "unknown" => Some(StopReason::Unknown),
            _ => None,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the run stopped: normalized reason plus the vendor's own string.
///
/// `finished` is true only when the agent emitted its terminal envelope (a
/// stream-json `result`, a codex `turn.completed`); a crash or kill before
/// the terminal envelope leaves it false.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopInfo {
    pub reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_subtype: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub pending_tool_use: bool,
    #[serde(default)]
    pub finished: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

/// A tool call paired with its result (when one was observed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInteraction {
    pub tool_use_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionDenial {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub detail: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// When the limit resets, as epoch seconds (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch: Option<f64>,
    /// The vendor's limit state: `allowed`, `allowed_warning`, or
    /// `rejected` (a hard block until `resets_at_epoch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Which window tripped (e.g. `five_hour`, `seven_day`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_type: Option<String>,
    /// The full normalized `rate.limited` payload (native info under `detail`).
    #[serde(default)]
    pub detail: serde_json::Map<String, Value>,
}

/// A vendor-specific fault recognized by the adapter's classifier.
///
/// `kind` is a small open vocabulary: `session_limit`, `auth`, `quota`,
/// `infra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentFault {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch: Option<f64>,
}

/// What a fault classifier may inspect. No live process access.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultEvidence {
    pub final_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_subtype: Option<String>,
    #[serde(default)]
    pub is_error: bool,
}

/// Transport or process failure, distinct from agent output.
///
/// Raw evidence, no classification. The caller decides what a failure means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFailure {
    pub error_type: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentExit {
    /// Exit code, or the negated signal number when signal-terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returncode: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

/// One run folded to completion. The supported product contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedRun {
    pub final_text: String,
    pub stop: StopInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub tool_interactions: Vec<ToolInteraction>,
    #[serde(default)]
    pub permission_denials: Vec<PermissionDenial>,
    #[serde(default)]
    pub rate_limit_events: Vec<RateLimitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<AgentFault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<RunFailure>,
    #[serde(default)]
    pub exit: AgentExit,
    #[serde(default)]
    pub event_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_round_trips() {
        for reason in [
            StopReason::Completed,
            StopReason::MaxTokens,
            StopReason::MaxTurns,
            StopReason::Cancelled,
            StopReason::Error,
            StopReason::Unknown,
        ] {
            assert_eq!(StopReason::parse(reason.as_str()), Some(reason));
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
        }
        assert_eq!(StopReason::parse("bogus"), None);
    }

    #[test]
    fn auth_enums_serialize_kebab_case() {
        assert_eq!(
            serde_json::to_string(&AuthenticationKind::AccountSession).unwrap(),
            "\"account-session\""
        );
        assert_eq!(
            serde_json::to_string(&AssuranceLevel::StrongIndication).unwrap(),
            "\"strong-indication\""
        );
    }
}
