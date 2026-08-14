# distrun

Run a small process stack across your laptop and SSH hosts, then inspect runtime
state and logs from one terminal. distrun requires `tmux` 2.9 or newer locally
and on every SSH host, so there is no remote daemon to install.
Project runtimes use tmux sessions named `distrun/<project>`.

## Example

Put this in `./distrun.yml`:

```yaml
project: readme-demo
on_existing: skip

hosts:
  edge:
    ssh: edge # any target from your OpenSSH config

services:
  api:
    host: edge
    cmd: bash -lc 'while true; do echo edge-api $(date +%H:%M:%S) GET /health 200; sleep 1; done'

  worker:
    host: edge
    cmd: bash -lc 'while true; do echo edge-worker job complete; sleep 2; done'

  db:
    cmd: bash -lc 'while true; do echo local-db ready; sleep 2; done'
```

Then run:

```sh
distrun up
distrun status
distrun tui
```

Omit `host` for local services. Set `host` to a named entry under `hosts` for a
remote service; the `ssh` value is passed to your system `ssh` command.

## Command Context

Commands operate from a complete project configuration or directly against
existing runtime state:

```text
distrun [ROOT OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

Root options select the project and hosts before the command runs:

| Root option | Meaning |
| --- | --- |
| `-f, --file FILE` | Load a complete project configuration. |
| `-p, --project PROJECT` | Select an existing runtime project without loading a project configuration. |
| `--hosts-file FILE` | Load only the `hosts:` inventory for runtime commands. |
| `--host HOST` | Select `local` or an alias from `--hosts-file`; repeat for more. |
| `--ssh TARGET` | Select an ad-hoc SSH target; repeat for more. |

Root options must appear before the command. Options after the command belong
to that command. This lets both meanings of `-f` remain unambiguous:

```sh
distrun -f deploy.yml logs api -f
#       project file          follow logs
```

`--file` and `--project` are mutually exclusive. A complete configuration also
cannot be combined with runtime host selectors: its project and host topology
come entirely from the configuration. `--hosts-file` and ad-hoc `--ssh` targets
are also mutually exclusive; put every reusable SSH target in the inventory.

With no root selector, distrun uses `./distrun.yml` when that exact file exists.
It does not search parent directories. Without that file, runtime discovery
defaults to the local host; commands that target one project still require
`--project`.

Whether selected implicitly or with `--file`, a configuration always defines
the same exact host scope. `local` is present only when declared or when a
service omits `host`. A `--hosts-file` is also exact and never adds `local`
implicitly.

`--project`, `--hosts-file`, `--host`, and `--ssh` are explicit runtime
selectors, so they do not load an incidental `./distrun.yml` from the current
directory.

## Commands

| Command | Context | Behavior |
| --- | --- | --- |
| `up [SERVICE...]` | Configuration | Ensure all or selected configured services are running. |
| `recreate [SERVICE...]` | Configuration | Destroy and recreate the whole project or selected services. |
| `down` | Either | Stop the selected project on every selected host. |
| `stop <[HOST/]SERVICE...> [--timeout TIME]` | Either | Stop one or more observed runtime services, including orphans. |
| `status [--timeout TIME]` | Configuration | Compare the configured services with runtime state. |
| `list [--all-projects] [--timeout TIME]` | Either | List observed runtime services. Aliased as `ls`; `ps` remains a compatibility alias. |
| `logs <[HOST/]SERVICE> [-f] [-n LINES] [--timeout TIME]` | Either | Read or follow one observed service's logs. |
| `tui [--all-projects] [-n LINES] [--timeout TIME]` | Either | Browse runtime services and logs in a read-only interface. |

`up`, `recreate`, and `status` operate on desired configuration, so they require
an explicit `--file` or `./distrun.yml`. `down`, `stop`, and `logs` can instead
use a runtime `--project` context. `list` and `tui` can also discover every
project in the selected host scope without choosing a project first.

Runtime observation defaults to a 5-second timeout per host. `--timeout` is
available on `status`, `list`, `stop`, `logs`, and `tui`. For `logs -f`, it
limits service resolution. When the selected service has exited, `logs -f`
reports the fallback and performs a finite read that also uses this timeout; a
running service's follow stream is intentionally unbounded.

### Lifecycle

Use optional service names to limit `up` and `recreate`:

```sh
distrun up api worker
distrun recreate api
```

With no service arguments, `recreate` stops the whole runtime project and then
starts every configured service. This is the usual way to apply a changed
configuration: orphan services are removed because they are stopped and not
created again.

```sh
distrun recreate
```

With service arguments, `recreate` affects only those configured services. It
does not clean up unrelated orphans. `down` always stops the whole project;
`stop` is the explicit service-level operation.

`on_existing: skip` leaves running services alone and creates only missing or
exited services when `up` runs. `on_existing: restart` stops and recreates every
selected configured service that is already running.

### Runtime Operations

Use `--project` when there is no project configuration, or when you deliberately
want to operate only on observed runtime state:

```sh
distrun -p readme-demo list
distrun -p readme-demo logs api -f
distrun -p readme-demo stop old-worker
distrun -p readme-demo down
```

Runtime mode defaults to `local` only when no host selector is present. An
explicit selector defines the exact scope:

```sh
distrun -p readme-demo --host local list
distrun -p readme-demo --ssh edge --ssh gpu down
distrun -p readme-demo --host local --ssh edge list
```

`--host` is always a named selection. Without `--hosts-file`, its only valid
value is `local`; use `--ssh` for an ad-hoc OpenSSH target. This keeps a command
such as `--ssh edge down` from silently including the local machine.

### Host Inventories

For repeated runtime operations across SSH hosts, put their definitions in a
hosts inventory:

```yaml
hosts:
  edge:
    ssh: user@10.0.0.8
  gpu:
    ssh: gpu-box
