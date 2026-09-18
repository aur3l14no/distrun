# Rust E2E migration

The reference is Rust distrun v0.2.3:
[`tests/cli.rs`](https://github.com/aur3l14no/distrun/blob/v0.2.3/tests/cli.rs) and
[`tests/ssh_tmux.rs`](https://github.com/aur3l14no/distrun/blob/v0.2.3/tests/ssh_tmux.rs).
Assertions were ported to public behavior, not terminal column
spacing or fake-tmux script internals. Tests are next to their owning Python code.

## Switching from Rust

- Stop Rust-managed services with the Rust CLI before switching. The Python CLI
  uses a different runtime layout and cannot manage existing Rust services.
- Install Python 3.11+ and tmux 3.6+ on execution hosts. The operator also needs
  Python 3.11+ and an OpenSSH client for remote hosts.
- Check configurations against the [current configuration rules](../README.md#configuration).
  Includes reject duplicate definitions, and interpolation is single-pass.
- The Python TUI is a read-only live status display; it does not provide the Rust
  TUI's interactive service selection and log navigation.
- CLI text and JSON output are not promised to match Rust output. Check scripts
  that parse output before switching them to Python.

## Behavior coverage

| Reference behavior | Python coverage |
|---|---|
| Remote service lifecycle and configuration drift | `test_cli_lifecycle_config_drift_and_runtime_cleanup`, local + SSH |
| Selected up/restart/stop, orphan mutation | Lifecycle and selection E2E, local + SSH |
| Whole-project recreate and exact project scope | Lifecycle and exact-project E2E, local + SSH |
| Concurrent starts produce one instance | Concurrent-start E2E, local + SSH |
| Resolved instance replaced before stop | Stale-handle E2E, local + SSH |
| Logs follow through exit; repeated lines; multiple followers | Finite-follow E2E, local + SSH |
| Exact tail count; zero tail; exited transcript | Finite-follow E2E, local + SSH |
| Config includes, env files, interpolation precedence | Executable CLI E2E |
| Runtime context without configuration, empty scope | Discovery/empty-scope CLI E2E |
| Missing tmux vs missing runtime | Missing-tmux CLI E2E |
| Unavailable host preserves available results/cleanup | Real refusing-SSH transport E2E |
| Stop grace and escalation; successful log cleanup | Real signal-ignoring process E2E |
| TUI requires a terminal; help | Executable CLI E2E |

New library requirements additionally cover dependency readiness, exclusive scopes,
rollback after startup failure, unrelated-service preservation, body exceptions,
service failure observation, CLI SIGTERM, command-probe timeout, callable probes,
and large UTF-8 logs spanning stream chunks.

The only unit-test group covers interpolation operators: unset versus empty,
default/alternate/required values, escaped dollars, and single-pass substitution.

Not ported verbatim: Rust-specific text snapshots, old tmux window migration,
transcript-pipe failure injection (there is no transcript pipe), and full-screen
TUI navigation. The Python namespace and metadata are intentionally independent.
