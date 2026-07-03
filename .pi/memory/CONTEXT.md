# Context

## Repository state

- Repository: `/home/sivanirosh/git_repos/khazad-doom`.
- Package metadata: `package.json` exposes the `khazad-doom` skill and optional `extensions/khazad-monitor` Pi overlay.
- Core implementation: Rust CLI/daemon for JSON Issue Slices with repo-local `.workflow` artifacts and per-user daemon runtime state.
- Dominant path: `main -> cli -> daemon Client/Server -> workflow::Manager -> artifact/state/gitutil/agent`.

## Architecture shape

- Strong seams: `agent::Runner`, `artifact::Store`, `state::Store`, daemon client/server, domain structs, workflow gate/shell execution, and black-box daemon integration tests.
- Main maintainability pressure: `src/workflow/manager.rs` is large but cohesive temporal orchestration. Refactor only around deep seams and lifecycle invariants.
- `docs/workflow-invariants.md` is the behavioral contract for release-polish refactors.

## Workflow behavior

- Slices live under `.workflow/slices/*.json`; successful runs close completed slices in the integration branch with `status: "closed"`, `closed_by_run`, and `closed_at`.
- Slice semantics: slices are bounded intent contracts and acceptance is minimum evidence, not an exhaustive mini-spec. Learning is allowed inside the JSON fence; moving the fence requires approval. Within-intent TDD discoveries may proceed inside declared `areas`; intent/path/policy expansion becomes `ask-user` or a follow-up slice.
- Runtime handoffs, raw worker outputs, checkpoints, and inspection artifacts live under `.workflow/runs/` and are gitignored.
- Final reports and handoff JSON expose runtime economics, incidents, explicit `exit_states`, and `evidence_attestation`.
- Monitor UX: core `monitor`/`watch`/`status` are harness-neutral; optional Pi overlay renders the same daemon `status` JSON as an activity feed.
