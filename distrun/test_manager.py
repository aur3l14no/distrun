"""Core Rust lifecycle E2E, exercised through both public entry points."""

from __future__ import annotations

import json
import shlex
import subprocess
from concurrent.futures import ThreadPoolExecutor

import pytest

from distrun import CommandProbe, DistrunError
from distrun.conftest import eventually

pytestmark = pytest.mark.parametrize(
    "rig", ["local", pytest.param("ssh", marks=pytest.mark.ssh)], indirect=True
)


def test_cli_lifecycle_config_drift_and_runtime_cleanup(rig):
    api, worker, cron = (rig.service(name) for name in ("api", "worker", "cron"))
    path = rig.config(api, worker)
    rig.cli("-f", path, "up")
    first = rig.rows(path)
    assert {row["service"]: row["state"] for row in first} == {
        "api": "running",
        "worker": "running",
    }
    assert "ready" in eventually(lambda: rig.cli("-f", path, "logs", "api"))
    rig.cli("-f", path, "up")
    assert rig.rows(path) == first
    rig.config(api, cron)
    assert {(r["service"], r["state"], r["relation"]) for r in rig.rows(path)} == {
        ("api", "running", "configured"),
        ("worker", "running", "orphan"),
        ("cron", "missing", "configured"),
    }
    rig.cli("-f", path, "up")
    rig.cli("-f", path, "stop", "worker")
    rig.cli("-f", path, "recreate")
    recreated = rig.rows(path)
    assert {r["service"] for r in recreated} == {"api", "cron"}
    assert next(r["token"] for r in recreated if r["service"] == "api") != first[0]["token"]
    # Config-free observation and shutdown use the same host/namespace.
    hosts = ["--ssh", rig.host.ssh] if rig.host.ssh else []
    rows = json.loads(rig.cli("--project", rig.project, *hosts, "--json", "status"))
    assert {r["service"] for r in rows} == {"api", "cron"}
    rig.cli("--project", rig.project, *hosts, "down")
    assert all(r["state"] == "missing" for r in rig.rows(path))


def test_selection_restart_and_validate_before_stop(rig):
    api, worker = rig.service("api"), rig.service("worker")
    path = rig.config(api, worker)
    rig.cli("-f", path, "up", "api")
    assert {(r["service"], r["state"]) for r in rig.rows(path)} == {
        ("api", "running"),
        ("worker", "missing"),
    }
    rig.cli("-f", path, "up")
    before = {r["service"]: r["token"] for r in rig.rows(path)}
    rig.config(api, worker, restart=True)
    rig.cli("-f", path, "up", "api")
    after = {r["service"]: r["token"] for r in rig.rows(path)}
    assert before["api"] != after["api"] and before["worker"] == after["worker"]
    rig.cli("-f", path, "stop", "api", "absent", success=False)
    assert {r["service"]: r["token"] for r in rig.rows(path)} == after
    rig.cli("-f", path, "stop", f"{rig.host.name}/api")
    assert next(r for r in rig.rows(path) if r["service"] == "worker")["state"] == "running"


def test_concurrent_start_and_stale_handle_cannot_stop_replacement(rig):
    manager = rig.manager(rig.service("api", "echo start >> starts; exec sleep 300"))
    with ThreadPoolExecutor(max_workers=4) as pool:
        reports = tuple(pool.map(lambda _: manager.up(), range(4)))
    assert all(report.ok for report in reports)
    assert sum(o.action == "started" for report in reports for o in report.outcomes) == 1
    assert eventually(lambda: rig.shell(f"cat {shlex.quote(rig.directory)}/starts")) == "start\n"
    (original,) = rig.backend.list(rig.host, rig.project)
    manager.recreate("api").raise_for_errors()
    with pytest.raises(DistrunError, match="instance changed"):
        rig.backend.stop(rig.host, original, 0.1)
    assert manager.status()[0].state == "running"


def test_exact_project_scope_preserves_longer_name(rig):
    one = rig.manager(rig.service("api"))
    two = rig.manager(rig.service("api"), name=rig.project + "_extra")
    one.up().raise_for_errors()
    two.up().raise_for_errors()
    one.down().raise_for_errors()
    assert one.status()[0].state == "missing"
    assert two.status()[0].state == "running"
    assert "ready" in "".join(two.logs("api"))


