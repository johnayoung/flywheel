"""``[sandbox.network]`` → Docker network resolution (spec 00044 G6).

The honest network model: the orchestrator owns *attachment*, the operator owns
the *proxy*. Network policy is mapped to a Docker ``--network`` choice rather
than a bespoke iptables/netns firewall (fragile, and not what a shipped tool —
``mattpocock/sandcastle`` — does):

- ``allow`` (default) → the default bridge; full egress, byte-identical to today.
- ``deny`` with no ``allow_hosts`` → a Docker ``--internal`` network, which has
  no external route at all. Real, verifiable enforcement.
- ``deny`` with ``allow_hosts`` → an operator-provisioned egress-proxy network
  (named via config). flywheel provides the attachment point; the allowlisting
  proxy is the operator's. Without one configured this is an error, never a
  silent full-egress fallback.

Pure: takes the policy as primitives (``policy`` str, ``allow_hosts``) so the
package stays decoupled from the orchestrator's ``SandboxNetwork`` dataclass;
the side effect (creating the internal network) lives in ``_docker``.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

DEFAULT_INTERNAL_NETWORK = "flywheel-internal"


@dataclass(frozen=True)
class ResolvedNetwork:
    """The Docker network choice for a run.

    ``name`` is the ``docker run --network`` value (``None`` = the default
    bridge). ``ensure_internal`` is ``True`` when ``name`` must first be created
    as a ``--internal`` network.
    """

    name: str | None
    ensure_internal: bool


def resolve_network(
    *,
    policy: str = "allow",
    allow_hosts: Sequence[str] = (),
    egress_network: str | None = None,
    internal_network: str = DEFAULT_INTERNAL_NETWORK,
) -> ResolvedNetwork:
    """Resolve ``[sandbox.network]`` to a Docker network choice. Pure."""
    if policy == "allow":
        return ResolvedNetwork(name=None, ensure_internal=False)
    if policy != "deny":
        raise ValueError(
            f"unknown sandbox network policy {policy!r} (expected 'allow' or 'deny')"
        )
    if allow_hosts:
        if not egress_network:
            raise ValueError(
                "network policy 'deny' with allow_hosts requires an "
                "operator-provisioned egress_network (an allowlisting proxy "
                "network); none was configured. Under the container backend "
                "flywheel attaches the sandbox to that network but does not "
                "run the proxy itself."
            )
        return ResolvedNetwork(name=egress_network, ensure_internal=False)
    # deny + no allow_hosts → fully isolated, no egress.
    return ResolvedNetwork(name=internal_network, ensure_internal=True)
