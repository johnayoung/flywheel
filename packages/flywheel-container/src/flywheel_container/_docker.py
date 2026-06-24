"""Docker lifecycle primitives for the container sandbox backend.

Increment G3 of [spec 00044]. SDK-free: pure ``subprocess`` orchestration of the
``docker`` CLI — the agent runs as *its own* CLI inside the image, so nothing
here imports ``claude_agent_sdk``.

The argv construction is factored into pure ``build_*_argv`` functions so the
flag mapping is unit-testable without a running daemon; the ``start_container``
/ ``exec_in_container`` / ``remove_container`` wrappers add the subprocess I/O.

Patterns adopted from ``mattpocock/sandcastle`` (see the project memory): a
single long-lived container per sandbox (``docker run -d`` against an image
whose ``ENTRYPOINT`` is ``sleep infinity``), ``docker exec`` per command,
line-streamed stdout with a bounded tail, UID/GID alignment for bind mounts,
and a pre-flight image+UID check.
"""

from __future__ import annotations

import atexit
import subprocess
import threading
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass

DEFAULT_AGENT_HOME = "/home/agent"
DEFAULT_WORKDIR = f"{DEFAULT_AGENT_HOME}/workspace"
# Cap the retained stdout/stderr tail (64 KiB/stream) so a long agent run cannot
# accumulate unbounded output in memory; lines are still delivered live to
# ``on_line`` as they arrive.
DEFAULT_MAX_TAIL_CHARS = 64 * 1024


class DockerError(RuntimeError):
    """A ``docker`` invocation failed (non-zero exit or spawn error)."""


@dataclass(frozen=True)
class ExecResult:
    """Outcome of one ``docker exec`` — bounded tails plus the exit code."""

    stdout: str
    stderr: str
    exit_code: int


@dataclass(frozen=True, kw_only=True)
class VolumeMount:
    """A host→container bind mount (``-v host:sandbox[:ro]``)."""

    host_path: str
    sandbox_path: str
    readonly: bool = False

    def to_flag(self) -> str:
        suffix = ":ro" if self.readonly else ""
        return f"{self.host_path}:{self.sandbox_path}{suffix}"


class _BoundedTail:
    """Accumulate text but retain only the last ``max_chars`` characters."""

    def __init__(self, max_chars: int, sep: str = "") -> None:
        self._max = max_chars
        self._sep = sep
        self._s = ""

    def push(self, chunk: str) -> None:
        self._s = (self._s + self._sep + chunk) if self._s else chunk
        if len(self._s) > self._max:
            self._s = self._s[-self._max :]

    def __str__(self) -> str:
        return self._s


def _normalize_networks(
    network: str | Sequence[str] | None,
) -> tuple[str, ...]:
    if network is None:
        return ()
    if isinstance(network, str):
        return (network,)
    return tuple(network)


def build_run_argv(
    name: str,
    image: str,
    *,
    env: Mapping[str, str] | None = None,
    mounts: Sequence[VolumeMount] = (),
    workdir: str | None = None,
    user: str | None = None,
    network: str | Sequence[str] | None = None,
    groups: Sequence[str | int] = (),
    devices: Sequence[str] = (),
    cpus: float | None = None,
    command: Sequence[str] = (),
) -> list[str]:
    """Build the ``docker run -d`` argv. Pure — no subprocess.

    Flag order is deterministic (env, volumes, workdir, user, network, groups,
    devices, cpus) so it is assertable in tests. The container is detached; it
    stays alive for subsequent ``docker exec`` calls via the image's
    ``sleep infinity`` entrypoint, or via an explicit ``command`` override
    (appended after the image — e.g. a generic base image in tests).
    """
    argv = ["docker", "run", "-d", "--name", name]
    for key, value in (env or {}).items():
        argv += ["-e", f"{key}={value}"]
    for mount in mounts:
        argv += ["-v", mount.to_flag()]
    if workdir is not None:
        argv += ["-w", workdir]
    if user is not None:
        argv += ["--user", user]
    for net in _normalize_networks(network):
        argv += ["--network", net]
    for group in groups:
        argv += ["--group-add", str(group)]
    for device in devices:
        argv += ["--device", device]
    if cpus is not None:
        argv += ["--cpus", str(cpus)]
    argv.append(image)
    argv.extend(command)
    return argv


def build_exec_argv(
    name: str,
    command: str,
    *,
    interactive: bool = False,
    cwd: str | None = None,
) -> list[str]:
    """Build a ``docker exec ... sh -c <command>`` argv. Pure — no subprocess.

    ``interactive`` adds ``-i`` so a prompt can be piped on stdin (avoiding the
    Linux per-arg ``ARG_MAX`` limit). ``cwd`` sets the working directory.
    """
    argv = ["docker", "exec"]
    if interactive:
        argv.append("-i")
    if cwd is not None:
        argv += ["-w", cwd]
    argv += [name, "sh", "-c", command]
    return argv


def _run_docker(argv: Sequence[str], *, timeout: float | None = None) -> str:
    """Run a docker subcommand, returning stdout; raise ``DockerError`` on
    failure."""
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise DockerError(f"docker {argv[1] if len(argv) > 1 else ''} failed: {exc}") from exc
    if proc.returncode != 0:
        raise DockerError(
            f"docker {argv[1] if len(argv) > 1 else ''} failed "
            f"(exit {proc.returncode}): {proc.stderr.strip() or proc.stdout.strip()}"
        )
    return proc.stdout


def image_exists(image: str) -> bool:
    """Whether ``image`` is present locally."""
    try:
        _run_docker(["docker", "image", "inspect", image, "--format", "{{.Id}}"])
        return True
    except DockerError:
        return False


