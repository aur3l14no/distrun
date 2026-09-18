# distrun contributor guide

- distrun is application-independent. Keep application names, protocols, and control policies out of its code; use cases may link to consumers.
- Use uv for dependencies and commands. Read AGENTS.local.md when present.
- CLI and SDK use the same lifecycle implementation. A process has one lifecycle owner.
- Prefer real end-to-end behavior tests next to their owning module. Unit tests are reserved for tricky local algorithms. Always clean processes, sessions, and files in fixtures, including on failure.
- Keep transport, host execution, orchestration, configuration, and presentation separate. Do not add plugin frameworks or compatibility paths without a consumer.
- Document externally visible lifecycle guarantees and limitations in docs/architecture.md.
