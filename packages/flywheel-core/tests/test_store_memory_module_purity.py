import ast
import inspect

import flywheel_core.store_memory as store_memory_module


# The in-memory store is the test substrate — durable persistence belongs
# to the sibling SQLite store, not here.
FORBIDDEN_IMPORTS = {
    "sqlite3",
    "pathlib",
    "io",
    "socket",
    "urllib",
    "http",
    "shelve",
    "pickle",
}
FORBIDDEN_BUILTINS = {"open"}


def _module_source() -> str:
    source_path = inspect.getsourcefile(store_memory_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_in_memory_store_module_has_no_persistence_or_file_imports() -> None:
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
        f"flywheel_core.store_memory imports forbidden modules: {leaked}"
    )


def test_in_memory_store_module_does_not_call_open() -> None:
    tree = ast.parse(_module_source())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            assert node.func.id not in FORBIDDEN_BUILTINS, (
                f"flywheel_core.store_memory calls forbidden builtin "
                f"{node.func.id!r}"
            )
