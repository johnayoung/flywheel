//! `[sandbox.network]` to Docker network resolution (spec 00044 G6).
//!
//! The honest network model: the orchestrator owns *attachment*, the
//! operator owns the *proxy*. Network policy is mapped to a Docker
//! `--network` choice rather than a bespoke iptables/netns firewall:
//!
//! - `allow` (default): the default bridge; full egress.
//! - `deny` with no `allow_hosts`: a Docker `--internal` network, which has
//!   no external route at all. Real, verifiable enforcement.
//! - `deny` with `allow_hosts`: an operator-provisioned egress-proxy network
//!   (named via config). flywheel provides the attachment point; the
//!   allowlisting proxy is the operator's. Without one configured this is an
//!   error, never a silent full-egress fallback.
//!
//! Pure: takes the policy as primitives so the crate stays decoupled from
//! the orchestrator's `SandboxNetwork`; the side effect (creating the
//! internal network) lives in [`crate::docker`].

/// The `--internal` network created for `deny` with no `allow_hosts`.
pub const DEFAULT_INTERNAL_NETWORK: &str = "flywheel-internal";

/// The Docker network choice for a run.
///
/// `name` is the `docker run --network` value (`None` = the default
/// bridge). `ensure_internal` is true when `name` must first be created as a
/// `--internal` network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNetwork {
    pub name: Option<String>,
    pub ensure_internal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetworkError {
    #[error("unknown sandbox network policy {0:?} (expected 'allow' or 'deny')")]
    UnknownPolicy(String),
    #[error(
        "network policy 'deny' with allow_hosts requires an operator-provisioned egress_network \
         (an allowlisting proxy network); none was configured. Under the container backend \
         flywheel attaches the sandbox to that network but does not run the proxy itself."
    )]
    EgressNetworkRequired,
}

/// Resolve `[sandbox.network]` to a Docker network choice. Pure.
///
/// An empty `egress_network` counts as unset.
pub fn resolve_network(
    policy: &str,
    allow_hosts: &[String],
    egress_network: Option<&str>,
    internal_network: &str,
) -> Result<ResolvedNetwork, NetworkError> {
    match policy {
        "allow" => Ok(ResolvedNetwork {
            name: None,
            ensure_internal: false,
        }),
        "deny" if !allow_hosts.is_empty() => match egress_network {
            Some(name) if !name.is_empty() => Ok(ResolvedNetwork {
                name: Some(name.to_owned()),
                ensure_internal: false,
            }),
            _ => Err(NetworkError::EgressNetworkRequired),
        },
        // deny + no allow_hosts: fully isolated, no egress.
        "deny" => Ok(ResolvedNetwork {
            name: Some(internal_network.to_owned()),
            ensure_internal: true,
        }),
        other => Err(NetworkError::UnknownPolicy(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn allow_uses_default_bridge() {
        assert_eq!(
            resolve_network("allow", &[], None, DEFAULT_INTERNAL_NETWORK).unwrap(),
            ResolvedNetwork {
                name: None,
                ensure_internal: false
            }
        );
    }

    #[test]
    fn deny_without_hosts_uses_internal_network() {
        assert_eq!(
            resolve_network("deny", &[], None, DEFAULT_INTERNAL_NETWORK).unwrap(),
            ResolvedNetwork {
                name: Some(DEFAULT_INTERNAL_NETWORK.into()),
                ensure_internal: true
            }
        );
    }

    #[test]
    fn deny_with_hosts_requires_egress_network() {
        let err = resolve_network(
            "deny",
            &hosts(&["api.github.com"]),
            None,
            DEFAULT_INTERNAL_NETWORK,
        )
        .unwrap_err();
        assert_eq!(err, NetworkError::EgressNetworkRequired);
        assert!(err.to_string().contains("egress_network"));
        // An empty name is "unset", never a silent fallback.
        assert_eq!(
            resolve_network("deny", &hosts(&["h"]), Some(""), DEFAULT_INTERNAL_NETWORK)
                .unwrap_err(),
            NetworkError::EgressNetworkRequired
        );
    }

    #[test]
    fn deny_with_hosts_and_egress_uses_it() {
        assert_eq!(
            resolve_network(
                "deny",
                &hosts(&["api.github.com"]),
                Some("proxy-net"),
                DEFAULT_INTERNAL_NETWORK
            )
            .unwrap(),
            ResolvedNetwork {
                name: Some("proxy-net".into()),
                ensure_internal: false
            }
        );
    }

    #[test]
    fn unknown_policy_rejected() {
        let err = resolve_network("sideways", &[], None, DEFAULT_INTERNAL_NETWORK).unwrap_err();
        assert_eq!(err, NetworkError::UnknownPolicy("sideways".into()));
        assert!(
            err.to_string()
                .starts_with("unknown sandbox network policy")
        );
    }
}
