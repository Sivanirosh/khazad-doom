# Context

## Repository state

- Repository: `/home/sivanirosh/git_repos/khazad-doom`; package version `0.1.0`; Rust edition 2024.
- Khazad-Doom is a local Rust CLI plus per-user daemon for JSON Issue Slice workflows. Repository contracts and durable run artifacts live under `.workflow/`; daemon runtime state is per-user.
- The Pi package ships the `khazad-doom` skill and thin `extensions/khazad-monitor` bridge. The daemon injects `extensions/khazad-worker` per native worker attempt for `ask_operator` and `submit_worker_result`.
- Pi package installation and Rust CLI installation are independent. CLI changes require reinstalling the binary and safely restarting the daemon.
- Worker profiles are operator-wide in `~/.khazad-doom/agents.toml`; repo-local agent profiles are not runtime inputs.

## Architecture

- Dominant path: `main -> cli -> daemon Client/Server -> workflow::Manager -> state/artifact/git/agent seams`.
- `workflow::Manager` remains the cohesive temporal orchestrator. Refactor around deep invariant-bearing seams, not phase boundaries or file size.
- Strong seams include `agent::Runner`, `pi_contract`, agent profiles, `artifact::Store`, SQLite state, daemon IPC, domain/wire types, gate/shell/read-model/projection/attention/events/economics/frontier/cockpit modules, Git/worktree helpers, and black-box daemon integration tests.
- `docs/workflow-invariants.md` is the behavioral contract. `src/pi_contract.rs` owns Pi JSONL parsing, event vocabulary, usage, and launch-failure classification. Unknown future fields/events remain bounded and inspectable.
- Pi is the sole real worker harness; `fake` is a deterministic test seam. The daemon owns state, policy, authorization, verification, merge, recovery, and handoff. Herdr and Pi UI are execution/display adapters.

## Workflow behavior

- Slices under `.workflow/slices/*.json` are bounded intent contracts. Learning may remain inside goal, acceptance, and literal-prefix `areas`; authority expansion requires operator approval, an authorized replan, or a follow-up slice.
- Closed dependencies are satisfied and do not rerun. Launches use isolated worktrees and immutable append-only `launch_id`; attempts, retries, repairs, and envelope retries are separate evidence.
- Active worker/repair wires contain only worker-authored facts. The daemon injects authoritative slice, attempt, launch, and repair-trigger identity.
- Worker acceptance is a claim; daemon checks and gates attest it. Verification is observationally pure. Parallel layers are integration-atomic, integration is serial and journaled, repair is bounded, and repair cannot expand authority or weaken gates.
- Completion publication occurs only after a passing gate and commits an explicit daemon-owned manifest from the isolated integration worktree.
- Questions and replans use typed transactional first-commit-wins decisions. The 60-second exact-option recommendation fallback is allowed only for bounded reversible choices with explicit rationale; ambiguous or authority-expanding cases block.
- Status/read-model data is rooted in one coherent SQLite snapshot. CLI and Pi/Herdr surfaces paint the same daemon-owned feed/actions.
- Runtime artifacts under `.workflow/runs/` retain complete authoritative evidence while response tails, telemetry, polling, economics writes, and process supervision remain bounded.

## Pi TUI and Herdr

- Native Herdr-hosted Pi TUI workers are default when cockpit placement is available. Correctness comes only from daemon-owned result artifacts; pane text, scrollback, labels, and Herdr metadata are non-authoritative.
- The JSON-wrapper remains an explicit compatibility/fallback path; direct cockpit mode remains headless. Herdr failures may degrade visibility or trigger direct fallback but cannot change workflow truth.
- Cockpit anchors are resolved live by semantic role. Pane IDs are not durable authority.
- `extensions/khazad-monitor` is a thin bridge over daemon status/focus actions, not a workflow owner. Worker-question UI may appear in the worker Pi pane only after daemon state is persisted.
- Core `status`, `watch`, `monitor`, and `attend` remain usable without Pi or Herdr. Tests that bypass CLI helpers must force direct cockpit mode to avoid opening real operator UI.

## Complexity remediation outcome

- `ASK-FALLBACK-01` and CA-01 through CA-09 closed by 2026-07-11; `docs/design/complexity-remediation-2026-07-09.md` holds the closure evidence.
- CA-03 established immutable launch identity and authorization; CA-04 transactional question/replan decisions; CA-05 durable admission and merge reconciliation; CA-06 coherent snapshot projection; CA-07 bounded runtime evidence and shared process supervision; CA-08 typed closed wires/events/provenance with legacy compatibility; CA-09 shared Rust/Node fixtures and complete CI discovery.
- CA-09 passed 414 Rust unit tests, 2 confinement tests, 49 daemon integration tests, 38 Node tests, strict clippy/check/format, fixture/slice/roadmap checks, and the reproducible 1/3/10-worker soak.

## Open-slice audit

- `docs/design/evidence/open-slice-ledger-audit-2026-07-11.json` covers all 19 records still runnable: 6 provenance-repair, 6 acceptance-only, 4 superseded-intent, 2 negative-proof, and 1 bounded-remeasurement disposition.
- The audit is non-authoritative: it closes nothing and never turns failed, blocked, or cancelled runs into completed runs. Use each record's evidence and revisit condition; do not blindly rerun implemented work.
