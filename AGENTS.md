# Agent Guide

## Rules

- Test user-facing behavior. Prefer integration tests.
- Use unit tests only for complex logic.
- When changing behavior from A to B, test B directly. Do not test that A is gone.
- Avoid calling current behavior `v1` in user-facing docs; reserve version labels for roadmap sections.
- Weigh alternatives, then recommend one best overall solution; do not turn fallback options into the plan.
