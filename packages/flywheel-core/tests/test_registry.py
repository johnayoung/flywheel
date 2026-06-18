"""The plugin registry mechanism: name resolution and branded failures.

Exercises :mod:`flywheel_core._registry` in isolation, with specs that point
at stable standard-library targets so the test never depends on a real
backend. The missing-extra path is proved against deliberately non-existent
modules (and a synthetic module whose body raises ``ImportError``) so it runs
identically with or without any optional extra installed.
"""

from __future__ import annotations

import importlib
import importlib.metadata
import json
import sys
import textwrap
from pathlib import Path
from types import ModuleType
from typing import Callable, Iterator

import pytest

from flywheel_core._registry import (
    MissingExtraError,
    PluginSpec,
    Registry,
    UnknownPluginError,
    import_extra,
    install_hint,
)


def _registry() -> Registry:
    reg = Registry("widget", "flywheel.widgets")
    # json.JSONDecoder is a stable, always-importable resolution target.
    reg.register(
        PluginSpec(name="default", target="json:JSONDecoder", summary="json")
    )
    reg.register(
        PluginSpec(
            name="optional",
            target="flywheel_core._registry_absent:Thing",
            extra="widgetext",
        )
    )
    return reg


# --------------------------------------------------------------------------
# Contract 1: resolve returns the genuine target object (identity).
# --------------------------------------------------------------------------


def test_resolve_returns_the_genuine_target() -> None:
    reg = _registry()
    assert reg.resolve("default") is json.JSONDecoder


def test_resolve_resolves_a_nested_attribute_target() -> None:
    # A second stable target proves resolution is by name, not a fixture quirk.
    reg = Registry("widget", "flywheel.widgets")
    reg.register(PluginSpec(name="enc", target="json:JSONEncoder"))
    assert reg.resolve("enc") is json.JSONEncoder


# --------------------------------------------------------------------------
# Contract 2: names()/specs() preserve registration order; specs are PluginSpec.
# --------------------------------------------------------------------------


def test_names_preserve_registration_order() -> None:
    assert _registry().names() == ("default", "optional")


def test_specs_preserve_registration_order_and_return_pluginspecs() -> None:
    reg = _registry()
    specs = reg.specs()
    assert tuple(s.name for s in specs) == ("default", "optional")
    assert all(isinstance(s, PluginSpec) for s in specs)
    # The actual spec objects round-trip their declared fields.
    assert specs[0].target == "json:JSONDecoder"
    assert specs[0].summary == "json"
    assert specs[1].extra == "widgetext"


def test_names_and_specs_are_empty_for_a_fresh_registry() -> None:
    reg = Registry("widget", "flywheel.widgets")
    assert reg.names() == ()
    assert reg.specs() == ()


# --------------------------------------------------------------------------
# Contract 3: unknown name -> UnknownPluginError.
# --------------------------------------------------------------------------


def test_unknown_name_lists_registered_choices() -> None:
    reg = _registry()
    with pytest.raises(UnknownPluginError) as excinfo:
        reg.resolve("nope")
    err = excinfo.value
    message = str(err)
    # Family noun, the bad name, and every registered name appear.
    assert "widget" in message
    assert "'nope'" in message
    assert "'default'" in message and "'optional'" in message
    # Attributes are populated for programmatic handling.
    assert err.family == "widget"
    assert err.name == "nope"
    assert err.known == ("default", "optional")


def test_unknown_name_on_empty_registry_says_none() -> None:
    reg = Registry("widget", "flywheel.widgets")
    with pytest.raises(UnknownPluginError) as excinfo:
        reg.resolve("anything")
    err = excinfo.value
    assert "(none)" in str(err)
    assert err.known == ()


def test_unknown_plugin_error_is_a_lookup_error() -> None:
    # Callers may catch the generic LookupError.
    with pytest.raises(LookupError):
        _registry().resolve("nope")


# --------------------------------------------------------------------------
# Contract 4: missing extra -> MissingExtraError, branded, ImportError family.
# --------------------------------------------------------------------------


def test_missing_extra_is_branded_with_install_hint() -> None:
    reg = _registry()
    with pytest.raises(MissingExtraError) as excinfo:
        reg.resolve("optional")
    err = excinfo.value
    assert err.extra == "widgetext"
    assert install_hint("widgetext") in str(err)


def test_missing_extra_is_a_module_not_found_error() -> None:
    reg = _registry()
    with pytest.raises(ModuleNotFoundError):
        reg.resolve("optional")


def test_missing_extra_is_an_import_error() -> None:
    # Existing `except ImportError` paths must still catch the branded error.
    reg = _registry()
    with pytest.raises(ImportError):
        reg.resolve("optional")


