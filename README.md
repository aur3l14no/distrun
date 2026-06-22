# distrun

Run groups of processes locally and on remote machines.

## What It Does

- Starts services from a YAML file.
- Runs services locally, remotely, or across multiple hosts.
- Shows status and recent logs.
- Stops a project gracefully with `distrun down`.
- Reports services that are missing or left over on configured hosts.

## Design

- Uses `tmux` today.
- Runs local hosts directly and remote hosts over SSH.
- Does not require a remote daemon.
- Uses your normal OpenSSH config for ports, keys, proxy jumps, and other SSH
  options.
- Has a modular backend design. A `supervisord` backend is planned.

## Config

Declare hosts and services in `distrun.yml`:

```yaml
project: myapp
on_existing: skip # skip | restart

hosts:
  local: {}
  web:
    ssh: web-prod

services:
  ui:
    cmd: pnpm dev
    cwd: /Users/me/myapp

  watcher:
    host: local
    cmd: cargo watch

  api:
    host: web
    cmd: cargo run --release
    cwd: /srv/myapp
    env_file:
      - ./api.env
    env:
      RUST_LOG: info
    stop_timeout: 10s
```

Local services either omit `host` or use the reserved `host: local`.
`hosts.local` may be declared as a placeholder for host-level settings, but its
transport is fixed: it cannot set `ssh`. Remote hosts must set `ssh`, and SSH
targets are passed to the system `ssh` command, so put connection details in
your OpenSSH config.

Services are keyed by project, host, and service name.

`env_file` paths are local paths resolved relative to `distrun.yml`. Files are
read before the remote process starts, then sent as environment variables with
the service command. Later files override earlier files, and inline `env:`
values override `env_file` values. Supported env file lines are plain
`KEY=VALUE` entries, blank lines, and comment lines starting with `#`. distrun
does not treat `export`, quotes, or inline comments specially.

## Commands

```sh
distrun up
distrun status
distrun logs api
distrun down
```

`on_existing` controls what `up` does when a service with the same name already
exists remotely:

- `skip`: leave a running service alone; restart only if its `tmux` pane has
  exited or is missing.
- `restart`: gracefully stop and recreate the service.

## Status

`status` shows two kinds of state:

- Runtime: `running`, `exited`, or `unknown`.
- Spec: `in-sync`, `missing`, or `orphan`.

Spec state means:

- `in-sync`: the service is in the config and found remotely.
- `missing`: the service is in the config but not found remotely.
- `orphan`: the service is found remotely but not in the config.

Examples:

```text
HOST             SERVICE                  RUNTIME    SPEC
web              api                      running    in-sync
web              cron                     -          missing
web              worker                   running    orphan
```

## Current Limits

Changing a service cmd, environment, or working directory does not restart a
running process by itself. distrun does not compare a running `tmux` pane with
the service config. Use `on_existing: restart`, or run `distrun down` and then
`distrun up`.

If a service is removed from `distrun.yml` but its host is still configured,
`status` reports it as `orphan`. If an entire host is removed from `distrun.yml`,
distrun cannot find processes left behind on that host because it does not keep
a local state database or a remote project manifest.

## Feature Tests

The integration test uses Docker to start a Debian OpenSSH + tmux target, then
runs the compiled `distrun` binary against it.

```sh
scripts/run-docker-tests.sh
```

The test covers starting services, fetching logs, detecting missing/orphan
states after config changes, preserving a running remote service with
`on_existing: skip`, and stopping a whole project session including orphans.

## License

distrun is licensed under the GNU General Public License v3.0 or later. See
[LICENSE](LICENSE) for details.
