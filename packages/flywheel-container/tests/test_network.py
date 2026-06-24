"""Held-out oracle for spec 00044 G6 — [sandbox.network] enforcement.

RED until G6 lands. ``resolve_network`` maps the policy to a Docker network
choice (allow→default bridge; deny→an --internal network; deny+allow_hosts→an
operator egress network, erroring without one). ContainerSubmitStrategy wires
that into the container start (creating the internal network when needed). The
pure resolver + the wiring are deterministic; a live test confirms an --internal
network blocks egress. Do not weaken assertions.
"""

from __future__ import annotations

import shutil
import subprocess
import uuid
from pathlib import Path

import pytest

from flywheel_container import (
    ClaudeCliAgent,
    ContainerRuntime,
    ContainerSubmitStrategy,
    DEFAULT_INTERNAL_NETWORK,
    ResolvedNetwork,
    resolve_network,
)
from flywheel_container._docker import ensure_internal_network, exec_in_container
from flywheel_orchestrator import SandboxRequest


# --- pure resolver ----------------------------------------------------------


def test_allow_uses_default_bridge() -> None:
    assert resolve_network(policy="allow") == ResolvedNetwork(None, False)


def test_deny_without_hosts_uses_internal_network() -> None:
    assert resolve_network(policy="deny") == ResolvedNetwork(
        DEFAULT_INTERNAL_NETWORK, True
    )


def test_deny_with_hosts_requires_egress_network() -> None:
    with pytest.raises(ValueError, match="egress_network"):
        resolve_network(policy="deny", allow_hosts=["api.github.com"])


def test_deny_with_hosts_and_egress_uses_it() -> None:
    assert resolve_network(
        policy="deny", allow_hosts=["api.github.com"], egress_network="proxy-net"
    ) == ResolvedNetwork("proxy-net", False)


def test_unknown_policy_rejected() -> None:
    with pytest.raises(ValueError, match="unknown sandbox network policy"):
        resolve_network(policy="sideways")


# --- strategy wiring (fake runtime, no docker) ------------------------------


class _Recorder:
    def __init__(self) -> None:
        self.start_network: object = "unset"
        self.internal_created: list[str] = []

    def start(self, name, image, **kwargs):
        self.start_network = kwargs.get("network")
        return name

    def runtime(self) -> ContainerRuntime:
        return ContainerRuntime(
            start=self.start,
            exec_command=lambda *a, **k: None,  # unused here
            remove=lambda name: None,
            register_cleanup=lambda name: (lambda: None),
            ensure_internal_network=self.internal_created.append,
        )


class _Inner:
    def prepare_sandbox(self, request: SandboxRequest):
        return Path("/tmp/wt")

    def submit(self, request) -> None:  # pragma: no cover - unused
        pass


def _make(rec: _Recorder, **kwargs) -> ContainerSubmitStrategy:
    return ContainerSubmitStrategy(
        _Inner(),
        image="img",
        agent=ClaudeCliAgent(model="m"),
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.runtime(),
        **kwargs,
    )


def _req() -> SandboxRequest:
    return SandboxRequest(task_id="t", task_file=Path(), run_id=None, mode="fresh")


def test_deny_policy_creates_and_attaches_internal_network() -> None:
    rec = _Recorder()
    _make(rec, network_policy="deny").prepare_sandbox(_req())
    assert rec.internal_created == [DEFAULT_INTERNAL_NETWORK]
    assert rec.start_network == DEFAULT_INTERNAL_NETWORK


def test_allow_policy_uses_default_bridge_no_creation() -> None:
    rec = _Recorder()
    _make(rec, network_policy="allow").prepare_sandbox(_req())
    assert rec.internal_created == []
    assert rec.start_network is None


def test_deny_with_egress_attaches_proxy_network() -> None:
    rec = _Recorder()
    _make(
        rec,
        network_policy="deny",
        allow_hosts=["api.github.com"],
        egress_network="proxy-net",
    ).prepare_sandbox(_req())
    assert rec.internal_created == []
    assert rec.start_network == "proxy-net"


# --- live: an --internal network really blocks egress -----------------------


def _docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        return (
            subprocess.run(
                ["docker", "info"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            ).returncode
            == 0
        )
    except (OSError, subprocess.SubprocessError):
        return False


@pytest.mark.skipif(not _docker_available(), reason="docker daemon not available")
def test_internal_network_blocks_egress() -> None:
    from flywheel_container._docker import remove_container, start_container

    net = f"flywheel-test-net-{uuid.uuid4().hex[:8]}"
    name = f"flywheel-test-{uuid.uuid4().hex[:8]}"
    ensure_internal_network(net)
    try:
        start_container(
            name, "alpine:latest", network=net, command=["sleep", "infinity"]
        )
        # An --internal network has no gateway, so any egress attempt fails
        # fast and non-zero regardless of the host's own connectivity.
        result = exec_in_container(
            name, "wget -T 3 -t 1 -q -O /dev/null http://1.1.1.1/ ; echo $?"
        )
        assert result.exit_code == 0  # the `echo $?` shell ran
        assert result.stdout.strip().splitlines()[-1] != "0"  # wget failed
    finally:
        remove_container(name)
        subprocess.run(
            ["docker", "network", "rm", net],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
