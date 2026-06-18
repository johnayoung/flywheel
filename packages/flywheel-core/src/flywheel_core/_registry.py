"""The one mechanism behind every "pick a named backend" seam in flywheel.

Flywheel has several places where an operator names a backend and the code
must turn that name into a concrete implementation: the store (``sqlite`` vs
``postgres``), the work source (``directory`` vs ``github``), the submit
strategy (``merge`` vs ``pr``). Each of those seams used to hand-roll the same
four steps — validate the name against a hardcoded tuple, lazily import the
implementation, raise a branded "install the extra" error if an optional
dependency is missing, then construct. This module is that shape, written once.

What lives here is *selection glue only*. The contract a backend must satisfy
stays a ``@runtime_checkable`` Protocol owned by its own seam
(``LifecycleStore``, ``WorkSource``, ``SubmitStrategy``); the registry never
defines or rewrites those. A :class:`Registry` maps a name to a
:class:`PluginSpec` whose ``target`` is a ``"module:attr"`` string, and
:meth:`Registry.resolve` imports that target on demand.

Two kinds of failure are branded in exactly one place:

* an unknown name raises :class:`UnknownPluginError`, listing the names that
  *are* registered (replacing every seam's bespoke "must be one of (...)");
* a backend whose optional extra is not installed raises
  :class:`MissingExtraError`, whose message names the extra to install
  (replacing the install-hint strings previously copied across ``_sdk``,
  ``store_postgres`` and the store factory).

The ``"module:attr"`` target form is deliberate: it is exactly the encoding a
Python packaging entry point uses for ``ep.value``. So a built-in spec and one
*discovered* from an installed third-party package resolve through the
identical path: :meth:`Registry._maybe_discover_entry_points` reads
``importlib.metadata.entry_points(group=...)`` on first access and registers
any advertised backend not already built in — without touching a single call
site, because every caller already routes through :meth:`resolve`.

This module uses ``importlib`` and therefore is never imported by the pure
``flywheel_core.task`` / ``flywheel_core.lifecycle`` modules (their purity
tests would fail). It sits beside ``flywheel_core._sdk`` as core mechanism
that the data shapes themselves do not touch.
"""

from __future__ import annotations

import importlib
import importlib.metadata
from dataclasses import dataclass
from types import ModuleType
from typing import Any


def install_hint(extra: str) -> str:
    """The one canonical "how to install this optional extra" sentence.

    Every branded missing-dependency message in flywheel ends with this, so
    the install command an operator is told to run is defined in a single
    place. The product ``flywheel`` dist re-exports each core extra, so the
    hint names ``flywheel[...]`` rather than ``flywheel-core[...]``.
    """
    return f"install it with: uv add 'flywheel[{extra}]'"


class MissingExtraError(ModuleNotFoundError):
    """An optional dependency for a selected backend is not installed.

    Subclasses :class:`ModuleNotFoundError` so existing ``except
    ImportError`` / ``except ModuleNotFoundError`` paths keep catching it.
    The message names the extra to install via :func:`install_hint`.
    """

    def __init__(self, extra: str, *, detail: str | None = None) -> None:
        self.extra = extra
        prefix = detail or f"the optional '{extra}' extra is not installed"
        super().__init__(f"{prefix}; {install_hint(extra)}")


class UnknownPluginError(LookupError):
    """A name was requested that no backend in this family is registered for.

    The message lists the registered names so the caller (and the operator
    reading a config error) sees the valid choices immediately.
    """

    def __init__(self, family: str, name: str, known: tuple[str, ...]) -> None:
        self.family = family
        self.name = name
        self.known = known
        choices = ", ".join(repr(k) for k in known) if known else "(none)"
        super().__init__(
            f"unknown {family} {name!r}; registered: {choices}"
        )