def test_missing_extra_error_subclass_relationships() -> None:
    # Belt and braces: the class hierarchy itself, not just one raise site.
    assert issubclass(MissingExtraError, ModuleNotFoundError)
    assert issubclass(MissingExtraError, ImportError)


def test_install_hint_names_the_product_dist_and_extra() -> None:
    hint = install_hint("postgres")
    assert "flywheel[postgres]" in hint
    # Not the core dist name; the product shell re-exports the extras.
    assert "flywheel-core" not in hint


# --------------------------------------------------------------------------
# Contract 5: import_extra brands BOTH an absent module AND a module whose
# body raises a crafted ImportError. A present module passes through.
# --------------------------------------------------------------------------


def test_import_extra_passes_through_a_present_module() -> None:
    mod = import_extra("json", "anything")
    assert isinstance(mod, ModuleType)
    assert mod is json
    assert mod.__name__ == "json"


def test_import_extra_brands_a_truly_absent_module() -> None:
    with pytest.raises(MissingExtraError) as excinfo:
        import_extra("flywheel_core._definitely_absent_module", "demo")
    assert excinfo.value.extra == "demo"
    assert install_hint("demo") in str(excinfo.value)


@pytest.fixture
def _tmp_sys_path(tmp_path: Path) -> Iterator[Path]:
    """Put ``tmp_path`` on ``sys.path`` and restore module/path state after."""
    sys.path.insert(0, str(tmp_path))
    before = set(sys.modules)
    try:
        yield tmp_path
    finally:
        try:
            sys.path.remove(str(tmp_path))
        except ValueError:
            pass
        for name in set(sys.modules) - before:
            sys.modules.pop(name, None)
        importlib.invalidate_caches()


def test_import_extra_brands_a_module_whose_body_raises_import_error(
    _tmp_sys_path: Path,
) -> None:
    # This is the flywheel_core.store_postgres pattern: a module that is
    # present and importable as a file, but whose top-level body guards an
    # optional dependency and re-raises a crafted ImportError naming the extra.
    mod_path = _tmp_sys_path / "_reg_self_raising.py"
    mod_path.write_text(
        textwrap.dedent(
            """
            try:
                import _reg_nonexistent_dependency  # noqa: F401
            except ImportError as exc:  # pragma: no cover - exercised by test
                raise ImportError("the gated dependency is absent") from exc
            """
        )
    )
    with pytest.raises(MissingExtraError) as excinfo:
        import_extra("_reg_self_raising", "selfraise")
    assert excinfo.value.extra == "selfraise"
    assert install_hint("selfraise") in str(excinfo.value)


# --------------------------------------------------------------------------
# Contract 9: a NON-ImportError raised during import must propagate unchanged.
# --------------------------------------------------------------------------


def test_import_extra_does_not_mask_a_non_import_error(
    _tmp_sys_path: Path,
) -> None:
    # A module that fails to import for a reason unrelated to a missing
    # dependency (here a ValueError) must surface that genuine error, not be
    # mislabeled as a missing extra. Masking it would hide real bugs behind a
    # spurious "install the extra" hint.
    mod_path = _tmp_sys_path / "_reg_value_error.py"
    mod_path.write_text('raise ValueError("genuine import-time failure")\n')
    with pytest.raises(ValueError, match="genuine import-time failure"):
        import_extra("_reg_value_error", "demo")


# --------------------------------------------------------------------------
# Contract 6: resolve with extra=None and a missing target module raises the
# RAW ModuleNotFoundError, never a branded MissingExtraError.
# --------------------------------------------------------------------------


def test_resolve_without_extra_raises_raw_module_not_found() -> None:
    reg = Registry("widget", "flywheel.widgets")
    reg.register(
        PluginSpec(name="bare", target="flywheel_core._registry_absent:Thing")
    )
    with pytest.raises(ModuleNotFoundError) as excinfo:
        reg.resolve("bare")
    # Specifically NOT branded: no extra was declared, so no install hint.
    assert not isinstance(excinfo.value, MissingExtraError)


def test_resolve_without_extra_propagates_non_import_error(
    _tmp_sys_path: Path,
) -> None:
    # A declared-but-genuinely-broken target with no extra should surface its
    # real error, not be reinterpreted.
    mod_path = _tmp_sys_path / "_reg_resolve_value_error.py"
    mod_path.write_text('raise ValueError("broken target module")\n')
    reg = Registry("widget", "flywheel.widgets")
    reg.register(
        PluginSpec(name="broken", target="_reg_resolve_value_error:Thing")
    )
    with pytest.raises(ValueError, match="broken target module"):
        reg.resolve("broken")


# --------------------------------------------------------------------------
# Contract 7: duplicate register replaces (last wins), no duplicate name.
# --------------------------------------------------------------------------


