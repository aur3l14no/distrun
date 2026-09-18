"""CLI behavior at the executable boundary, with real local processes."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import time
from pathlib import Path

import pytest

from distrun import Host, Manager, Project, Service, Tmux
from distrun.conftest import eventually


def test_includes_env_files_interpolation_and_working_directory(rig):
    parts = rig.local / "parts"
    parts.mkdir()
    (parts / "service.env").write_text("PREFIX=file\nFILE_ONLY=ok\n")
    (parts / "services.yml").write_text(
        "services:\n  api:\n"
        "    cmd: printf '${PREFIX}-${FILE_ONLY}-${MISSING:-fallback}'; exec sleep 300\n"
        "    env_file: service.env\n    env:\n      PREFIX: inline\n"
        f"    cwd: {rig.directory}\n    stop_timeout: 100ms\n"
    )
    config = rig.local / "distrun.yml"
    config.write_text("project: demo\ninclude: parts/services.yml\ninclude?: absent.yml\n")
    rig.cli("-f", config, "up")
    assert eventually(lambda: rig.cli("-f", config, "logs", "api")) == "inline-ok-fallback"
    rig.cli("-f", config, "down")


@pytest.mark.parametrize(
    "content",
    [
        "project: demo\nservices:\n  a: {cmd: sleep 300, depends_on: [b]}\n"
        "  b: {cmd: sleep 300, depends_on: [a]}\n",
        "project: demo\nservices:\n  a: {cmd: sleep 300, host: unknown}\n",
        "project: demo\nservices:\n  a: {cmd: sleep 300, typo: true}\n",
        "project: demo\nproject: other\n",
        "project: demo\ninclude: distrun.yml\n",
        "project: demo\ninclude: missing.yml\n",
    ],
)
def test_invalid_config_fails_before_creating_processes(rig, content):
    path = rig.local / "distrun.yml"
    path.write_text(content)
    rig.cli("-f", path, "up", success=False)
    assert rig.backend.list(rig.host) == ()


def test_explicit_empty_host_scope_and_config_free_discovery(rig):
    manager = rig.manager(rig.service("api"))
    manager.up().raise_for_errors()
    empty = rig.local / "empty.yml"
    empty.write_text("project: demo\n")
    rig.cli("-f", empty, "down")
    assert manager.status()[0].state == "running"
    rows = json.loads(rig.cli("--json", "list", cwd=rig.local))
    assert [(row["project"], row["service"]) for row in rows] == [("demo", "api")]
    # An explicit project never reads the invalid default file.
    (rig.local / "distrun.yml").write_text("invalid: [")
    rig.cli("--project", "demo", "down", cwd=rig.local)
    assert manager.status()[0].state == "missing"


def test_missing_tmux_is_unavailable_not_missing(rig, tmp_path):
    empty_path = tmp_path / "empty-bin"
    empty_path.mkdir()
    result = subprocess.run(
        rig.argv("--project", "demo", "--json", "status"),
        env=os.environ | {"PATH": str(empty_path)},
        capture_output=True,
        text=True,
    )
    assert result.returncode == 1
    (row,) = json.loads(result.stdout)
    assert row["state"] == "unavailable"


def test_unavailable_host_does_not_hide_reachable_state_or_prevent_down(rig, tmp_path):
    # A refusing SSH port is deterministic and exercises a real transport failure.
    ssh_config = tmp_path / "ssh_config"
    ssh_config.write_text("Host unreachable\n HostName 127.0.0.1\n Port 1\n BatchMode yes\n")
    backend = Tmux(socket=rig.backend.socket, ssh_config=ssh_config, timeout=1)
    local = rig.service("local_service")
    remote = Service("remote_service", "sleep 300", host="remote")
    manager = Manager(
        Project("demo", (local, remote), (Host(), Host("remote", "unreachable"))), backend=backend
    )
    report = manager.up()
    assert not report.ok
    assert {row.service: row.state for row in manager.status()} == {
        "local_service": "running",
        "remote_service": "unavailable",
    }
    report = manager.down()
    assert not report.ok and any(o.action == "stopped" for o in report.outcomes)
    assert rig.backend.list(rig.host) == ()


def test_readiness_command_timeout_kills_its_process_group(rig):
    from distrun import CommandProbe

    pidfile = Path(rig.directory) / "probe_pid"
    command = "echo $$ > probe_pid; exec sleep 300"
    service = rig.service("api", ready=CommandProbe(command, timeout=0.05), start_timeout=0.1)
    report = rig.manager(service).up()
    assert not report.ok
    pid = int(pidfile.read_text())
    with pytest.raises(ProcessLookupError):
        os.kill(pid, 0)
    assert rig.backend.list(rig.host) == ()


def test_cli_run_sigterm_reaps_scope(rig, child_processes):
    path = rig.config(rig.service("api"))
    process = subprocess.Popen(
        rig.argv("-f", path, "run"), stdout=subprocess.PIPE, stderr=subprocess.PIPE
    )
    child_processes.append(process)
    eventually(
        lambda: any(row.state == "running" for row in rig.manager(rig.service("api")).status())
    )
    process.terminate()
    stdout, stderr = process.communicate(timeout=10)
    assert process.returncode == 130, (stdout, stderr)
    assert rig.backend.list(rig.host) == ()


def test_stop_escalates_for_signal_ignoring_service_and_cleans_logs(rig):
    pidfile = Path(rig.directory) / "pid"
    code = (
        "import os,signal,time; "
        "signal.signal(signal.SIGINT,signal.SIG_IGN); "
        "signal.signal(signal.SIGHUP,signal.SIG_IGN); "
        "open('pid','w').write(str(os.getpid())); time.sleep(300)"
    )
    service = rig.service("stubborn", "exec python3 -c " + shlex.quote(code))
    manager = rig.manager(service)
    manager.up().raise_for_errors()
    eventually(pidfile.exists)
    pid = int(pidfile.read_text())
    start = time.monotonic()
    manager.down().raise_for_errors()
    assert time.monotonic() - start < 5
    eventually(lambda: not Path(f"/proc/{pid}").exists())
    log_directory = Path.home() / ".local/state/distrun" / rig.backend.socket / rig.project
    assert not log_directory.exists()


def test_sdk_callable_probe_and_owned_rollback(rig):
    flag = Path(rig.directory) / "ready"
    provider = rig.service("provider", "echo ready > ready; exec sleep 300", ready=flag.exists)
    with rig.manager(provider).scope():
        assert flag.read_text() == "ready\n"
    assert rig.backend.list(rig.host) == ()


def test_noninteractive_tui_and_help(rig):
    assert "--ssh-config" in rig.cli("--help")
    rig.cli("tui", success=False)


def test_scope_monitor_can_be_closed_from_its_owner(rig):
    from concurrent.futures import ThreadPoolExecutor

    with rig.manager(rig.service("api")).scope() as running:
        with ThreadPoolExecutor(max_workers=1) as pool:
            waiter = pool.submit(running.wait)
            running.close()
            assert waiter.result(timeout=5) is None
    assert rig.backend.list(rig.host) == ()


def test_python_project_uses_the_same_cli_lifecycle(rig, capsys):
    from distrun.cli import run_cli

    manager = rig.manager(rig.service("api"))
    options = ["--socket", rig.backend.socket, "--json"]
    assert run_cli(manager.project, [*options, "up"]) == 0
    capsys.readouterr()
    assert manager.status()[0].state == "running"
    assert run_cli(manager.project, [*options, "status"]) == 0
    assert json.loads(capsys.readouterr().out)[0]["service"] == "api"
    assert run_cli(manager.project, [*options, "down"]) == 0
    assert manager.status()[0].state == "missing"