def import_extra(module: str, extra: str) -> ModuleType:
    """Import ``module``, branding a missing optional dependency.

    The single lazy-import boundary for backends gated behind an extra: a
    builder that needs an optional package imports its implementation module
    through here, so the ``ImportError`` -> :class:`MissingExtraError`
    translation (and the install hint) lives in one place instead of being
    re-derived in every backend. ``ImportError`` is caught broadly rather
    than only ``ModuleNotFoundError`` because a gated module typically guards
    its own optional imports and re-raises a crafted ``ImportError`` naming
    the extra (e.g. ``flywheel_core.store_postgres``).
    """
    try:
        return importlib.import_module(module)
    except ImportError as exc:
        raise MissingExtraError(extra) from exc


@dataclass(frozen=True, kw_only=True)
class PluginSpec:
    """One named backend in a family.

    ``target`` is a ``"module:attr"`` string naming the callable the registry
    resolves to — a store/source/strategy class, or a builder function with
    the family's construction signature. The string form matches a packaging
    entry point's ``ep.value`` so a built-in spec and a discovered one resolve
    through the identical path. ``extra`` names the optional dependency the
    target needs (``None`` when the backend is always importable); ``summary``
    is a short human label for ``list``/diagnostics.
    """

    name: str
    target: str
    extra: str | None = None
    summary: str = ""


class Registry:
    """A name -> :class:`PluginSpec` table for one backend family.

    ``family`` is the singular noun used in error messages ("store",
    "work source", "submit strategy"); ``group`` is the reserved Python
    entry-point group string (e.g. ``"flywheel.stores"``) that
    :meth:`_maybe_discover_entry_points` will read once discovery is enabled.
    """

    def __init__(self, family: str, group: str) -> None:
        self._family = family
        self._group = group
        self._specs: dict[str, PluginSpec] = {}
        self._discovered = False

    def register(self, spec: PluginSpec) -> None:
        """Add a built-in backend. Re-registering a name replaces it."""
        self._specs[spec.name] = spec

    def names(self) -> tuple[str, ...]:
        """The backend names, built-ins first then discovered plugins."""
        self._maybe_discover_entry_points()
        return tuple(self._specs)

    def specs(self) -> tuple[PluginSpec, ...]:
        """The registered specs (built-ins then discovered) for diagnostics."""
        self._maybe_discover_entry_points()
        return tuple(self._specs.values())

    def resolve(self, name: str) -> Any:
        """Return the callable ``name`` selects, importing it on demand.

        Raises :class:`UnknownPluginError` if ``name`` is not registered, and
        :class:`MissingExtraError` if the backend's optional extra is not
        installed. The returned object is the genuine class/function named by
        the spec's ``target``, so ``isinstance`` against the family's Protocol
        behaves exactly as a direct import would.
        """
        self._maybe_discover_entry_points()
        spec = self._specs.get(name)
        if spec is None:
            raise UnknownPluginError(self._family, name, self.names())
        module_name, _, attr = spec.target.partition(":")
        if spec.extra is not None:
            module = import_extra(module_name, spec.extra)
        else:
            module = importlib.import_module(module_name)
        return getattr(module, attr)

    def _maybe_discover_entry_points(self) -> None:
        """Register third-party backends advertised under this family's group.

        Scans ``importlib.metadata.entry_points(group=self._group)`` once per
        registry and adds any name not already built in — **built-ins win on a
        name collision**, so an installed package can *extend* the choices but
        never silently shadow a shipped backend, keeping resolution
        deterministic. Each entry point's ``value`` (``"module:attr"``) becomes
        the spec ``target``, resolving through the identical path as a built-in;
        a discovered spec declares no ``extra``, so a missing dependency in the
        plugin's own module surfaces as the raw ``ImportError`` it raises. The
        scan runs at most once — afterwards the cached flag short-circuits, so
        :meth:`resolve` stays cheap on the hot path. Because every caller routes
        through :meth:`resolve`, enabling discovery moved no call site.
        """
        if self._discovered:
            return
        self._discovered = True
        for entry_point in importlib.metadata.entry_points(group=self._group):
            if entry_point.name in self._specs:
                continue
            self._specs[entry_point.name] = PluginSpec(
                name=entry_point.name,
                target=entry_point.value,
                summary=f"third-party plugin ({entry_point.value})",
            )


__all__ = [
    "MissingExtraError",
    "PluginSpec",
    "Registry",
    "UnknownPluginError",
    "import_extra",
    "install_hint",
]