```

`--hosts-file` reads only the `hosts:` section; `project:` and `services:` are
ignored if present. With no `--host`, exactly those entries are selected. When
both options are present, each `--host` selects an alias from the inventory and
an unknown alias is an error. Add an explicit `local: {}` entry when local
runtime state should be included:

```sh
distrun -p readme-demo --hosts-file hosts.yml list
distrun -p readme-demo --hosts-file hosts.yml --host edge logs api
```

### Project Discovery

In a configuration context, `list` and `tui` use the configuration's hosts and
show its project by default. Add `--all-projects` to inspect every distrun project
on the same hosts:

```sh
distrun list
distrun list --all-projects
distrun tui --all-projects
```

`--all-projects` cannot be combined with root `--project`: one asks for every
project while the other selects exactly one.

Without a configuration or `--project`, `list` and `tui` discover all projects
on the selected runtime hosts. A hosts inventory makes cross-host discovery
explicit:

```sh
distrun --hosts-file hosts.yml list --all-projects
```

## Service Selectors

`logs` and `stop` resolve services from observed runtime state, not from the
configured service list. They can therefore inspect and stop an orphan service.

A bare service name works when it identifies one service across the selected
hosts. If the same name exists on more than one host, qualify it as
`HOST/SERVICE`:

```sh
distrun logs edge/api
distrun stop edge/api gpu/api
```

An ambiguous bare name fails and reports the qualified candidates. `logs`
accepts exactly one selector; `stop` accepts one or more.

Multiple same-named runtime instances on one host indicate a damaged runtime
state, usually from an interrupted or concurrent older start. `logs` refuses to
guess between them. `stop HOST/SERVICE` stops every matching instance, and
`recreate SERVICE` removes them before creating one replacement. `status` puts
the runtime identity in `ISSUE`, while `list` and the TUI append it to the
service name, so the instances remain distinguishable.

## Configuration

Use `include` to split config across required files, and `include?` for optional
additions that may be absent:

```yaml
include:
  - ./hosts.yml
  - ./services/api.yml