def test_finite_logs_keep_repeated_lines_and_support_multiple_followers(rig, child_processes):
    command = (
        "printf 'early\\nearly\\n'"
        "; while [ ! -e release ]; do sleep 0.03; done; printf 'last\\nlast\\n'"
    )
    path = rig.config(rig.service("finite", command))
    rig.cli("-f", path, "up")
    eventually(lambda: rig.cli("-f", path, "logs", "finite", "-n", "2") == "early\nearly\n")
    followers = []
    for _ in range(2):
        process = subprocess.Popen(
            rig.argv("-f", path, "logs", "finite", "-f"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        child_processes.append(process)
        followers.append(process)
    rig.shell("touch " + shlex.quote(rig.directory + "/release"))
    for follower in followers:
        stdout, stderr = follower.communicate(timeout=15)
        assert follower.returncode == 0, stderr
        assert stdout == "early\nearly\nlast\nlast\n"
    assert rig.cli("-f", path, "logs", "finite", "-n", "1") == "last\n"
    assert rig.cli("-f", path, "logs", "finite", "-n", "0") == ""
    assert rig.rows(path)[0]["exit_code"] == 0


def test_scope_waits_for_dependency_and_cleans_after_body_failure(rig):
    provider = rig.service(
        "provider", "printf data > ready; exec sleep 300", ready=CommandProbe("test -s ready")
    )
    consumer = rig.service(
        "consumer",
        "test -s ready && echo consumed > consumed; exec sleep 300",
        depends_on=("provider",),
    )
    manager = rig.manager(consumer, provider)
    with pytest.raises(ValueError, match="body"):
        with manager.scope() as scope:
            assert len(scope.instances) == 2
            assert (
                eventually(lambda: rig.shell("cat " + shlex.quote(rig.directory + "/consumed")))
                == "consumed\n"
            )
            raise ValueError("body failed")
    assert all(row.state == "missing" for row in manager.status())


def test_scope_rolls_back_readiness_failure_and_preserves_external_service(rig):
    existing = rig.manager(rig.service("external"))
    existing.up().raise_for_errors()
    token = existing.status()[0].token
    manager = rig.manager(
        rig.service("first"), rig.service("never", ready=CommandProbe("false"), start_timeout=0.15)
    )
    with pytest.raises(DistrunError, match="readiness timed out"):
        with manager.scope():
            pytest.fail("startup must fail")
    assert all(row.state == "missing" for row in manager.status() if row.relation == "configured")
    assert existing.status()[0].token == token
    with pytest.raises(DistrunError, match="already exists"):
        with existing.scope():
            pytest.fail("must not adopt an existing service")
    assert existing.status()[0].token == token


def test_scope_reports_dependency_exit_without_automatic_restart(rig):
    provider = rig.service("provider", "while [ ! -e release ]; do sleep 0.03; done; exit 7")
    manager = rig.manager(provider, rig.service("consumer", depends_on=("provider",)))
    with manager.scope() as scope:
        assert scope.wait(timeout=0.05) is None
        rig.shell("touch " + shlex.quote(rig.directory + "/release"))
        ended = scope.wait(timeout=5)
        assert ended is not None and ended.service == "provider" and ended.exit_code == 7
    assert all(row.state == "missing" for row in manager.status())


def test_independent_start_survives_readiness_failure(rig):
    manager = rig.manager(
        rig.service("bad", ready=CommandProbe("false"), start_timeout=0.1),
        rig.service("dependent", depends_on=("bad",)),
        rig.service("good"),
    )
    report = manager.up()
    assert not report.ok
    assert {r.service: r.state for r in manager.status()} == {
        "bad": "missing",
        "dependent": "missing",
        "good": "running",
    }


def test_large_utf8_logs_are_not_truncated_or_corrupted(rig):
    # Initial history and subsequent >1 MiB chunks exercise both read paths.
    script = "import sys; print('initial'); sys.stdout.flush(); "
    script += (
        'exec("import time\\nfrom pathlib import Path\\n'
        "while not Path('release').exists(): time.sleep(.02)\"); "
    )
    script += "sys.stdout.write('界' * 400000 + '\\nlast\\n')"
    manager = rig.manager(rig.service("writer", "exec python3 -c " + shlex.quote(script)))
    manager.up().raise_for_errors()
    eventually(lambda: "initial" in "".join(manager.logs("writer")))
    followed = manager.logs("writer", follow=True)
    assert next(followed) == "initial\n"
    rig.shell("touch " + shlex.quote(rig.directory + "/release"))
    assert "".join(followed) == "界" * 400000 + "\nlast\n"
    assert "".join(manager.logs("writer", tail=1)) == "last\n"


def test_startup_check_cancels_pending_readiness_and_reaps_owned_process(rig):
    started = []

    def pending():
        started.append(True)
        return False

    def check():
        if started:
            raise RuntimeError("caller cancelled")

    manager = rig.manager(rig.service("pending", ready=pending, start_timeout=30))
    with pytest.raises(RuntimeError, match="caller cancelled"):
        with manager.scope(check=check):
            pytest.fail("cancelled startup became ready")
    assert all(row.state == "missing" for row in manager.status())


def test_scope_acknowledges_optional_start_failure_once_and_still_owns_cleanup(rig):
    failures = []

    def policy(service, error):
        assert service == "optional"
        failures.append(str(error))

    manager = rig.manager(
        rig.service("required"),
        rig.service("optional", ready=CommandProbe("false"), start_timeout=0.1),
    )
    with manager.scope(on_failure=policy) as scope:
        assert scope.wait(timeout=0.15) is None
        assert len(failures) == 1 and "readiness timed out" in failures[0]
        assert {row.state for row in manager.status()} == {"running"}
        scope.stop("required")
        assert {row.service: row.state for row in manager.status()} == {
            "required": "missing",
            "optional": "running",
        }
    assert all(row.state == "missing" for row in manager.status())


@pytest.mark.parametrize("logs_available", (True, False))
def test_startup_exit_reaches_failure_policy_even_when_log_transport_fails(
    rig,
    monkeypatch,
    logs_available,
):
    def unavailable(*args, **kwargs):
        raise DistrunError("log transport unavailable")

    if not logs_available:
        monkeypatch.setattr(rig.backend, "logs", unavailable)

    def policy(service, error):
        assert service == "broken"
        assert "exited before becoming ready (4)" in str(error)
        diagnostic = (
            "model-initialization-failed" if logs_available else "log transport unavailable"
        )
        assert diagnostic in str(error)
        raise RuntimeError("application applied its failure policy")

    manager = rig.manager(
        rig.service(
            "broken",
            "echo model-initialization-failed >&2; exit 4",
            ready=CommandProbe("false"),
        )
    )
    with pytest.raises(RuntimeError, match="application applied its failure policy"):
        with manager.scope(on_failure=policy):
            pytest.fail("failed model became ready")
    assert all(row.state == "missing" for row in manager.status())
