import? 'justfile.local'

# Local or remote development callers can use these same gates.
check:
    uv run ruff check .
    uv run ruff format --check .
    uv run mypy distrun

test:
    uv run pytest

e2e:
    uv run pytest --ssh-local
