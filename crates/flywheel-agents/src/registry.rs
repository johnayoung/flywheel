//! Adapter registry: first-party registration plus caller-supplied plugins.
//!
//! Python discovered third-party adapters through the
//! `flywheel_agents.adapters` entry-point group. Rust has no runtime
//! entry-point mechanism, so plugins register through
//! [`AdapterRegistry::register_plugin`]: a factory that fails (returns an
//! error or panics) is recorded on `load_failures` and never prevents the
//! registry from loading.

use crate::adapter::{AgentAdapter, PluginLoadFailure};
use crate::errors::AgentError;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

#[derive(Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
    pub load_failures: Vec<PluginLoadFailure>,
}

impl std::fmt::Debug for AdapterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterRegistry")
            .field("ids", &self.ids())
            .field("load_failures", &self.load_failures)
            .finish()
    }
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, adapter: Arc<dyn AgentAdapter>) -> Result<(), AgentError> {
        let agent_id = adapter.descriptor().id.clone();
        if self.adapters.contains_key(&agent_id) {
            return Err(AgentError::AdapterAlreadyRegistered(format!(
                "adapter already registered for {agent_id:?}"
            )));
        }
        self.adapters.insert(agent_id, adapter);
        Ok(())
    }

    /// Register a third-party adapter through a fallible factory. A factory
    /// error, a duplicate id, or a panic inside the factory is recorded and
    /// swallowed; the registry stays usable.
    pub fn register_plugin<F>(&mut self, name: &str, factory: F)
    where
        F: FnOnce() -> Result<Arc<dyn AgentAdapter>, String>,
    {
        let outcome = catch_unwind(AssertUnwindSafe(factory));
        let result = match outcome {
            Ok(Ok(adapter)) => self.register(adapter).map_err(|err| err.to_string()),
            Ok(Err(error)) => Err(error),
            Err(panic) => Err(panic_message(panic.as_ref())),
        };
        if let Err(error) = result {
            self.load_failures.push(PluginLoadFailure {
                name: name.to_string(),
                error,
            });
        }
    }

    pub fn get(&self, agent_id: &str) -> Result<Arc<dyn AgentAdapter>, AgentError> {
        self.adapters.get(agent_id).cloned().ok_or_else(|| {
            let known = if self.adapters.is_empty() {
                "<none>".to_string()
            } else {
                self.ids().join(", ")
            };
            AgentError::UnknownAgent(format!(
                "no adapter registered for {agent_id:?} (registered: {known})"
            ))
        })
    }

    pub fn ids(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        return format!("plugin factory panicked: {message}");
    }
    if let Some(message) = panic.downcast_ref::<String>() {
        return format!("plugin factory panicked: {message}");
    }
    "plugin factory panicked".to_string()
}

/// First-party adapters: claude-code and codex.
pub fn default_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    // Both ids are distinct constants; registration cannot collide.
    let _ = registry.register(Arc::new(crate::claude_code::ClaudeCodeAdapter::new()));
    let _ = registry.register(Arc::new(crate::codex::CodexAdapter::new()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeAdapter;

    #[test]
    fn get_unknown_id_names_registered_ids() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(ClaudeCodeAdapter::new()))
            .unwrap();
        let Err(err) = registry.get("codex") else {
            panic!("expected unknown agent")
        };
        let message = err.to_string();
        assert!(message.contains("codex"));
        assert!(message.contains("claude-code"));
        assert_eq!(err.code(), "unknown_agent");
    }

    #[test]
    fn get_on_empty_registry_says_none() {
        let Err(err) = AdapterRegistry::new().get("anything") else {
            panic!("expected unknown agent")
        };
        assert!(err.to_string().contains("<none>"));
    }

    #[test]
    fn duplicate_register_is_an_error() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(ClaudeCodeAdapter::new()))
            .unwrap();
        let err = registry
            .register(Arc::new(ClaudeCodeAdapter::new()))
            .unwrap_err();
        assert_eq!(err.code(), "adapter_already_registered");
    }

    #[test]
    fn default_registry_contains_both_first_party_adapters() {
        let ids = default_registry().ids();
        assert_eq!(ids, vec!["claude-code".to_string(), "codex".to_string()]);
    }

    #[test]
    fn failing_plugin_is_recorded_not_fatal() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(ClaudeCodeAdapter::new()))
            .unwrap();
        registry.register_plugin("exploding-plugin", || {
            Err("plugin import exploded".to_string())
        });
        registry.register_plugin("panicking-plugin", || panic!("factory blew up"));
        registry.register_plugin("duplicate-plugin", || {
            Ok(Arc::new(ClaudeCodeAdapter::new()) as Arc<dyn AgentAdapter>)
        });
        assert_eq!(registry.load_failures.len(), 3);
        assert_eq!(registry.load_failures[0].name, "exploding-plugin");
        assert!(
            registry.load_failures[0]
                .error
                .contains("plugin import exploded")
        );
        assert!(registry.load_failures[1].error.contains("factory blew up"));
        assert!(
            registry.load_failures[2]
                .error
                .contains("already registered")
        );
        assert_eq!(registry.ids(), vec!["claude-code".to_string()]);
        assert!(registry.get("claude-code").is_ok());
    }
}
