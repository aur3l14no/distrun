"""YAML is an input adapter; SDK users construct the same public values directly."""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any

import yaml

from .model import CommandProbe, DistrunError, Host, Project, Service, duration


class UniqueLoader(yaml.SafeLoader):
    pass


def _mapping(loader, node):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node)
        if not isinstance(key, str):
            raise DistrunError("YAML mapping keys must be strings")
        if key in result:
            raise DistrunError(f"duplicate YAML key {key!r}")
        result[key] = loader.construct_object(value_node)
    return result


UniqueLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _mapping)
_PATTERN = re.compile(
    r"\$\$|\$\{([A-Za-z_][A-Za-z0-9_]*)(?:(:?[-+?])([^{}]*))?\}|\$([A-Za-z_][A-Za-z0-9_]*)"
)


def expand(text: str, env: dict[str, str]) -> str:
    """One expansion pass; escaped dollars and substituted values are never re-expanded."""
    if not isinstance(text, str):
        raise DistrunError(f"expected a string, got {text!r}")

    def substitute(match):
        if match.group() == "$$":
            return "$"
        key, operator, fallback, bare = match.groups()
        key = key or bare
        operator = operator or ""
        present = key in env and (not operator.startswith(":") or bool(env[key]))
        if operator.endswith("-"):
            return env[key] if present else fallback
        if operator.endswith("+"):
            return fallback if present else ""
        if operator.endswith("?") and not present:
            raise DistrunError(fallback or f"missing environment variable {key}")
        if not present:
            raise DistrunError(
                f"missing environment variable {key}; use $${key} for shell expansion"
            )
        return env[key]

    return _PATTERN.sub(substitute, text)


def _seconds(value) -> float:
    if isinstance(value, str):
        multiplier = 0.001 if value.endswith("ms") else 1
        value = float(value.removesuffix("ms").removesuffix("s")) * multiplier
    return duration(value)


def _paths(value):
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise DistrunError("include/env_file expects a path or list of paths")
    return value


def _document(path: Path, env: dict[str, str], stack: tuple[Path, ...] = ()):
    path = path.resolve()
    if path in stack:
        raise DistrunError(f"include cycle at {path}")
    try:
        data = yaml.load(path.read_text(), Loader=UniqueLoader)
    except (OSError, yaml.YAMLError) as error:
        raise DistrunError(f"cannot read {path}: {error}") from error
    if not isinstance(data, dict):
        raise DistrunError(f"{path}: configuration must be a mapping")
    hosts: dict[str, tuple[Any, Path]] = {}
    services: dict[str, tuple[Any, Path]] = {}
    settings = {}
    for field in ("include", "include?"):
        for reference in _paths(data.get(field)):
            included = path.parent / expand(reference, env)
            if field == "include?" and not included.exists():
                continue
            child_settings, child_hosts, child_services = _document(included, env, (*stack, path))
            settings.update(child_settings)
            for target, incoming in ((hosts, child_hosts), (services, child_services)):
                if target.keys() & incoming.keys():
                    raise DistrunError(
                        f"duplicate definition in {included}: {target.keys() & incoming.keys()}"
                    )
                target.update(incoming)
    unknown = data.keys() - {"include", "include?", "project", "on_existing", "hosts", "services"}
    if unknown:
        raise DistrunError(f"unknown configuration fields: {sorted(unknown)}")
    for field in ("project", "on_existing"):
        if field in data:
            settings[field] = data[field]
    for field, target in (("hosts", hosts), ("services", services)):
        raw = data.get(field, {})
        if not isinstance(raw, dict):
            raise DistrunError(f"{field} must be a mapping")
        if target.keys() & raw.keys():
            raise DistrunError(f"duplicate {field}: {target.keys() & raw.keys()}")
        target.update({key: (value, path.parent) for key, value in raw.items()})
    return settings, hosts, services


def _hosts(raw, env):
    result = []
    for key, (value, _) in raw.items():
        if not isinstance(value, dict) or value.keys() - {"ssh"}:
            raise DistrunError(f"invalid host {key!r}")
        ssh = expand(value["ssh"], env) if "ssh" in value else None
        result.append(Host(key, ssh))
    return tuple(result)


def load_hosts(path: str | Path) -> tuple[Host, ...]:
    _, hosts, _ = _document(Path(path), dict(os.environ))
    result = _hosts(hosts, dict(os.environ))
    if not result:
        raise DistrunError("host inventory cannot be empty")
    return result


def load(path: str | Path) -> Project:
    env = dict(os.environ)
    settings, raw_hosts, raw_services = _document(Path(path), env)
    hosts = list(_hosts(raw_hosts, env))
    services = []
    for key, (raw, directory) in raw_services.items():
        allowed = {
            "host",
            "cmd",
            "cwd",
            "env",
            "env_file",
            "depends_on",
            "ready",
            "start_timeout",
            "stop_timeout",
        }
        if not isinstance(raw, dict) or raw.keys() - allowed:
            raise DistrunError(f"invalid service fields for {key!r}")
        service_env = {}
        for reference in _paths(raw.get("env_file")):
            file = directory / expand(reference, env)
            for line in file.read_text().splitlines():
                if not line.strip() or line.lstrip().startswith("#"):
                    continue
                entry, separator, value = line.partition("=")
                if not separator:
                    raise DistrunError(f"invalid env file line in {file}")
                service_env[entry.strip()] = value.strip()
        inline = raw.get("env", {})
        if not isinstance(inline, dict):
            raise DistrunError("env must be a mapping")
        service_env.update(inline)
        service_env = {
            k: expand(v, env | {other: value for other, value in service_env.items() if other != k})
            for k, v in service_env.items()
        }
        properties = env | service_env
        host = expand(raw.get("host", "local"), properties)
        if host == "local" and not any(h.name == "local" for h in hosts):
            hosts.append(Host())
        dependencies = raw.get("depends_on", [])
        if not isinstance(dependencies, list) or not all(
            isinstance(item, str) for item in dependencies
        ):
            raise DistrunError("depends_on must be a list of service names")
        ready = raw.get("ready")
        if ready is not None:
            if (
                not isinstance(ready, dict)
                or ready.keys() - {"cmd", "timeout"}
                or "cmd" not in ready
            ):
                raise DistrunError("ready requires cmd and optional timeout")
            ready = CommandProbe(
                expand(ready["cmd"], properties), _seconds(ready.get("timeout", 1))
            )
        if "cmd" not in raw:
            raise DistrunError(f"service {key!r} requires cmd")
        services.append(
            Service(
                key,
                expand(raw["cmd"], properties),
                host,
                expand(raw["cwd"], properties) if "cwd" in raw else None,
                service_env,
                tuple(dependencies),
                ready,
                _seconds(expand(str(raw.get("start_timeout", 60)), properties)),
                _seconds(expand(str(raw.get("stop_timeout", 3)), properties)),
            )
        )
    if "project" not in settings:
        raise DistrunError("configuration requires project")
    return Project(
        expand(settings["project"], env),
        tuple(services),
        tuple(hosts),
        expand(settings.get("on_existing", "skip"), env),
    )
