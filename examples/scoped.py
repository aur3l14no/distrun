"""Run with `uv run python examples/scoped.py`; no remote host is required."""

from distrun import Manager, Project, Service

manager = Manager(Project("scoped_example", (Service("clock", "exec sleep 300"),)))
with manager.scope() as running:
    print(manager.status())
    running.wait(timeout=1)
print("Scope cleaned up")