include?: ./distrun.local.yml
```

Host and service names must be unique across the root file and all includes;
`include?` controls whether a missing file is accepted, not whether duplicate
definitions override one another.

`env_file` values are read on the machine running distrun, then sent with the
service command. Later env files override earlier ones, and inline `env:`
overrides `env_file`.

String values support Docker Compose-style interpolation: `$KEY`, `${KEY}`,
`${KEY:-default}`, `${KEY:+replacement}`, `${KEY:?error}`, and related forms.
See [Interpolation](docs/interpolation.md) for the exact loading layers.

## Status

`status` reports runtime state (`running`, `exited`, `unknown`, `missing`,
`unavailable`), the factual relationship to configuration (`configured` or
`orphan`), and runtime integrity issues. `configured` means the service is
declared in the file; it does not claim that a running command, environment, or
working directory match. A duplicate runtime is shown as
`duplicate:<instance-id>` in `ISSUE`; older runtimes without a distrun runtime
ID use their tmux window ID.

```text
HOST             SERVICE                  RUNTIME      RELATION     ISSUE
edge             api                      running      configured   -
edge             metrics                  missing      configured   -
edge             worker                   running      orphan       -
```

Status checks query hosts in parallel and use a per-host timeout, defaulting to
5 seconds. If a host cannot be observed before the timeout, configured services
on that host are reported as `unavailable` instead of being misreported as
`missing`. `status` and `list` still print every available row, then exit nonzero
to tell automation that the observation was incomplete.

## Logs

`logs` reads one service at a time and preserves its original output without a
host or service prefix. It returns the latest 80 lines by default:

```sh
distrun logs api
distrun logs api -n 200
distrun logs api -f
```

`-f, --follow` keeps streaming after the initial output. `-n, --tail LINES`
changes the number of initial lines.

New services write a combined stdout/stderr PTY transcript to a distrun-owned
per-runtime log file using `tmux pipe-pane`. The pipe is attached before the
service command starts, so early output is included. `logs` uses `tail` and
`logs -f` follows that same append-only file. The transcript grows while the
runtime exists and is removed with that runtime by `stop`, `down`, or
`recreate`. Existing runtimes created by an older distrun can still be read from
tmux scrollback, but must be recreated before they can be followed. If the
transcript pipe detaches while a service is still running, log reads fail with a
recreate prompt instead of silently showing stale output.

## TUI

`distrun tui` is a read-only runtime and log browser. It uses the same project
and host context as `list`, shows observed services, and loads recent logs for
the selected service. It provides navigation, filtering, refresh, and quit
controls, but it cannot start, stop, or recreate services.

The log pane uses the same per-runtime transcript as `distrun logs`. Terminal
control sequences are removed before rendering so raw colored process output
cannot corrupt the TUI. Use `PgUp`/`PgDn`, `Home`, and `End` to read or resume
following the latest lines.

## Failure Semantics

Lifecycle operations are best-effort across hosts and services. distrun prints
every successful mutation and every failed target, then exits nonzero when any
target failed. Once a target returns an error, later independent targets are
still attempted. distrun does not roll back successful remote process
operations: there is no reliable transaction boundary across SSH hosts.

## Tests

The integration test starts a Debian OpenSSH + tmux container and runs the
compiled `distrun` binary against it:

```sh
scripts/run-docker-tests.sh
```

It covers remote start, logs through service exit, missing/orphan detection after
config changes, `on_existing: skip`, project recreation, and project shutdown.

## Current Limitations

distrun does not compare a running tmux pane with the current service command,
environment, or working directory. Use `on_existing: restart` or `distrun
recreate` when you want to recreate processes after configuration changes.

If a whole host is removed from the config, distrun cannot discover leftover
processes on that host because it does not keep a local state database or remote
manifest. Use a runtime host selector or hosts inventory to inspect that host
explicitly.

Host aliases are runtime identity boundaries. If two aliases point to the same
SSH endpoint, distrun does not canonicalize or merge their observations; define
one alias per endpoint within an operation scope.

Per-runtime PTY transcripts combine stdout and stderr and retain terminal escape
bytes. They are append-only and have no size limit while the runtime exists.
Recreating or removing a runtime removes its transcript. distrun does not promise
remote follower cleanup after the local CLI is forcibly killed. On natural
service exit, log reads use a bounded drain delay because tmux has no durable
transcript-flushed event; an unusually slow filesystem may therefore omit the
last bytes from that read even though the transcript remains readable later.

Service and project stops have an internal per-host deadline: the configured
`stop_timeout` plus five seconds for transport and tmux coordination. `down`
stops hosts concurrently, so one unavailable host does not prevent the others
from being attempted. A timed-out stop may have partially completed because the
remote command may already have started; distrun does not roll back successful
hosts. Observation and start phases used by `up` and `recreate` do not yet have
the same outer deadline.

If a start is forcibly killed after its transcript directory is created but
before the runtime becomes ready, a later `up` repairs the tmux window but may
leave that incomplete transcript directory behind. distrun has no global log
prune command or state manifest yet.

## License

distrun is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE)
for details.
