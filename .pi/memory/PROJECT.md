# Project Memory: Khazad-Doom

Scope: `/home/sivanirosh/git_repos/khazad-doom`, the Rust CLI/daemon and optional Pi package that enforce bounded, observable agentic coding workflows.

Current direction:
- Use JSON Issue Slices as the atomic worker contract: explicit scope, acceptance, verification, dependencies, and `must_ask_if` stop rules.
- Keep the daemon as workflow owner: state, scheduling, worktrees, cancellation, progress, checkpoints, verification, repair, incidents, economics, and handoff artifacts.
- Keep Pi integration optional: the `khazad-doom` skill and `/khazad-monitor` overlay adapt over daemon state; they do not own workflow state or cancellation.
- Optimize for YAGNI, runtime economy, and evidence-driven handoff readiness rather than heavyweight agent-team/process machinery.
- Keep repo-local project memory here. Home-level `/home/sivanirosh/.pi/memory` is for Pi/global tooling only.
