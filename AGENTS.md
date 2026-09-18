# distrun contributor guide

- Keep application-specific names, protocols, and control policies out of distrun code; use cases may link to consumers.
- Use uv. Read AGENTS.local.md when present.
- Keep tests beside the code they cover. Use real E2E tests; reserve unit tests ONLY for tricky local algorithms.
- Record public lifecycle guarantees and limitations in docs/architecture.md.
