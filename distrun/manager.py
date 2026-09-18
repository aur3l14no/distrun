"""Shared orchestration for detached CLI operations and scoped SDK runs."""

from __future__ import annotations

import base64
import codecs
import time
from collections.abc import Callable, Iterator
from concurrent.futures import ThreadPoolExecutor
from functools import partial
from threading import Event, Lock

from .model import CommandProbe, DistrunError, Instance, Outcome, Project, Report, Service, Status
from .transport import Tmux


class Manager:
    def __init__(self, project: Project, *, backend: Tmux | None = None):
        self.project = project
        self.backend = backend or Tmux()
        self._hosts = {host.name: host for host in project.hosts}

    def _inventory(self, *, all_projects: bool = False, hosts: tuple[str, ...] | None = None):
        def observe(host):
            try:
                return (
                    host.name,
                    self.backend.list(host, None if all_projects else self.project.name),
                    None,
                )
            except DistrunError as error:
                return host.name, (), str(error)

        selected = (
            tuple(self._hosts.values()) if hosts is None else tuple(self._hosts[h] for h in hosts)
        )
        with ThreadPoolExecutor(max_workers=max(1, len(selected))) as pool:
            return tuple(pool.map(observe, selected))

    def status(self, *, all_projects: bool = False) -> tuple[Status, ...]:
        rows = []
        for host, instances, error in self._inventory(all_projects=all_projects):
            configured = {s.name: s for s in self.project.services if s.host == host}
            seen = set()
            for instance in instances:
                relation = (
                    "configured"
                    if instance.project == self.project.name and instance.service in configured
                    else "orphan"
                )
                rows.append(
                    Status(
                        host,
                        instance.project,
                        instance.service,
                        instance.state,
                        relation,
                        instance.token,
                        instance.exit_code,
                    )
                )
                if instance.project == self.project.name:
                    seen.add(instance.service)
            for key in configured.keys() - seen:
                rows.append(
                    Status(
                        host,
                        self.project.name,
                        key,
                        "unavailable" if error else "missing",
                        "configured",
                        error=error,
                    )
                )
            if error and not configured:
                rows.append(
                    Status(host, self.project.name, "*", "unavailable", "unknown", error=error)
                )
        return tuple(sorted(rows, key=lambda row: (row.host, row.project, row.service)))

    def _ready(
        self,
        service: Service,
        instance: Instance,
        *,
        poll: Callable[[Instance], bool] | None = None,
        on_error: Callable[[Exception], None] | None = None,
    ) -> None:
        deadline = time.monotonic() + service.start_timeout
        host = self._hosts[service.host]
        while True:
            # A scoped run supplies its whole-run observation. The readiness
            # loop then reuses that result instead of querying this host again.
            if poll is not None:
                if not poll(instance):
                    return
            else:
                current = next(
                    (
                        item
                        for item in self.backend.list(host, self.project.name)
                        if item.token == instance.token
                    ),
                    None,
                )
                if current is None:
                    raise DistrunError(f"{service.name}: disappeared before becoming ready")
                if current.state != "running":
                    raise self._startup_exit(current)
            try:
                probe = service.ready
                ready = (
                    True
                    if probe is None
                    else self.backend.probe(host, service, probe)
                    if isinstance(probe, CommandProbe)
                    else probe()
                )
                if ready:
                    return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise DistrunError(f"{service.name}: readiness timed out")
            except Exception as error:
                if on_error is None:
                    raise
                on_error(error)
                return
            time.sleep(min(0.05, remaining))

    def _startup_exit(self, instance: Instance) -> DistrunError:
        try:
            log = self.backend.logs(self._hosts[instance.host], instance, tail=40)
            details = base64.b64decode(log["data"]).decode("utf-8", errors="replace")
        except DistrunError as error:
            # Diagnostics cannot replace the confirmed exit or bypass policy.
            details = f"Could not read startup logs: {error}"
        return DistrunError(
            f"{instance.service}: exited before becoming ready ({instance.exit_code})\n{details}"
        )

    def _stop(self, instance: Instance) -> None:
        grace = next(
            (
                s.stop_timeout
                for s in self.project.services
                if s.name == instance.service and s.host == instance.host
            ),
            3.0,
        )
        self.backend.stop(self._hosts[instance.host], instance, grace)

    def up(self, *selected: str) -> Report:
        """Start dependencies first; preserve unrelated successes on failure.

        Readiness failure removes only the instance just created by this operation.
        Existing sessions survive this call and the caller's exit.
        """
        ordered = self.project.ordered(selected)
        outcomes = []
        failed: set[str] = set()
        for service in ordered:
            if failed.intersection(service.depends_on):
                failed.add(service.name)
                outcomes.append(Outcome(service.host, service.name, "failed", "dependency failed"))
                continue
            created = None
            try:
                instance, action = self.backend.start(
                    self._hosts[service.host],
                    self.project.name,
                    service,
                    restart=self.project.on_existing == "restart",
                )
                if action != "skipped":
                    created = instance
                # Finite commands are valid detached services without a readiness probe.
                if service.ready is not None or any(service.name in s.depends_on for s in ordered):
                    self._ready(service, instance)
                outcomes.append(Outcome(service.host, service.name, action))
            except Exception as error:
                failed.add(service.name)
                message = str(error)
                if created:
                    try:
                        self._stop(created)
                    except DistrunError as cleanup:
                        message += f"; cleanup failed: {cleanup}"
                outcomes.append(Outcome(service.host, service.name, "failed", message))
        return Report(tuple(outcomes))

    def _resolve(self, selectors: tuple[str, ...]) -> tuple[Instance, ...]:
        inventory = self._inventory()
        errors = [error for _, _, error in inventory if error]
        if errors:
            raise DistrunError("; ".join(errors))
        instances = [item for _, items, _ in inventory for item in items]
        chosen = {}
        for selector in selectors:
            matches = [
                item
                for item in instances
                if selector in (item.service, f"{item.host}/{item.service}")
            ]
            if len(matches) != 1:
                raise DistrunError(f"selector {selector!r} matched {len(matches)} instances")
            chosen[matches[0].token] = matches[0]
        return tuple(chosen.values())

    def stop(self, *selectors: str) -> Report:
        """Resolve every selector before any mutation; accepts configured or orphan services."""
        if not selectors:
            raise DistrunError("stop requires a service selector")
        outcomes = []
        for instance in reversed(self._resolve(selectors)):
            try:
                self._stop(instance)
                outcomes.append(Outcome(instance.host, instance.service, "stopped"))
            except DistrunError as error:
                outcomes.append(Outcome(instance.host, instance.service, "failed", str(error)))
        return Report(tuple(outcomes))

    def down(self) -> Report:
        """Stop this project's observed instances, including orphans, on the exact host scope."""
        order = {s.name: index for index, s in enumerate(self.project.ordered())}
        outcomes = []
        # Inventory failures don't suppress cleanup on reachable hosts.
        inventory = self._inventory()
        instances = []
        for host, items, error in inventory:
            if error:
                outcomes.append(Outcome(host, None, "failed", error))
            instances.extend(items)
        for instance in sorted(
            instances, key=lambda item: order.get(item.service, -1), reverse=True
        ):
            try:
                self._stop(instance)
                outcomes.append(Outcome(instance.host, instance.service, "stopped"))
            except DistrunError as error:
                outcomes.append(Outcome(instance.host, instance.service, "failed", str(error)))
        return Report(tuple(outcomes))

    def recreate(self, *selected: str) -> Report:
        if not selected:
            stopped = self.down()
            if not stopped.ok:
                return stopped
            return Report(stopped.outcomes + self.up().outcomes)
        self.project.ordered(selected)  # Validate the entire selection before mutation.
        existing = {
            item.service for _, items, error in self._inventory() if not error for item in items
        }
        targets = tuple(key for key in selected if key in existing)
        stopped = self.stop(*targets) if targets else Report(())
        if not stopped.ok:
            return stopped
        return Report(stopped.outcomes + self.up(*selected).outcomes)

    def logs(self, selector: str, *, tail: int = 80, follow: bool = False) -> Iterator[str]:
        (instance,) = self._resolve((selector,))
        offset = None
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        while True:
            row = self.backend.logs(self._hosts[instance.host], instance, tail=tail, offset=offset)
            text = decoder.decode(base64.b64decode(row["data"]))
            if text:
                yield text
            previous = offset
            offset = row["offset"]
            if not follow or (row["state"] == "exited" and previous == offset):
                final = decoder.decode(b"", final=True)
                if final:
                    yield final
                return
            time.sleep(0.05)

    def scope(
        self,
        *selected: str,
        check: Callable[[], None] | None = None,
        on_failure: Callable[[str, Exception], None] | None = None,
    ) -> Scope:
        """Own one run; application callbacks may cancel or handle a process failure.

        check runs during startup and wait, outside the process being supervised.
        on_failure may raise to end the run, or return to acknowledge that failure.
        Neither callback takes ownership of processes or cleanup.
        """
        return Scope(self, selected, check=check, on_failure=on_failure)


