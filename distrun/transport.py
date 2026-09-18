"""Bounded execution of the same host operations locally or through OpenSSH."""

from __future__ import annotations

import json
import os
import shlex
import signal
import subprocess
import sys
import uuid
from dataclasses import asdict
from pathlib import Path
from typing import Any

from .model import CommandProbe, DistrunError, Host, Instance, Service, duration, name

_HOST_SOURCE = Path(__file__).with_name("_host.py").read_text()


def service_payload(service: Service) -> dict[str, Any]:
    return dict(
        name=service.name,
        command=service.command,
        cwd=service.cwd,
        env=dict(service.env),
        stop_timeout=service.stop_timeout,
    )


class Tmux:
    """A tmux namespace shared by CLI and SDK. No persistent Python agent is needed.

    Hosts require tmux and Python >= 3.11. SSH uses the operator's OpenSSH config.
    The timeout bounds each host operation, not a service's lifetime.
    """

    def __init__(
        self,
        *,
        socket: str = "distrun",
        timeout: float = 10.0,
        ssh_config: str | Path | None = None,
    ):
        self.socket = name(socket)
        self.timeout = duration(timeout)
        self.ssh_config = str(ssh_config) if ssh_config is not None else None

    def request(
        self, host: Host, operation: str, *, timeout: float | None = None, **payload: Any
    ) -> Any:
        command = [sys.executable, "-m", "distrun._host"]
        if host.ssh:
            command = [
                "ssh",
                *(["-F", self.ssh_config] if self.ssh_config else []),
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "--",
                host.ssh,
                "python3 -c " + shlex.quote(_HOST_SOURCE),
            ]
        request = json.dumps(dict(socket=self.socket, operation=operation, **payload))
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
        except OSError as error:
            raise DistrunError(f"{host.name}: cannot start host transport: {error}") from error
        try:
            stdout, stderr = process.communicate(request, timeout=timeout or self.timeout)
        except BaseException as error:
            # Let a host transaction finish before propagating caller cancellation.
            # Its immutable token lets start() roll back a committed result.
            try:
                if not isinstance(error, subprocess.TimeoutExpired):
                    process.communicate(timeout=timeout or self.timeout)
                else:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.communicate(timeout=2)
            except BaseException:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.communicate()
            if isinstance(error, subprocess.TimeoutExpired):
                raise DistrunError(
                    f"{host.name}: host operation {operation!r} timed out; remote state unknown"
                ) from error
            raise
        try:
            response = json.loads(stdout)
        except ValueError as error:
            raise DistrunError(
                f"{host.name}: host transport failed: {stderr.strip() or 'invalid response'}"
            ) from error
        if process.returncode or "error" in response:
            raise DistrunError(f"{host.name}: {response.get('error', stderr.strip())}")
        return response["result"]

    @staticmethod
    def instance(host: Host, row: dict[str, Any]) -> Instance:
        return Instance(
            host.name, row["project"], row["service"], row["token"], row["state"], row["exit_code"]
        )

    def list(self, host: Host, project: str | None = None) -> tuple[Instance, ...]:
        return tuple(
            self.instance(host, row) for row in self.request(host, "list", project=project)
        )

    def start(
        self,
        host: Host,
        project: str,
        service: Service,
        *,
        restart: bool = False,
        exclusive: bool = False,
    ) -> tuple[Instance, str]:
        token = uuid.uuid4().hex
        try:
            row = self.request(
                host,
                "start",
                project=project,
                service=service_payload(service),
                restart=restart,
                exclusive=exclusive,
                token=token,
                timeout=self.timeout + service.stop_timeout,
            )
            return self.instance(host, row["instance"]), row["action"]
        except BaseException as error:
            try:
                for instance in self.list(host, project):
                    if instance.token == token:
                        self.stop(host, instance, service.stop_timeout)
            except Exception as cleanup:
                raise BaseExceptionGroup(
                    "start failed and ownership could not be settled", [error, cleanup]
                ) from None
            raise

    def stop(self, host: Host, instance: Instance, grace: float) -> None:
        self.request(
            host,
            "stop",
            instance=asdict(instance),
            timeout=self.timeout + grace,
            **{"timeout_seconds": grace},
        )

    def probe(self, host: Host, service: Service, probe: CommandProbe) -> bool:
        return bool(
            self.request(
                host,
                "probe",
                service=service_payload(service),
                command=probe.command,
                timeout=self.timeout + probe.timeout,
                timeout_seconds=probe.timeout,
            )
        )

    def logs(
        self, host: Host, instance: Instance, *, tail: int = 80, offset: int | None = None
    ) -> dict[str, Any]:
        if tail < 0:
            raise DistrunError("tail must be nonnegative")
        return self.request(host, "logs", instance=asdict(instance), tail=tail, offset=offset)
