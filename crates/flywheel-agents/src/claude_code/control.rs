//! Mid-turn control for a claude-code run over the stream-json control
//! protocol: the same `control_request` frames the vendor SDKs write to the
//! CLI's stdin.

use crate::BoxFuture;
use crate::adapter::AgentControl;
use crate::config::PermissionPolicy;
use crate::errors::AgentError;
use crate::jsonl::ControlChannel;
use serde_json::{Value, json};
use std::sync::Arc;

/// The CLI's own permission-mode vocabulary for one policy, as accepted by
/// `--permission-mode` and the `set_permission_mode` control request.
/// `Supervised` maps to the CLI's default (`"default"`).
pub fn permission_mode_for(policy: PermissionPolicy) -> Result<&'static str, AgentError> {
    match policy {
        PermissionPolicy::Auto => Ok("bypassPermissions"),
        PermissionPolicy::Supervised => Ok("default"),
        PermissionPolicy::Plan => Ok("plan"),
        PermissionPolicy::ReadOnly => Err(AgentError::UnsupportedCapability(
            "claude-code has no read-only permission mode; use PLAN or AUTO".into(),
        )),
    }
}

#[derive(Debug)]
pub struct ClaudeControl {
    channel: Arc<ControlChannel>,
}

impl ClaudeControl {
    pub fn new(channel: Arc<ControlChannel>) -> Self {
        Self { channel }
    }
}

impl AgentControl for ClaudeControl {
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.channel
                .request(json!({"subtype": "interrupt"}))
                .await
                .map(|_| ())
        })
    }

    fn set_model(&self, model: Option<String>) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.channel
                .request(json!({"subtype": "set_model", "model": model}))
                .await
                .map(|_| ())
        })
    }

    fn set_permission_policy(
        &self,
        policy: PermissionPolicy,
    ) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let mode = permission_mode_for(policy)?;
            self.channel
                .request(json!({"subtype": "set_permission_mode", "mode": mode}))
                .await
                .map(|_| ())
        })
    }

    fn inject_message(&self, text: String) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move { self.channel.send_user_message(&text) })
    }

    fn context_usage(&self) -> BoxFuture<'_, Result<Value, AgentError>> {
        Box::pin(async move {
            self.channel
                .request(json!({"subtype": "get_context_usage"}))
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_modes_map_to_cli_vocabulary() {
        assert_eq!(
            permission_mode_for(PermissionPolicy::Auto).unwrap(),
            "bypassPermissions"
        );
        assert_eq!(
            permission_mode_for(PermissionPolicy::Supervised).unwrap(),
            "default"
        );
        assert_eq!(permission_mode_for(PermissionPolicy::Plan).unwrap(), "plan");
        assert_eq!(
            permission_mode_for(PermissionPolicy::ReadOnly)
                .unwrap_err()
                .code(),
            "unsupported_capability"
        );
    }
}
