# CPLX-05 locality cleanup evidence — 2026-07-07

## Measurement after CPLX-02..CPLX-04

- `src/workflow/manager.rs`: 8,151 lines after manual CPLX-01..CPLX-04 integration plus REPAIR-01 recovery work in this session.
- `tests/daemon_integration.rs`: 3,897 lines after extracting domain-oriented helper modules under `tests/daemon/`.
- Remaining manager hotspots are still the worker-attempt lifecycle, integration/repair lifecycle, terminal summaries, and replan/status glue.

## Cleanup performed

- Centralized Herdr Pi wrapper artifact path construction under `artifact::Store::pi_wrapper_artifacts_for_output_path`; `PiWrapperArtifacts` is now an artifact-store type, and `.herdr.*` path naming no longer lives in `src/agent.rs` or `src/workflow/manager.rs`.
- Reduced the daemon integration monolith with domain helper modules for publication, attention, cockpit, and replan assertions: `tests/daemon/{publication,attention,cockpit,replan}.rs`.

## Worker-attempt extraction decision

Deferred. Remeasurement still shows `run_slice_worker` / worker-attempt supervision as the dominant complexity hotspot, but REPAIR-01 had to change retry semantics, evidence events, and targeted repair policy in that same area. Extracting `src/workflow/worker_attempt.rs` in the same change would mix semantic repair policy with locality movement and raise review risk.

Next extraction should happen only after the REPAIR-01 behavior settles and should keep `Manager` as lifecycle facade while moving attempt-local setup/handoff/launch/result-validation/status-transition behind a typed `AttemptOutcome` interface. No shallow module was introduced solely to reduce line count.

## Snapshot/parity note

No intentional projection snapshot, parity fixture, or golden-output divergence is part of CPLX-05. The changes are locality/path-ownership cleanup plus tests/docs for the observed repair policy.