def test_register_replaces_an_existing_name() -> None:
    reg = Registry("widget", "flywheel.widgets")
    reg.register(PluginSpec(name="x", target="json:JSONDecoder"))
    reg.register(PluginSpec(name="x", target="json:JSONEncoder"))
    assert reg.resolve("x") is json.JSONEncoder
    assert reg.names() == ("x",)
    assert len(reg.specs()) == 1


def test_register_replacement_does_not_reorder_other_names() -> None:
    # Replacing an early name keeps its original position (dict semantics).
    reg = Registry("widget", "flywheel.widgets")
    reg.register(PluginSpec(name="a", target="json:JSONDecoder"))
    reg.register(PluginSpec(name="b", target="json:JSONEncoder"))
    reg.register(PluginSpec(name="a", target="json:JSONEncoder"))
    assert reg.names() == ("a", "b")
    assert reg.resolve("a") is json.JSONEncoder


# --------------------------------------------------------------------------
# Contract 8: entry-point discovery registers third-party backends, with
# built-ins winning a name collision; resolve works on built-ins alone too.
# --------------------------------------------------------------------------


def test_resolve_succeeds_without_any_entry_point_discovery() -> None:
    # No package advertises the test group, so discovery finds nothing and
    # built-in registration alone suffices.
    reg = _registry()
    assert reg.resolve("default") is json.JSONDecoder
    assert reg.names() == ("default", "optional")


@pytest.fixture
def _install_entry_point(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> Iterator[Callable[[str, str, str, str], None]]:
    """Materialize a real ``*.dist-info`` on ``sys.path`` advertising an entry
    point, so the genuine ``importlib.metadata.entry_points`` discovers it.

    This is a held-out oracle: it proves discovery through the real packaging
    machinery (not a monkeypatched ``entry_points``), exactly the path a
    pip/uv-installed third-party plugin would take.
    """
    site = tmp_path / "site"
    site.mkdir()
    monkeypatch.syspath_prepend(str(site))

    def _install(dist_name: str, group: str, name: str, value: str) -> None:
        dist_info = site / f"{dist_name}-0.0.0.dist-info"
        dist_info.mkdir()
        (dist_info / "METADATA").write_text(
            f"Metadata-Version: 2.1\nName: {dist_name}\nVersion: 0.0.0\n"
        )
        (dist_info / "entry_points.txt").write_text(
            f"[{group}]\n{name} = {value}\n"
        )
        importlib.invalidate_caches()

    yield _install


def test_discovery_registers_a_real_third_party_plugin(
    _install_entry_point: Callable[[str, str, str, str], None],
) -> None:
    # An installed dist advertising flywheel.widgets/ext resolves through the
    # identical "module:attr" path as a built-in.
    _install_entry_point("ext_widget", "flywheel.widgets", "ext", "json:JSONEncoder")
    reg = _registry()
    assert reg.resolve("ext") is json.JSONEncoder
    # It appears in diagnostics (after the built-ins).
    assert "ext" in reg.names()
    assert reg.names()[:2] == ("default", "optional")
    spec = next(s for s in reg.specs() if s.name == "ext")
    assert spec.target == "json:JSONEncoder"


def test_names_and_specs_trigger_discovery_without_a_resolve(
    _install_entry_point: Callable[[str, str, str, str], None],
) -> None:
    # A "list backends" caller that never resolves still sees discovered names.
    _install_entry_point("ext_widget", "flywheel.widgets", "ext", "json:JSONEncoder")
    reg = _registry()
    assert "ext" in reg.names()
    assert any(s.name == "ext" for s in reg.specs())


def test_builtin_wins_on_name_collision_with_a_discovered_plugin(
    _install_entry_point: Callable[[str, str, str, str], None],
) -> None:
    # A plugin advertising a built-in's name must NOT shadow the shipped
    # backend: resolution stays deterministic on the built-in target.
    _install_entry_point(
        "rogue_widget", "flywheel.widgets", "default", "json:JSONEncoder"
    )
    reg = _registry()  # built-in default -> json:JSONDecoder
    assert reg.resolve("default") is json.JSONDecoder
    assert reg.names().count("default") == 1


def test_discovery_scans_at_most_once(monkeypatch: pytest.MonkeyPatch) -> None:
    # The scan is cached: resolve/names/specs do not re-walk entry points.
    calls: list[str] = []

    def _counting_entry_points(*, group: str) -> list[importlib.metadata.EntryPoint]:
        calls.append(group)
        return []

    monkeypatch.setattr(
        importlib.metadata, "entry_points", _counting_entry_points
    )
    reg = _registry()
    reg.resolve("default")
    reg.resolve("default")
    reg.names()
    reg.specs()
    assert calls.count("flywheel.widgets") == 1


def test_discovery_with_no_entry_points_leaves_builtins_unchanged(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        importlib.metadata,
        "entry_points",
        lambda *, group: [],
    )
    reg = _registry()
    assert reg.names() == ("default", "optional")
