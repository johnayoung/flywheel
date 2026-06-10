import ast
import inspect

import flywheel_core.redaction as redaction_module


# Mirror the discipline of tests/test_task_module_purity.py and
# tests/test_lifecycle_module_purity.py: the redaction core must not touch
# files, JSON, or path APIs. `os`, `re`, `hmac`, `hashlib` are intentionally
# NOT forbidden -- the upcoming built-in redactors (env-value, HMAC tokens)
# need them.
FORBIDDEN_IMPORTS = {"json", "pathlib", "io"}
FORBIDDEN_BUILTINS = {"open"}


def _module_source() -> str:
    source_path = inspect.getsourcefile(redaction_module)
    assert source_path is not None
    with open(source_path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


def test_redaction_module_has_no_file_or_stream_imports() -> None:
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
    assert not leaked, f"flywheel_core.redaction imports forbidden modules: {leaked}"


def test_redaction_module_does_not_call_open_or_file_apis() -> None:
    tree = ast.parse(_module_source())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            assert node.func.id not in FORBIDDEN_BUILTINS, (
                f"flywheel_core.redaction calls forbidden builtin {node.func.id!r}"
            )


def test_redaction_module_exposes_documented_public_surface() -> None:
    from flywheel_core.redaction import PatternRedactor, Redactor, compose

    assert Redactor is not None
    assert PatternRedactor is not None
    assert callable(compose)
