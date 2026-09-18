"""Standard-library host executor, sent over SSH; not a daemon or public API.

All mutations serialize on a host-local flock. tmux is the runtime registry;
log paths are derived from immutable instance tokens, never from configuration.
"""

from __future__ import annotations

import base64
import fcntl
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path


class HostError(Exception):
    pass


def checked_name(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", value):
        raise HostError("invalid name")
    return value


class HostRuntime:
    def __init__(self, socket):
        self.socket = checked_name(socket)
        self.root = Path.home() / ".local/state/distrun" / self.socket
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)

    def tmux(self, *args, check=True):
        result = subprocess.run(
            ["tmux", "-L", self.socket, *args], capture_output=True, text=True, timeout=5
        )
        if check and result.returncode:
            raise HostError(
                f"tmux {args[0]}: " + (result.stderr.strip() or f"failed ({result.returncode})")
            )
        return result

    @contextmanager
    def lock(self):
        # Kept after down: unlinking a locked inode would allow a second owner.
        with (self.root / "lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def inventory(self, project=None):
        result = self.tmux(
            "list-sessions",
            "-F",
            "#{session_id}|#{session_name}|#{@distrun_project}|#{@distrun_service}|#{@distrun_token}",
            check=False,
        )
        if result.returncode:
            if "no server running" in result.stderr or "No such file or directory" in result.stderr:
                return []
            raise HostError(result.stderr.strip() or "tmux inventory failed")
        rows = []
        for line in result.stdout.splitlines():
            fields = line.split("|")
            if len(fields) != 5:
                continue
            session_id, session, owner, service, token = fields
            if not token or (project is not None and owner != project):
                continue
            if session != f"distrun/{owner}/{service}":
                continue
            checked_name(owner)
            checked_name(service)
            if not re.fullmatch(r"[a-f0-9]{32}", token):
                raise HostError("invalid runtime token")
            pane = self.tmux(
                "list-panes",
                "-s",
                "-t",
                session_id,
                "-F",
                "#{pane_dead}|#{pane_dead_status}|#{pane_pid}",
            ).stdout.strip()
            parts = pane.split("|")
            if len(parts) != 3 or parts[0] not in ("0", "1"):
                raise HostError(f"invalid pane state for {session}")
            dead, status, pid = parts
            rows.append(
                dict(
                    project=owner,
                    service=service,
                    token=token,
                    state="exited" if dead == "1" else "running",
                    exit_code=int(status) if status else None,
                    pid=int(pid),
                    session_id=session_id,
                )
            )
        return rows

    def path(self, instance):
        return self.root / instance["project"] / (instance["token"] + ".log")

    def resolve(self, expected):
        matches = [
            r for r in self.inventory(expected["project"]) if r["service"] == expected["service"]
        ]
        if not matches:
            return None
        if matches[0]["token"] != expected["token"]:
            raise HostError("runtime instance changed; refusing to affect its replacement")
        return matches[0]

    def shell(self, service, command):
        env = service.get("env", {})
        for key, value in env.items():
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) or not isinstance(value, str):
                raise HostError("invalid environment")
        prefix = ""
        if service.get("cwd"):
            prefix = "cd -- " + shlex.quote(service["cwd"]) + " && "
        return (
            prefix
            + "exec env "
            + " ".join(shlex.quote(k + "=" + v) for k, v in env.items())
            + " sh -c "
            + shlex.quote(command)
        )

    def start(self, request):
        project = checked_name(request["project"])
        service = request["service"]
        key = checked_name(service["name"])
        existing = next((r for r in self.inventory(project) if r["service"] == key), None)
        if existing:
            if request.get("exclusive"):
                raise HostError(f"scoped service {key!r} already exists")
            if existing["state"] == "running" and not request.get("restart"):
                return dict(instance=existing, action="skipped")
            self.stop(existing, service["stop_timeout"])
        token = request["token"]
        if not re.fullmatch(r"[a-f0-9]{32}", token):
            raise HostError("invalid start token")
        instance = dict(project=project, service=key, token=token)
        logfile = self.path(instance)
        logfile.parent.mkdir(mode=0o700, exist_ok=True)
        logfile.touch(mode=0o600)
        session = f"distrun/{project}/{key}"
        target = "=" + session
        # Construction is hidden from inventory until the token is published.
        created = False
        try:
            target = self.tmux(
                "new-session", "-d", "-P", "-F", "#{session_id}", "-s", session, "sleep 2147483647"
            ).stdout.strip()
            created = True
            self.tmux("set-option", "-w", "-t", target + ":", "remain-on-exit", "on")
            self.tmux("set-option", "-t", target, "@distrun_project", project)
            self.tmux("set-option", "-t", target, "@distrun_service", key)
            command = (
                self.shell(service, service["command"]) + " >" + shlex.quote(str(logfile)) + " 2>&1"
            )
            self.tmux("respawn-pane", "-k", "-t", target + ":", command)
            self.tmux("set-option", "-t", target, "@distrun_token", token)
        except BaseException:
            if created:
                self.tmux("kill-session", "-t", target, check=False)
            logfile.unlink(missing_ok=True)
            raise
        return dict(instance=self.resolve(instance), action="restarted" if existing else "started")

    def stop(self, expected, timeout):
        instance = self.resolve(expected)
        if instance is None:
            return
        target = instance["session_id"]
        if instance["state"] == "running":
            self.tmux("send-keys", "-t", target + ":", "C-c")
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                current = self.resolve(expected)
                if current is None or current["state"] == "exited":
                    break
                time.sleep(min(0.03, max(0, deadline - time.monotonic())))
            # The leader may exit during the grace period while children remain.
            # Never signal a PID from a retained, already-exited historical pane.
            try:
                os.killpg(instance["pid"], signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.tmux("kill-session", "-t", target)
        self.path(instance).unlink(missing_ok=True)
        try:
            self.path(instance).parent.rmdir()
        except OSError:
            pass

    def dispatch(self, request):
        operation = request["operation"]
        with self.lock():
            if operation == "list":
                return self.inventory(request.get("project"))
            if operation == "start":
                return self.start(request)
            if operation == "stop":
                self.stop(request["instance"], request["timeout_seconds"])
                return None
            if operation == "logs":
                current = self.resolve(request["instance"])
                if current is None:
                    raise HostError("runtime disappeared")
                with self.path(current).open("rb") as stream:
                    offset = request.get("offset")
                    if offset is None:
                        end = stream.seek(0, os.SEEK_END)
                        start = end
                        chunks = []
                        newline_count = 0
                        count = request["tail"]
                        while count and start and newline_count <= count:
                            size = min(start, 8192)
                            start -= size
                            stream.seek(start)
                            block = stream.read(size)
                            chunks.append(block)
                            newline_count += block.count(b"\n")
                        data = b"".join(reversed(chunks))
                        chunk = (
                            b"" if count == 0 else b"".join(data.splitlines(keepends=True)[-count:])
                        )
                        position = end
                    else:
                        stream.seek(offset)
                        chunk = stream.read(1024 * 1024)
                        position = stream.tell()
                return dict(
                    data=base64.b64encode(chunk).decode("ascii"),
                    offset=position,
                    state=current["state"],
                )
            if operation == "probe":
                service = request["service"]
                process = subprocess.Popen(
                    ["sh", "-c", self.shell(service, request["command"])],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    start_new_session=True,
                )
                try:
                    return process.wait(timeout=request["timeout_seconds"]) == 0
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    return False
        raise HostError(f"unknown operation {operation!r}")


def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGHUP, interrupted)
    try:
        request = json.load(sys.stdin)
        result = HostRuntime(request.pop("socket")).dispatch(request)
        print(json.dumps(dict(result=result)))
    except Exception as error:
        print(json.dumps(dict(error=str(error))))
        sys.exit(1)


if __name__ == "__main__":
    main()
