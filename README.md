# distrun

Run a small process stack across local and SSH hosts, from a CLI or Python.
The two interfaces share one orchestration library. Services run in isolated tmux
sessions; SSH hosts need Python 3.11+ and tmux, but no installed distrun package or
persistent Python agent.

## Rust to Python transition

The default branch now contains the Python implementation and is the focus of
future development. The Rust implementation is no longer actively maintained;
its source and documentation remain on the
[rust-legacy branch](https://github.com/aur3l14no/distrun/tree/rust-legacy), with
the final Rust version preserved at
[v0.2.3](https://github.com/aur3l14no/distrun/tree/v0.2.3).

The implementations use different runtime layouts. Before switching, use the
Rust CLI to stop existing services on every affected host, then start them with
the Python CLI. Python does not adopt or clean up Rust-managed services. Review
the [migration guide](docs/e2e-migration.md) for behavior differences and the
requirements below before reusing a configuration.

## Install

```sh
git clone https://github.com/aur3l14no/distrun.git
cd distrun
uv sync
uv run distrun --help
```

Use Python 3.11+ and an OpenSSH client on the operator machine, and tmux 3.6+
on each machine that runs processes. The complete Linux local/SSH lifecycle gate
is verified with tmux 3.6 and libutempter; tmux 3.4 failed process-exit reporting.
Installing a newer client does not replace already-running tmux servers. The host
executor uses POSIX signals and flock; Windows is not supported.

## CLI

```yaml
# distrun.yml
project: demo
services:
  api:
    cmd: python3 -m http.server 8091 --bind 127.0.0.1
    ready:
      cmd: "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", 8091), timeout=0.5).close()'"
      timeout: 1s
  consumer:
    cmd: "while true; do echo working; sleep 1; done"
    depends_on: [api]
```

```sh
uv run distrun up                    # detached; survives this CLI's exit
uv run distrun --json status
uv run distrun logs consumer -f
uv run distrun recreate consumer
uv run distrun down                  # includes observed orphan services

uv run distrun run                   # supervised, exclusive, task-owned run
```

`run` starts in dependency order, waits for the first service exit, and cleans its
owned services in reverse order. SIGINT/SIGTERM also trigger cleanup. Run it after
`down`: a scoped run refuses to adopt an existing service.

Remote placement adds a host inventory and selects it on the service:

```yaml
project: remote-demo
hosts:
  worker:
    ssh: my-ssh-config-alias
services:
  api:
    host: worker
    cmd: ./start-api
    cwd: /srv/api
```

OpenSSH config, ProxyJump, agents, and multiplexing work as in `ssh`. Use
`--ssh-config path` to select a separate OpenSSH configuration.

Without the original stack file, use an explicit runtime project and host scope:

```sh
uv run distrun --project remote-demo --ssh my-ssh-config-alias status
uv run distrun --project remote-demo --ssh my-ssh-config-alias down
uv run distrun --ssh my-ssh-config-alias list
uv run distrun --hosts-file hosts.yml --host worker list --all-projects
uv run distrun tui                   # read-only Rich live status, Ctrl-C exits
```

`--ssh` selects only the supplied targets. It never adds the operator machine.
`--project` disables default config discovery. Discovery only checks the current
working directory's `distrun.yml`, never its parents. Global options precede the
subcommand; `logs -f` means follow, while root `-f` means configuration file.

## Python SDK

```python
from distrun import CommandProbe, Manager, Project, Service

project = Project(
    "experiment",
    services=(
        Service(
            "producer",
            "./producer",
            ready=CommandProbe("./check-producer"),
        ),
        Service("consumer", "./consumer", depends_on=("producer",)),
    ),
)
manager = Manager(project)

# Detached operations are identical to the CLI.
manager.up().raise_for_errors()
print(manager.status())
manager.down().raise_for_errors()

# A scope owns only the concrete instances it creates.
with manager.scope() as run:
    # Do application setup or work here.
    ended = run.wait(timeout=30)
    if ended is not None:
        print(ended.service, ended.exit_code)
```

An application that composes a `Project` can expose the standard CLI without
writing another lifecycle implementation:

```python
from distrun.cli import run_cli

# Parses sys.argv by default; pass a sequence to embed an explicit invocation.
raise SystemExit(run_cli(project))
# For example: run_cli(project, ["--json", "status"])
```

The supplied project owns configuration and host scope; config/inventory override
flags are rejected. CLI parsing and signal handling belong on the main thread.
Use `Manager` directly from application code that owns its signal handling.

`Service.ready` also accepts a `Callable[[], bool]` for application-specific
protocol checks. It runs on the caller, must return promptly, and must bound any
I/O itself. A `CommandProbe` runs on the service host in its cwd/environment and
has a process timeout.

The SDK is synchronous orchestration code. Keep it outside latency-sensitive
application loops. `scope()` does not secretly start a monitoring thread: use
`run.wait()` while supervising, or call it from an application-owned monitor.
By default `wait()` returns an exit; leaving the context performs cleanup. For
application policy, `scope(on_failure=handler, check=cancel_check)` calls the
handler with a process name and error during startup and waiting. Raise to end
the run, or return to acknowledge an optional failure. One scope still owns all
tokens and cleanup; `run.stop("consumer")` can stop a consumer first. No process
is automatically restarted.

To connect to an externally owned service, connect using the application's own
client. Do not put that external service into an exclusive distrun scope.

## Configuration

- `include` and optional `include?` accept a path or list, relative to their file.
  Definition duplicates and include cycles are errors; includes are composition,
  not implicit overrides.
- `env_file` paths are relative to the declaring service file. Inline `env` wins
  over env files; service environment wins over the operator environment during
  interpolation. Env files use literal `KEY=VALUE` lines, not shell execution.
- `$VAR`, `${VAR}`, default/required/alternate parameter forms, and `$$` escaping
  are supported. Expansion is one pass. Nested expressions are not supported.
  Escape dollars intended for the service's shell, e.g. `$$PATH`.
- `cmd` is a shell command. `cwd` is interpreted on the execution host.
- `start_timeout`, `stop_timeout`, and probe `timeout` accept seconds or `ms`/`s`.
- `on_existing: skip` preserves running services. `restart` replaces selected
  services and dependencies included by the selection. Exited services restart.
- `up NAME` includes its transitive dependencies. `recreate NAME` replaces NAME
  and ensures its dependencies exist. Whole-project `recreate` removes orphans.
- An explicit empty config has an empty host scope and cannot implicitly stop
  services on the local host.

## Tests

Tests live next to the implementation. Normal tests launch real processes and
real tmux servers; no fake tmux shell scripts stand in for the lifecycle.

```sh
uv run pytest                         # local E2E + focused interpolation tests
uv run pytest --ssh-local             # adds a real, isolated loopback sshd
# Or run against an explicitly authorized host:
uv run pytest --ssh-target test-node --ssh-config ./test-ssh-config
uv run ruff check .
uv run ruff format --check .
uv run mypy distrun
```

`--ssh-local` needs `sshd` and `ssh-keygen`, uses temporary keys and a loopback
port, and shuts down the daemon and multiplexed connection in fixture teardown.
Tests own unique tmux namespaces and temporary directories on every selected host.
Explicit SSH tests fail if their requirements are missing; the ordinary gate
excludes SSH tests unless requested.

See [architecture and lifecycle guarantees](docs/architecture.md) and the
[Rust E2E migration map](docs/e2e-migration.md).

## License

distrun is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
