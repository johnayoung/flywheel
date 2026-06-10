import ast
import inspect

import flywheel_core.task as task_module


FORBIDDEN_IMPORTS = {"json", "pathlib", "io"}
FORBIDDEN_BUILTINS = {"open"}


def _module_source() -> str:
    source_path = inspect.getsourcefile(task_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_task_module_has_no_file_or_stream_imports() -> None:
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
    assert not leaked, f"flywheel_core.task imports forbidden modules: {leaked}"


def test_task_module_does_not_call_open_or_file_apis() -> None:
    tree = ast.parse(_module_source())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            assert node.func.id not in FORBIDDEN_BUILTINS, (
                f"flywheel_core.task calls forbidden builtin {node.func.id!r}"
            )


def test_task_dataclass_constructor_takes_no_path_or_payload_args() -> None:
    from flywheel_core.task import Task

    field_names = {f.name for f in Task.__dataclass_fields__.values()}
    for forbidden in {"path", "file", "filename", "filepath", "payload", "raw", "json", "source"}:
        assert forbidden not in field_names, (
            f"Task field {forbidden!r} hints at input-format coupling"
        )
