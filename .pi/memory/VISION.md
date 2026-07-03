# Vision

## Agentic workflow

Khazad-Doom should make agentic coding bounded, observable, and evidence-driven:

- Planning, implementation, and verification/handoff are separate phases.
- JSON Issue Slices are the atomic implementation unit.
- Workers get only the context they need, operate in isolated worktrees, return JSON-only results, and leave committed, reviewable branches.
- The daemon owns workflow state, retries, progress snapshots, checkpoints, integration gates, incidents, economics, and handoff artifacts.
- Pi skills/extensions and other harnesses are adapters over daemon state, not owners of workflow state.
- Runtime economy matters: avoid hidden extra agent turns, duplicate expensive checks, unconditional no-op repair, invisible retries, and noisy optional gates.

## Implementation style

- Prefer YAGNI and surgical fixes. One-line fixes are good when correct and readable, but correctness, tests, and workflow invariants win.
- Refactor seam-first, not file-size-first. Extract only where the new interface is smaller than the behavior it hides.
- Favor deep modules: small interfaces, high leverage, strong locality, and tests through the same seam callers use.