class Scope:
    """One task-owned process set, including startup, observation and token cleanup.

    Context entry starts the selected dependency graph. wait() observes each host
    once per poll, including while a later process is becoming ready. There is no
    background thread: callbacks execute on the caller's acquiring/waiting thread.
    """

    def __init__(
        self,
        manager: Manager,
        selected: tuple[str, ...],
        *,
        check: Callable[[], None] | None = None,
        on_failure: Callable[[str, Exception], None] | None = None,
    ):
        self.manager = manager
        self._check = check
        self._on_failure = on_failure
        self._services = manager.project.ordered(selected)
        self._owned: list[Instance] = []
        # An acknowledged optional failure is delivered once, but its token is
        # retained for cleanup. This is notification state, not process state.
        self._acknowledged: set[str] = set()
        self._entered = False
        self._closing = Event()
        self._cleanup_lock = Lock()

    @property
    def instances(self) -> tuple[Instance, ...]:
        return tuple(self._owned)

    def _failure(self, service: str, error: Exception, token: str | None = None) -> None:
        if token in self._acknowledged:
            return
        if self._on_failure is None:
            raise error
        self._on_failure(service, error)
        if token is not None:
            self._acknowledged.add(token)

    def _poll(self, *, startup: bool = False) -> Instance | None:
        if self._check is not None:
            self._check()
        monitored = tuple(item for item in self._owned if item.token not in self._acknowledged)
        inventory = self.manager._inventory(hosts=tuple(dict.fromkeys(i.host for i in monitored)))
        for host, instances, unavailable in inventory:
            current = {item.token: item for item in instances}
            if self._closing.is_set():
                return None
            for owned in monitored:
                if owned.host != host or owned not in self._owned:
                    continue
                observed = current.get(owned.token)
                if unavailable:
                    error = DistrunError(f"{owned.service}: host unavailable: {unavailable}")
                elif observed is None:
                    error = DistrunError(
                        f"{owned.service}: owned runtime disappeared or was replaced"
                    )
                elif observed.state != "running":
                    if not startup and self._on_failure is None:
                        return observed
                    error = (
                        self.manager._startup_exit(observed)
                        if startup
                        else DistrunError(f"{owned.service}: exited ({observed.exit_code})")
                    )
                else:
                    continue
                self._failure(owned.service, error, owned.token)
        return None

    def __enter__(self) -> Scope:
        if self._entered or self._closing.is_set():
            raise DistrunError("a scope cannot be reused after entry or close")
        self._entered = True
        try:
            for service in self._services:
                self._poll(startup=True)
                ready_names = {
                    item.service for item in self._owned if item.token not in self._acknowledged
                }
                if not set(service.depends_on) <= ready_names:
                    self._failure(service.name, DistrunError(f"{service.name}: dependency failed"))
                    continue
                try:
                    instance, _ = self.manager.backend.start(
                        self.manager._hosts[service.host],
                        self.manager.project.name,
                        service,
                        exclusive=True,
                    )
                except Exception as error:
                    self._failure(service.name, error)
                    continue
                self._owned.append(instance)
                self.manager._ready(
                    service,
                    instance,
                    poll=self._startup_poll,
                    on_error=partial(self._failure, service.name, token=instance.token),
                )
            self._poll(startup=True)
        except BaseException as error:
            try:
                self.close()
            except Exception as cleanup:
                raise BaseExceptionGroup("startup and rollback failed", [error, cleanup]) from None
            raise
        return self

    def _startup_poll(self, instance: Instance) -> bool:
        self._poll(startup=True)
        return not self._closing.is_set() and instance.token not in self._acknowledged

    def wait(self, *, timeout: float | None = None) -> Instance | None:
        """Observe the run until failure, timeout or close.

        Without on_failure, return the first exited process; missing ownership or
        transport loss raises. With a policy, each failure is delivered once and
        waiting continues when the callback returns. Every acquired token
        remains owned until stopped or closed.
        """
        if not self._entered:
            raise DistrunError("scope has not been entered")
        deadline = None if timeout is None else time.monotonic() + timeout
        while not self._closing.is_set():
            ended = self._poll()
            if ended is not None:
                return ended
            if deadline is not None and time.monotonic() >= deadline:
                return None
            self._closing.wait(0.05)
        return None

    def stop(self, *services: str) -> None:
        """Stop named processes owned by this scope, preserving unrelated tokens.

        A process that was never acquired or already stopped needs no work. This
        also allows an application to stop its consumer before general cleanup.
        """
        with self._cleanup_lock:
            errors = []
            for instance in tuple(reversed(self._owned)):
                if instance.service not in services:
                    continue
                try:
                    self.manager._stop(instance)
                    self._owned.remove(instance)
                except Exception as error:
                    errors.append(error)
            if errors:
                raise ExceptionGroup("scope cleanup failed; unresolved instances retained", errors)

    def close(self) -> None:
        self._closing.set()
        self.stop(*(item.service for item in self._owned))

    def __exit__(self, exc_type, exc, tb) -> None:
        try:
            self.close()
        except Exception as cleanup:
            if exc is not None:
                raise BaseExceptionGroup("run and cleanup failed", [exc, cleanup]) from None
            raise
