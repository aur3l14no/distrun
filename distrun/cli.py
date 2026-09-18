"""CLI presentation and argument routing; lifecycle behavior lives in Manager."""

from __future__ import annotations

import argparse
import json
import signal
import sys
import time
from collections.abc import Sequence
from dataclasses import asdict
from pathlib import Path

from rich.console import Console
from rich.live import Live
from rich.table import Table

from .config import load, load_hosts
from .manager import Manager
from .model import DistrunError, Host, Project
from .transport import Tmux


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description="Run process stacks locally and over SSH.")
    result.add_argument("-f", "--file", type=Path)
    result.add_argument("--project", help="Explicit runtime project; disables config discovery")
    result.add_argument("--hosts-file", type=Path)
    result.add_argument("--host", action="append", default=[], help="Exact host alias scope")
    result.add_argument(
        "--ssh", action="append", default=[], help="Ad hoc SSH target; no implicit local host"
    )
    result.add_argument("--ssh-config", type=Path, help="OpenSSH configuration file")
    result.add_argument("--socket", default="distrun", help="Isolated tmux namespace")
    result.add_argument(
        "--timeout", type=float, default=10, help="Per-host operation timeout in seconds"
    )
    result.add_argument("--json", action="store_true", help="Structured output")
    commands = result.add_subparsers(dest="command", required=True)
    for command in ("up", "recreate", "run"):
        commands.add_parser(command, help="Requires a stack configuration").add_argument(
            "services", nargs="*"
        )
    commands.add_parser("down", help="Stop a project including orphan services")
    commands.add_parser("stop").add_argument("services", nargs="+")
    for command in ("status", "list", "tui"):
        commands.add_parser(command).add_argument("--all-projects", action="store_true")
    logs = commands.add_parser("logs")
    logs.add_argument("service")
    logs.add_argument("-n", "--tail", type=int, default=80)
    logs.add_argument("-f", "--follow", action="store_true")
    return result


def context(args) -> Project:
    if args.file and args.project:
        raise DistrunError("--file and --project are mutually exclusive")
    if args.ssh and (args.hosts_file or args.host):
        raise DistrunError("--ssh cannot be mixed with --hosts-file or --host")
    if args.project and getattr(args, "all_projects", False):
        raise DistrunError("--project and --all-projects are mutually exclusive")
    path = args.file
    if path is None and not args.project and not args.ssh and not args.hosts_file:
        candidate = Path("distrun.yml")
        if candidate.is_file():
            path = candidate
    if path:
        if args.hosts_file or args.ssh:
            raise DistrunError("stack config and runtime host inventory are mutually exclusive")
        project = load(path)
    else:
        if args.command in ("up", "recreate", "run"):
            raise DistrunError(f"{args.command} requires a stack configuration")
        if not args.project and args.command in ("down", "stop", "logs"):
            raise DistrunError(f"{args.command} requires --project or a stack configuration")
        if args.ssh:
            hosts = tuple(
                Host(f"ssh{index}", target) for index, target in enumerate(dict.fromkeys(args.ssh))
            )
        else:
            hosts = load_hosts(args.hosts_file) if args.hosts_file else (Host(),)
        project = Project(args.project or "inventory", hosts=hosts)
    if args.host:
        selected = set(args.host)
        if selected - {h.name for h in project.hosts}:
            raise DistrunError("unknown host alias")
        project = Project(
            project.name,
            tuple(s for s in project.services if s.host in selected),
            tuple(h for h in project.hosts if h.name in selected),
            project.on_existing,
        )
    return project


def table(rows) -> Table:
    result = Table("HOST", "PROJECT", "SERVICE", "STATE", "RELATION", "ERROR")
    for row in rows:
        result.add_row(row.host, row.project, row.service, row.state, row.relation, row.error or "")
    return result


def execute(args, project: Project | None = None) -> int:
    project = context(args) if project is None else project
    manager = Manager(
        project, backend=Tmux(socket=args.socket, timeout=args.timeout, ssh_config=args.ssh_config)
    )
    console = Console()
    if args.command in ("status", "list", "tui"):
        all_projects = args.all_projects or (args.command == "list" and project.name == "inventory")
        if args.command == "tui":
            if not sys.stdout.isatty() or args.json:
                raise DistrunError("tui requires an interactive terminal")
            with Live(console=console, auto_refresh=False) as live:
                while True:
                    live.update(table(manager.status(all_projects=all_projects)), refresh=True)
                    time.sleep(1)
        rows = manager.status(all_projects=all_projects)
        if args.json:
            print(json.dumps([asdict(row) for row in rows]))
        else:
            console.print(table(rows))
        return int(any(row.error for row in rows))
    if args.command == "logs":
        for chunk in manager.logs(args.service, tail=args.tail, follow=args.follow):
            sys.stdout.write(chunk)
            sys.stdout.flush()
        return 0
    if args.command == "run":
        with manager.scope(*args.services) as running:
            instance = running.wait()
            return instance.exit_code if instance and instance.exit_code is not None else 1
    operation = getattr(manager, args.command)
    report = operation(*getattr(args, "services", ()))
    if args.json:
        print(json.dumps([asdict(outcome) for outcome in report.outcomes]))
    else:
        for outcome in report.outcomes:
            console.print(
                f"{outcome.host} {outcome.service or '*'} {outcome.action}"
                + (f": {outcome.error}" if outcome.error else ""),
                markup=False,
            )
    return 0 if report.ok else 1


def run_cli(project: Project, argv: Sequence[str] | None = None) -> int:
    """Expose the same CLI operations for a Project composed by a Python caller."""
    return _main(argv, project)


def main(argv: Sequence[str] | None = None) -> int:
    return _main(argv, None)


def _main(argv: Sequence[str] | None, project: Project | None) -> int:
    def interrupted(signum, frame):
        raise KeyboardInterrupt

    previous = signal.signal(signal.SIGTERM, interrupted)
    try:
        args = parser().parse_args(argv)
        if project is not None and (
            args.file or args.project or args.hosts_file or args.host or args.ssh
        ):
            raise DistrunError("a supplied Project already defines configuration and host scope")
        return execute(args, project)
    except KeyboardInterrupt:
        return 130
    except (DistrunError, OSError, ValueError, ExceptionGroup) as error:
        print(f"distrun: {error}", file=sys.stderr)
        return 1
    finally:
        signal.signal(signal.SIGTERM, previous)
