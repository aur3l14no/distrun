"""Real process fixtures. Every process/session/file has a failure-safe owner."""

from __future__ import annotations

import getpass
import json
import shlex
import shutil
import socket
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest
import yaml

from distrun import Host, Manager, Project, Service, Tmux


def pytest_addoption(parser):
    parser.addoption("--ssh-target", help="Enable SSH E2E against this authorized host")
    parser.addoption("--ssh-config", help="OpenSSH config for --ssh-target")
    parser.addoption(
        "--ssh-local", action="store_true", help="Run SSH E2E through an isolated loopback sshd"
    )


def pytest_collection_modifyitems(config, items):
    if config.getoption("--ssh-target") or config.getoption("--ssh-local"):
        return
    selected = [item for item in items if "ssh" not in item.keywords]
    config.hook.pytest_deselected(items=[item for item in items if "ssh" in item.keywords])
    items[:] = selected


def eventually(check, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        result = check()
        if result:
            return result
        if time.monotonic() >= deadline:
            raise AssertionError("condition did not become true")
        time.sleep(0.03)


@pytest.fixture(scope="session")
def ssh_endpoint(request, tmp_path_factory):
    if not request.config.getoption("--ssh-local"):
        target = request.config.getoption("--ssh-target")
        assert target, "SSH tests require --ssh-target or --ssh-local"
        yield target, request.config.getoption("--ssh-config")
        return
    directory = tmp_path_factory.mktemp("sshd")
    daemon = shutil.which("sshd")
    assert daemon, "--ssh-local requires OpenSSH server"
    for key in ("host", "client"):
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(directory / key)], check=True
        )
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    server_config = directory / "sshd_config"
    server_config.write_text(
        f"Port {port}\nListenAddress 127.0.0.1\nHostKey {directory}/host\n"
        f"PidFile {directory}/pid\nAuthorizedKeysFile {directory}/client.pub\n"
        "StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        "UsePAM no\nPermitRootLogin prohibit-password\nLogLevel ERROR\n"
    )
    client_config = directory / "ssh_config"
    client_config.write_text(
        f"Host test-node\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n"
        f" IdentityFile {directory}/client\n IdentitiesOnly yes\n"
        " StrictHostKeyChecking no\n UserKnownHostsFile /dev/null\n LogLevel ERROR\n"
        " ControlMaster auto\n ControlPersist 10\n"
        f" ControlPath {directory}/mux\n"
    )
    with (directory / "server.log").open("w+") as log:
        process = subprocess.Popen(
            [daemon, "-D", "-e", "-f", str(server_config)], stdout=log, stderr=log
        )
        try:

            def connected():
                assert process.poll() is None, (directory / "server.log").read_text()
                return (
                    subprocess.run(
                        ["ssh", "-F", str(client_config), "test-node", "true"], capture_output=True
                    ).returncode
                    == 0
                )

            eventually(connected)
            yield "test-node", str(client_config)
        finally:
            subprocess.run(
                ["ssh", "-F", str(client_config), "-O", "exit", "test-node"], capture_output=True
            )
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


@dataclass
class Rig:
    host: Host
    backend: Tmux
    directory: str
    local: Path
    project: str

    def shell(self, command):
        argv = ["sh", "-c", command]
        if self.host.ssh:
            argv = [
                "ssh",
                *(["-F", self.backend.ssh_config] if self.backend.ssh_config else []),
                "--",
                self.host.ssh,
                command,
            ]
        return subprocess.run(argv, check=True, capture_output=True, text=True, timeout=15).stdout

    def service(self, name, command="printf 'ready\\n'; exec sleep 300", **kwargs):
        return Service(
            name, command, host=self.host.name, cwd=self.directory, stop_timeout=0.3, **kwargs
        )

    def manager(self, *services, name=None, restart=False):
        return Manager(
            Project(name or self.project, services, (self.host,), "restart" if restart else "skip"),
            backend=self.backend,
        )

    def config(self, *services, name=None, restart=False):
        path = self.local / "distrun.yml"
        path.write_text(
            yaml.safe_dump(
                dict(
                    project=name or self.project,
                    on_existing="restart" if restart else "skip",
                    hosts={self.host.name: {"ssh": self.host.ssh} if self.host.ssh else {}},
                    services={
                        s.name: dict(
                            cmd=s.command.replace("$", "$$"),
                            host=s.host,
                            cwd=s.cwd,
                            env=dict(s.env),
                            stop_timeout=s.stop_timeout,
                        )
                        for s in services
                    },
                )
            )
        )
        return path

    def argv(self, *args):
        return [
            sys.executable,
            "-m",
            "distrun",
            "--socket",
            self.backend.socket,
            *(["--ssh-config", self.backend.ssh_config] if self.backend.ssh_config else []),
            *map(str, args),
        ]

    def cli(self, *args, success=True, cwd=None):
        result = subprocess.run(
            self.argv(*args), cwd=cwd, capture_output=True, text=True, timeout=30
        )
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        return result.stdout

    def rows(self, path):
        return json.loads(self.cli("-f", path, "--json", "status"))


@pytest.fixture
def rig(request, tmp_path):
    location = getattr(request, "param", "local")
    target, config = request.getfixturevalue("ssh_endpoint") if location == "ssh" else (None, None)
    host = Host("node", target) if target else Host()
    backend = Tmux(socket="test_" + uuid.uuid4().hex, ssh_config=config)
    result = Rig(host, backend, "", tmp_path, "demo")
    try:
        result.directory = result.shell("mktemp -d /tmp/distrun-e2e-XXXXXXXX").strip()
        yield result
    finally:
        # Dedicated tmux server makes cleanup independent of the assertion or config state.
        result.shell(f"tmux -L {shlex.quote(backend.socket)} kill-server 2>/dev/null || true")
        cleanup = (
            "import pathlib,shutil; shutil.rmtree(pathlib.Path.home()/'.local/state/distrun'/"
            + repr(backend.socket)
            + ", ignore_errors=True)"
        )
        result.shell("python3 -c " + shlex.quote(cleanup))
        if result.directory:
            result.shell("rm -rf -- " + shlex.quote(result.directory))


@pytest.fixture
def child_processes():
    processes: list[subprocess.Popen] = []
    try:
        yield processes
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
