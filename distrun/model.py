"""Public values; no process execution or application-specific policy."""

from __future__ import annotations

import math
import re
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from types import MappingProxyType


class DistrunError(Exception):
    """A configuration, transport, or lifecycle operation failed."""


def name(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", value):
        raise DistrunError(f"invalid name: {value!r}; use ASCII letters, digits, '_' or '-'")
    return value


def duration(value: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (float, int)):
        raise DistrunError(f"invalid duration: {value!r}")
    if not math.isfinite(value) or value <= 0:
        raise DistrunError("durations must be finite and positive")
    return float(value)


@dataclass(frozen=True)
class Host:
    name: str = "local"
    ssh: str | None = None

    def __post_init__(self) -> None:
        name(self.name)
        if self.ssh is not None and (not self.ssh or self.ssh.startswith("-")):
            raise DistrunError("ssh must be a nonempty target, not an option")
        if self.name == "local" and self.ssh is not None:
            raise DistrunError("the local host cannot use SSH")
        if self.name != "local" and self.ssh is None:
            raise DistrunError(f"host {self.name!r} requires an SSH target")


@dataclass(frozen=True)
class CommandProbe:
    """Exit code zero means ready; executes on the service's host with its cwd/env."""

    command: str
    timeout: float = 1.0

    def __post_init__(self) -> None:
        if not isinstance(self.command, str) or not self.command.strip():
            raise DistrunError("probe command cannot be empty")
        duration(self.timeout)


Ready = CommandProbe | Callable[[], bool]


@dataclass(frozen=True)
class Service:
    name: str
    command: str
    host: str = "local"
    cwd: str | None = None
    env: Mapping[str, str] = field(default_factory=dict)
    depends_on: tuple[str, ...] = ()
    ready: Ready | None = None
    start_timeout: float = 60.0
    stop_timeout: float = 3.0

    def __post_init__(self) -> None:
        name(self.name)
        name(self.host)
        if not isinstance(self.command, str) or not self.command.strip():
            raise DistrunError("service command cannot be empty")
        if self.cwd is not None and not isinstance(self.cwd, str):
            raise DistrunError("cwd must be a string")
        duration(self.start_timeout)
        duration(self.stop_timeout)
        for key, value in self.env.items():
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) or not isinstance(value, str):
                raise DistrunError(f"invalid environment entry: {key!r}")
        object.__setattr__(self, "env", MappingProxyType(dict(self.env)))
        object.__setattr__(self, "depends_on", tuple(self.depends_on))
        if (
            self.ready is not None
            and not isinstance(self.ready, CommandProbe)
            and not callable(self.ready)
        ):
            raise DistrunError("ready must be a CommandProbe or a callable")


@dataclass(frozen=True)
class Project:
    name: str
    services: tuple[Service, ...] = ()
    hosts: tuple[Host, ...] = (Host(),)
    on_existing: str = "skip"

    def __post_init__(self) -> None:
        name(self.name)
        object.__setattr__(self, "services", tuple(self.services))
        object.__setattr__(self, "hosts", tuple(self.hosts))
        if self.on_existing not in ("skip", "restart"):
            raise DistrunError("on_existing must be skip or restart")
        if len({h.name for h in self.hosts}) != len(self.hosts):
            raise DistrunError("duplicate host name")
        if len({h.ssh for h in self.hosts}) != len(self.hosts):
            raise DistrunError("each SSH target must have one host alias")
        if len({s.name for s in self.services}) != len(self.services):
            raise DistrunError("duplicate service name")
        hosts = {h.name for h in self.hosts}
        for service in self.services:
            if service.host not in hosts:
                raise DistrunError(f"unknown host {service.host!r}")
        self.ordered()

    def ordered(self, selected: tuple[str, ...] = ()) -> tuple[Service, ...]:
        """Topological order, including the dependencies of a selection."""
        services = {s.name: s for s in self.services}
        result: dict[str, Service] = {}
        visiting: set[str] = set()

        def visit(key: str) -> None:
            if key in visiting:
                raise DistrunError(f"dependency cycle at {key!r}")
            if key in result:
                return
            if key not in services:
                raise DistrunError(f"unknown service {key!r}")
            visiting.add(key)
            for dependency in services[key].depends_on:
                visit(dependency)
            visiting.remove(key)
            result[key] = services[key]

        for key in selected or tuple(services):
            visit(key)
        return tuple(result.values())


@dataclass(frozen=True)
class Instance:
    host: str
    project: str
    service: str
    token: str
    state: str
    exit_code: int | None = None


@dataclass(frozen=True)
class Status:
    host: str
    project: str
    service: str
    state: str
    relation: str
    token: str | None = None
    exit_code: int | None = None
    error: str | None = None


@dataclass(frozen=True)
class Outcome:
    host: str
    service: str | None
    action: str
    error: str | None = None


@dataclass(frozen=True)
class Report:
    outcomes: tuple[Outcome, ...]

    @property
    def ok(self) -> bool:
        return all(o.error is None for o in self.outcomes)

    def raise_for_errors(self) -> None:
        if not self.ok:
            raise DistrunError("; ".join(o.error for o in self.outcomes if o.error))
