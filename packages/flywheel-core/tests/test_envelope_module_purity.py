import ast
import inspect

import flywheel_core.envelope as envelope_module


FORBIDDEN_IMPORTS = {
    "pathlib",
    "io",
    "os",
    "subprocess",
    "socket",
    "urllib",
    "requests",
    "httpx",
    "logging",
    "claude_agent_sdk",
}
FORBIDDEN_BUILTINS = {"open"}


def _module_source() -> str:
    source_path = inspect.getsourcefile(envelope_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_envelope_module_has_no_io_sdk_or_network_imports() -> None:
    tree = ast.parse(_module_source())
    seen: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                seen.add(alias.name.split(".")[0])
        elif isinstance(node, ast.ImportFrom):
            if node.module:
                seen.add(node.module.split(".")[0])
    leaked = seen & FORBIDDEN_IMPORTS
    assert not leaked, f"flywheel_core.envelope imports forbidden modules: {leaked}"


def test_envelope_module_does_not_call_open_or_file_apis() -> None:
    tree = ast.parse(_module_source())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            assert node.func.id not in FORBIDDEN_BUILTINS, (
                f"flywheel_core.envelope calls forbidden builtin {node.func.id!r}"
            )
