"""Module-purity tests for :mod:`flywheel_core.prompt`.

The prompt builder operates only on its typed inputs (Task, Lifecycle,
IterationInputs) plus stable envelope constants. It must not import IO,
file, network, or SDK APIs, and must not call ``open()``.
"""

import ast
import inspect

import flywheel_core.prompt as prompt_module


FORBIDDEN_IMPORTS = {
    "json",
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
    source_path = inspect.getsourcefile(prompt_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_prompt_module_has_no_io_sdk_or_network_imports() -> None:
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
    assert not leaked, f"flywheel_core.prompt imports forbidden modules: {leaked}"


def test_prompt_module_does_not_call_open_or_file_apis() -> None:
    tree = ast.parse(_module_source())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            assert node.func.id not in FORBIDDEN_BUILTINS, (
                f"flywheel_core.prompt calls forbidden builtin {node.func.id!r}"
            )


def test_prompt_module_imports_only_flywheel_typed_inputs_and_stdlib() -> None:
    tree = ast.parse(_module_source())
    third_party: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                root = alias.name.split(".")[0]
                if root != "flywheel_core":
                    third_party.add(root)
        elif isinstance(node, ast.ImportFrom):
            if node.module:
                root = node.module.split(".")[0]
                if root != "flywheel_core":
                    third_party.add(root)

    allowed_stdlib = {"dataclasses", "typing", "enum", "collections"}
    leaked = third_party - allowed_stdlib
    assert not leaked, (
        f"flywheel_core.prompt imports outside the allowed stdlib subset: {leaked}"
    )
