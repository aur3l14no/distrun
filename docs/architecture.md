# Architecture and lifecycle contract

## Dependency direction

```text
CLI (argparse + Rich)       Python caller
          |                     |
          +------ Manager ------+
          |          |
      config       Scope
          |          |
          +-- public model values
                     |
                Tmux transport
                 /         \
          local Python     OpenSSH
                 \         /
             same host executor
                     |
              tmux + filesystem
```

`model.py` contains immutable input/result values and graph validation.
`config.py` converts YAML into those values. `manager.py` implements the lifecycle
once; `Scope` is its exclusive transaction owner, not a second backend.
`transport.py` executes typed host operations and bounds subprocess/SSH calls.
`_host.py` is self-contained standard-library code sent to SSH hosts; it contains
host-local tmux execution and the mutation lock. `cli.py` only selects context,
presents results, and maps application exit to a CLI exit status.

No application protocol, observation type, model/controller concept, device, or
control policy belongs in this library. Applications supply readiness probes and
choose what a service failure means for their operation. RPC data transport and
application scheduling remain outside distrun.

There is one tmux backend for both persistent and scoped use. A second native
process backend is not implemented: it would add another lifecycle to maintain
without being necessary for the current local/SSH requirements. Consequently,
SDK-owned local services require tmux too.

The Linux local/loopback-SSH lifecycle gate is verified with tmux 3.6 built with
libutempter enabled. Version 3.4 could leave exited children unreaped and omit
their exit status; 3.6 includes the upstream
[libutempter/SIGCHLD fix](https://github.com/tmux/tmux/commit/fa5f3cef3d651b0eb9abfa77fc37ccade81679b5).
The same process tests and assertions pass after upgrading the execution
environment. Both the selected client and newly started servers were verified;
existing server processes keep their old version when a client is replaced.
This is an environment dependency, with no version gate or signal workaround in
the lifecycle implementation.

## Authority and identity

A service name identifies desired configuration. An immutable random token
identifies one concrete process instance. Restart creates a new token. A scope
records successful starts; stops check the token again while holding the mutation
lock. A stale handle cannot stop its replacement. This token is necessary for
safe process ownership, not a request sequence number.

The host's tmux sessions are the runtime registry. There is no second writable
PID/state database in the CLI or SDK. Each service has one session under
`distrun/<project>/<service>`, and the selected tmux socket is a separate namespace.
Mutations use resolved session IDs. Status reconciles observed instances with
configuration to derive configured/missing/orphan rows.

A host-local flock serializes operations in that namespace, including competing
CLI and SDK processes. Session construction publishes its token last. Failed
construction removes its session and log. The lock inode is retained after
shutdown so another process cannot acquire an unlinked replacement lock.

Processes must run in the foreground. distrun interrupts the pane, waits up to the
configured grace interval, kills remaining members of its process group, then
removes the session and its token-owned log. Intentionally daemonized descendants
that create their own session/process group are outside that ownership contract.

## Two lifetimes, one process owner

| Operation | Existing service | End of caller lifetime | Startup failure |
|---|---|---|---|
| `up` | Skip/restart per configuration | Keep sessions | Report partial success; remove a newly started instance that failed readiness |
| `scope` / CLI `run` | Refuse to adopt | Stop acquired instances in reverse order | Roll back acquired instances; preserve unrelated sessions |

An external persistent server can be used by many applications. Those applications
own their connections, not that server's lifetime. Private workers can instead be
created in a scope. A parent launcher may own the application process while its
scope owns private children, but it must not also independently restart those same
children.

Readiness is a startup condition. Process existence alone does not establish
application readiness. Dependencies start after their startup probes pass.
`up` does not stay resident to monitor later failures. CLI `run` and SDK
`Scope.wait()` observe exits and transport failures; context exit settles the run.
The application decides which exits matter. There is no automatic restart policy
and no separate health-state machine.

Startup deadlines are checked between probes. A command probe has a subprocess
timeout; a Python callback must bound its own execution. Host transport calls
also have their own timeout, so `start_timeout` is not a hard real-time deadline.

A scope observes its whole acquired process set once per host, during readiness
as well as `wait()`. An earlier dependency exit therefore interrupts later
startup instead of remaining hidden until the current readiness timeout. Startup
exit errors retain the process's final log lines before rollback removes them.

`scope(check=..., on_failure=...)` accepts two application callbacks. `check()`
handles caller cancellation or an independently owned local object's lifetime.
`on_failure(name, error)` handles process start, exit and observation failures:
raise to end the run, or return to acknowledge an optional failure. Each acquired
token's failure is acknowledged once, while its ownership remains in the scope
for cleanup. The scope never restarts it. A dependent process is not started
when its predecessor failed readiness, even if that failure was acknowledged.

Callbacks run on the acquiring/waiting thread, never a hidden monitor thread.
They may call `Scope.stop(name)` to stop an owned consumer before general cleanup.
The default remains transactional startup and `wait()` returning the first exit.
An empty project is a valid empty scope, useful when all application work is
local and no workers need acquiring. Cancellation latency includes the current
bounded host operation/probe and cleanup.

## Failure and cancellation

- `status` reports an unreachable host as unavailable, not as an empty inventory.
  Reachable hosts remain visible. `down` attempts reachable-host cleanup even
  when another host cannot be observed.
- `stop` resolves all selectors before mutation. Partial operational failures
  after resolution are reported; successful mutations cannot be rolled back.
- Task-body exceptions and SIGINT/SIGTERM trigger scope cleanup. If both the
  operation and cleanup fail, an exception group preserves both errors. Failed
  cleanup retains unresolved handles so a caller can retry `close()`.
- Start requests allocate their token before transport. Caller cancellation lets
  an in-flight host transaction settle before attempting token-specific rollback.
  Transport failures are reported as uncertain if ownership cannot be settled.
- SIGKILL, operator-machine failure, and network partitions cannot guarantee remote
  cleanup with this daemon-free design. Sessions may survive; inspect and stop
  them later. There is no implied lease, parent-death detector, or distributed
  atomic transaction. Stronger guarantees require a remote enforcement mechanism.
- A host mutation lock can delay another request. Host operations are bounded by
  the transport timeout; they are orchestration operations, not a control hot path.

Logs are redirected directly to an instance-owned file, avoiding tmux transcript
pipe races. Tail scans backward; follow reads bounded byte chunks and decodes UTF-8
incrementally, preserving repeated lines and multibyte characters across chunks.
Each follower owns its read offset. An exited pane retains its log until explicit
stop/down. Successful stop removes that log; failed stop does not discard evidence.
Log rotation/quotas are not implemented; keep long-lived service output bounded or
use application-managed logging.

## Deliberate differences from the Rust CLI

The YAML vocabulary and major commands remain familiar, but there is no binary
compatibility or old tmux metadata migration. This version uses one session per
service and direct log files. Rich provides a read-only live status display,
without the old TUI's full navigation/detail feature set. Structured JSON output
and the Python API are the automation interfaces.

OpenSSH is retained instead of reimplementing SSH config and ProxyJump in a Python
SSH client. PyYAML and Rich handle configuration parsing and terminal presentation.
The package needs no Rust binary and no service-specific SDK dependencies.
