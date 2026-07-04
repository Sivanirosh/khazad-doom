# Project Memory: Khazad-Doom

Scope: `/home/sivanirosh/git_repos/khazad-doom`, the Rust CLI/daemon and Pi package that enforce bounded, observable agentic coding workflows.

Current direction:
- Use JSON Issue Slices as the atomic worker contract: explicit scope, acceptance, verification, dependencies, and `must_ask_if` stop rules.
- Keep the daemon as workflow owner: state, scheduling, worktrees, cancellation, progress, checkpoints, verification, repair, incidents, economics, and handoff artifacts.
- Treat worker execution as Pi-native: Pi is the sole real worker harness, and `fake` exists only as a deterministic test seam.
- Keep daemon state and CLI status surfaces harness-neutral JSON; Pi skills/extensions adapt over daemon state and should surface progress/attention ambiently without owning workflow state or cancellation.
- Optimize for YAGNI, runtime economy, and evidence-driven handoff readiness rather than heavyweight agent-team/process machinery.
- Keep repo-local project memory here. Home-level `/home/sivanirosh/.pi/memory` is for Pi/global tooling only.
