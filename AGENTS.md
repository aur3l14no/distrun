# distrun contributor guide

- Keep application names, protocols, and control policies out of distrun code; use cases may link to consumers.
- Use uv. Read AGENTS.local.md for local setup.
- CLI and SDK share lifecycle implementation; each process has one lifecycle owner.
- Put real E2E tests beside their module; reserve unit tests for tricky algorithms.
- Add plugin frameworks or compatibility paths only for an existing consumer.
- Record public lifecycle guarantees and limitations in docs/architecture.md.