def check_image_uid(image: str, expected_uid: int) -> None:
    """Pre-flight: the image must exist and its baked ``USER`` UID (when
    numeric) must equal ``expected_uid``.

    Misaligned UIDs are the #1 bind-mount gotcha: files the container writes
    land owned by the wrong host user and the orchestrator cannot read them
    back. A non-numeric or absent ``USER`` cannot be compared and is allowed.
    """
    try:
        out = _run_docker(
            ["docker", "image", "inspect", image, "--format", "{{.Config.User}}"]
        )
    except DockerError as exc:
        raise DockerError(
            f"image {image!r} not found locally; build it first"
        ) from exc
    user = out.strip()
    if not user:
        return
    uid_part = user.split(":", 1)[0]
    try:
        image_uid = int(uid_part)
    except ValueError:
        return
    if image_uid != expected_uid:
        raise DockerError(
            f"UID mismatch: image {image!r} was built with UID {image_uid}, "
            f"but the host expects {expected_uid}. Rebuild the image with "
            f"--build-arg AGENT_UID={expected_uid}, or pass container_uid="
            f"{image_uid} to match the image."
        )


def start_container(
    name: str,
    image: str,
    *,
    env: Mapping[str, str] | None = None,
    mounts: Sequence[VolumeMount] = (),
    workdir: str | None = None,
    user: str | None = None,
    network: str | Sequence[str] | None = None,
    groups: Sequence[str | int] = (),
    devices: Sequence[str] = (),
    cpus: float | None = None,
    command: Sequence[str] = (),
    timeout: float | None = 120.0,
) -> str:
    """``docker run -d`` a detached container; return its id."""
    argv = build_run_argv(
        name,
        image,
        env=env,
        mounts=mounts,
        workdir=workdir,
        user=user,
        network=network,
        groups=groups,
        devices=devices,
        cpus=cpus,
        command=command,
    )
    return _run_docker(argv, timeout=timeout).strip()


def exec_in_container(
    name: str,
    command: str,
    *,
    stdin: str | None = None,
    cwd: str | None = None,
    on_line: Callable[[str], None] | None = None,
    max_tail_chars: int = DEFAULT_MAX_TAIL_CHARS,
    timeout: float | None = None,
) -> ExecResult:
    """``docker exec`` a shell command, streaming stdout line-by-line.

    When ``on_line`` is set each stdout line is delivered live (for the agent
    stream parser / idle-timeout) and only a bounded tail is retained in the
    returned :class:`ExecResult`. ``stdin`` is written on a dedicated thread so
    a large prompt cannot deadlock against the child's early output.
    """
    argv = build_exec_argv(name, command, interactive=stdin is not None, cwd=cwd)
    try:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError as exc:
        raise DockerError(f"docker exec failed to spawn: {exc}") from exc

    stdout_tail = _BoundedTail(max_tail_chars, sep="\n")
    stderr_tail = _BoundedTail(max_tail_chars)

    def _write_stdin() -> None:
        assert proc.stdin is not None
        try:
            proc.stdin.write(stdin or "")
        except (BrokenPipeError, OSError):
            pass
        finally:
            try:
                proc.stdin.close()
            except OSError:
                pass

    def _drain_stderr() -> None:
        assert proc.stderr is not None
        for chunk in proc.stderr:
            stderr_tail.push(chunk)

    threads: list[threading.Thread] = []
    if stdin is not None:
        t = threading.Thread(target=_write_stdin, daemon=True)
        t.start()
        threads.append(t)
    stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
    stderr_thread.start()
    threads.append(stderr_thread)

    assert proc.stdout is not None
    try:
        for raw in proc.stdout:
            line = raw.rstrip("\n")
            stdout_tail.push(line)
            if on_line is not None:
                on_line(line)
        exit_code = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        raise DockerError(f"docker exec timed out after {timeout}s")
    finally:
        for t in threads:
            t.join(timeout=5.0)

    return ExecResult(
        stdout=str(stdout_tail),
        stderr=str(stderr_tail),
        exit_code=exit_code,
    )


def remove_container(name: str) -> None:
    """Stop and remove a container, best-effort (ignores 'not found')."""
    for sub in (["docker", "stop", name], ["docker", "rm", name]):
        try:
            _run_docker(sub)
        except DockerError:
            pass


def force_remove_container_sync(name: str) -> None:
    """``docker rm -f`` a container, swallowing all errors (crash cleanup)."""
    try:
        subprocess.run(
            ["docker", "rm", "-f", name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        pass


# --- crash-safety cleanup registry -----------------------------------------
# A best-effort backstop to the explicit per-run SandboxHandle.teardown (G1):
# any container still registered at interpreter exit is force-removed. Signal
# handling is intentionally NOT hijacked here (a library must not fight the
# worker's own SIGINT/SIGTERM handling) — atexit covers normal exit and
# uncaught exceptions; the explicit teardown covers the steady-state path.

_registry_lock = threading.Lock()
_registered: set[str] = set()
_atexit_installed = False
# Indirection so tests can flush against a fake remover without touching docker.
_remover: Callable[[str], None] = force_remove_container_sync


def register_container_cleanup(name: str) -> Callable[[], None]:
    """Register ``name`` for force-removal at interpreter exit; returns an
    unregister callback to call once the container is disposed normally."""
    global _atexit_installed
    with _registry_lock:
        _registered.add(name)
        if not _atexit_installed:
            atexit.register(_flush_cleanup_registry)
            _atexit_installed = True

    def _unregister() -> None:
        with _registry_lock:
            _registered.discard(name)

    return _unregister


def _flush_cleanup_registry() -> None:
    with _registry_lock:
        names = list(_registered)
        _registered.clear()
    for name in names:
        try:
            _remover(name)
        except Exception:  # noqa: BLE001 - cleanup is strictly best-effort
            pass
