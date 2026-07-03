# Timeline

## 2026-06-25

- Designed the owned workflow framework around JSON Issue Slices, isolated worker worktrees, JSON-only outputs, verification gates, and explicit handoffs.
- Named the framework Khazad-Doom and created `/home/sivanirosh/git_repos/khazad-doom`.
- Switched the initial daemon implementation direction from Go to Rust.
- Implemented the first Rust vertical slices for repo initialization, slice validation, daemon run lifecycle, worker handoffs, verification, integration summaries, and handoff artifacts.

## 2026-06-26

- Completed Khazad-Doom Rust slices through pre-release dogfood/audit work and local install/package wiring.
- Used Khazad-Doom on an external KataForge onboarding run; the run demonstrated real workflow coverage and later informed cleanup/revert handling.
- Implemented durable progress snapshots, `status --latest`, terminal monitor/watch UX, optional Pi `/khazad-monitor` overlay, and monitor documentation.
- Added worker attempt supervision: child process liveness, timeouts, no-output warnings, graceful termination, and clearer progress/status labels.
- Added YAGNI/surgical-fix guidance to Khazad-Doom worker and repair prompts.
- Refined monitor overlay into a centered activity-feed UI over daemon status JSON.
- Elevated runtime economics to a release invariant and landed gate-driven/cached verification direction.

## 2026-06-27

- Implemented daemon health/socket hardening, stale/unhealthy socket handling, accepted-stream read timeouts, threaded accepts with serialized RPC handling, and idle raw socket regression coverage.
- Added private `WorkerExecutionContext`, shared recorded-agent economics helper, runner-selection dedupe, and parallel-layer progress annotation while preserving `workflow::Manager` as the orchestration module.
- Aligned terminal monitor and optional Pi overlay around the same activity-feed vocabulary: Todos, Run, Worker/Shell/Merge/Repair, Warn, Economics, Incidents, Activity, and Tail.
- Diagnosed and fixed installed `khazad-doom monitor --repo . --latest` no-output hang caused by a daemon left in the terminal process group; daemon startup now calls `setsid()` and tests assert process-group detachment plus unhealthy stopped-daemon reporting.
- Implemented explicit `exit_states` and `evidence_attestation` metadata in final reports/handoff JSON so worker acceptance evidence remains a claim, not self-approval.
- Split project memory: Khazad-Doom memory now lives in repo-local `.pi/memory`; home-level `/home/sivanirosh/.pi/memory` is reserved for Pi/global tooling.
