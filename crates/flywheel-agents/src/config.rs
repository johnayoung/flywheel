//! Run configuration: the generic knobs plus a per-adapter escape hatch.
//!
//! `adapter_options` carries what the generic fields cannot honestly
//! represent (for claude-code: skills, tool allow/deny lists, setting
//! sources, MCP servers, session resume, ...). Adapters reject unknown
//! permission policies rather than silently approximating them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPolicy {
    /// flywheel's mode: translated to the agent's native bypass.
    Auto,
    #[default]
    Supervised,
    Plan,
    ReadOnly,
}

impl PermissionPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            PermissionPolicy::Auto => "auto",
            PermissionPolicy::Supervised => "supervised",
            PermissionPolicy::Plan => "plan",
            PermissionPolicy::ReadOnly => "read-only",
        }
    }
}

impl fmt::Display for PermissionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticationPolicy {
    #[default]
    AccountPreferred,
    AccountOnly,
    ApiAllowed,
}

impl AuthenticationPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            AuthenticationPolicy::AccountPreferred => "account-preferred",
            AuthenticationPolicy::AccountOnly => "account-only",
            AuthenticationPolicy::ApiAllowed => "api-allowed",
        }
    }
}

impl fmt::Display for AuthenticationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-adapter escape hatch: JSON-shaped options keyed by name.
pub type AdapterOptions = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfiguration {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_id: Option<String>,
    #[serde(default)]
    pub permission_policy: PermissionPolicy,
    #[serde(default)]
    pub authentication_policy: AuthenticationPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_override: Option<Vec<String>>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub adapter_options: AdapterOptions,
}

impl AgentConfiguration {
    /// A configuration with every optional field at its default.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            model_id: None,
            mode_id: None,
            reasoning_id: None,
            permission_policy: PermissionPolicy::default(),
            authentication_policy: AuthenticationPolicy::default(),
            max_turns: None,
            command_override: None,
            environment: BTreeMap::new(),
            adapter_options: AdapterOptions::new(),
        }
    }

    /// Look up one adapter option by key.
    pub fn option(&self, key: &str) -> Option<&serde_json::Value> {
        self.adapter_options.get(key)
    }

    /// A boolean adapter option, with `default` when absent or not a bool.
    pub fn option_bool(&self, key: &str, default: bool) -> bool {
        match self.option(key) {
            Some(serde_json::Value::Bool(b)) => *b,
            _ => default,
        }
    }

    /// A string adapter option, `None` when absent or not a string.
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.option(key).and_then(serde_json::Value::as_str)
    }

    /// A list-of-strings adapter option; non-string items are rendered with
    /// their JSON form. `None` when absent or not an array.
    pub fn option_string_list(&self, key: &str) -> Option<Vec<String>> {
        let items = self.option(key)?.as_array()?;
        Some(
            items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policies_serialize_kebab_case() {
        assert_eq!(
            serde_json::to_string(&PermissionPolicy::ReadOnly).unwrap(),
            "\"read-only\""
        );
        assert_eq!(
            serde_json::to_string(&AuthenticationPolicy::AccountOnly).unwrap(),
            "\"account-only\""
        );
        assert_eq!(PermissionPolicy::default(), PermissionPolicy::Supervised);
        assert_eq!(
            AuthenticationPolicy::default(),
            AuthenticationPolicy::AccountPreferred
        );
    }

    #[test]
    fn option_accessors() {
        let mut cfg = AgentConfiguration::new("claude-code");
        cfg.adapter_options.insert("flag".into(), json!(true));
        cfg.adapter_options.insert("name".into(), json!("x"));
        cfg.adapter_options.insert("list".into(), json!(["a", 1]));
        assert!(cfg.option_bool("flag", false));
        assert!(cfg.option_bool("missing", true));
        assert_eq!(cfg.option_str("name"), Some("x"));
        assert_eq!(cfg.option_str("flag"), None);
        assert_eq!(
            cfg.option_string_list("list").unwrap(),
            vec!["a".to_string(), "1".to_string()]
        );
        assert_eq!(cfg.option_string_list("name"), None);
    }
}
