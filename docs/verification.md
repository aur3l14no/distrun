# Verification — 2026-09-15

Executed in an isolated Linux workstation checkout with Python 3.11.15. The SSH
variants used a real OpenSSH server bound to a temporary loopback port, temporary
keys, and an independent tmux namespace for every case. This tests the actual SSH
execution path, not a mocked transport. A separate physical target was not used.

- `uv run pytest -q --ssh-local`: **47 passed**, 30.14 seconds.
  - 36 end-to-end behavior cases, including 10 through real SSH.
  - 11 focused interpolation unit cases.
- `uv run mypy distrun`: passed, 12 Python source/test modules.
- Ruff lint and formatting checks: passed.
- `uv build`: wheel and source distribution built successfully.
- Installed-wheel smoke test outside the source checkout: SDK start/status/down,
  CLI JSON status, scoped execution and cleanup all passed.
- Wheel contents: typing marker present; test modules and conftest excluded.
- Dependency boundary search: no application-specific dependency or consumer name
  in library code, examples, or general documentation.

The installed-wheel test used a fresh uv environment. Test tmux servers, temporary
service directories, SSH daemon/connection, credentials, and smoke-test files were
cleaned up. The temporary remote checkout was removed after copying the lockfile
and build artifacts to the local repository.

## Composed-project CLI follow-up

The public `run_cli(Project, argv)` entry was exercised by an application-provided
Project through CLI start/status/down and checked through the SDK. The follow-up
suite passed **48 cases** on Python 3.12.3 (32.49 seconds), including the same 10
real SSH cases. The isolated Python 3.11.15 locked development environment passed
mypy and Ruff checks. SSH fixture files were placed on `/tmp`, whose permissions
support OpenSSH private keys; the workspace mount does not preserve those modes.

Startup errors now retain the exited service's exit code and last 40 log lines
before scoped rollback removes owned resources.

## Python mainline migration — 2026-09-18

Verified the Python mainline on Linux with Python 3.11.15 and tmux 3.6, using
the locked dependencies:

- `uv run --locked pytest -q --ssh-local`: **56 passed**, 34.50 seconds,
  including real local processes and an isolated loopback SSH server.
- Ruff lint and formatting checks passed; mypy passed for all 12 source files.
- `uv build`: wheel and source distribution built successfully with the retained
  Apache-2.0 license and Python README metadata.
