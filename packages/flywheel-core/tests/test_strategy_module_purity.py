import ast
import inspect

import flywheel_core.strategy as strategy_module


FORBIDDEN_IMPORTS = {
    "subprocess",
    "socket",
    "urllib",
    "http",
    "httpx",
    "requests",
    "aiohttp",
    "git",
}


def _module_source() -> str:
    source_path = inspect.getsourcefile(strategy_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_strategy_module_has_no_subprocess_git_or_network_imports() -> None:
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
    assert not leaked, (
        f"flywheel_core.strategy imports forbidden modules: {leaked}"
    )


def test_strategy_module_namespace_exposes_no_forbidden_apis() -> None:
    for name in FORBIDDEN_IMPORTS:
        assert not hasattr(strategy_module, name), (
            f"flywheel_core.strategy unexpectedly exposes {name!r}"
        )
