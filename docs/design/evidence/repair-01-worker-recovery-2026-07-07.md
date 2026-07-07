# REPAIR-01 worker recovery evidence — 2026-07-07

Motivating run: `kd-20260707-153202-9f41ac7c`.

Observed classes encoded by this slice:

1. CPLX-04 attempt 1 produced an invalid worker evidence envelope. The invalid output artifact remains evidence: `.workflow/runs/kd-20260707-153202-9f41ac7c/outputs/CPLX-04.worker.attempt-1.invalid-output.json`.
2. CPLX-04 later hit a daemon path guard: `worker changed files outside slice areas: src/workflow/read_model.rs`. Scope violations remain hard stops unless an accepted RPL-02B-style grant expands authority.
3. CPLX-04 also hit mechanical daemon-owned verify failures, including clippy errors, in `CPLX-04.check.attempt-2.json` / `CPLX-04.check.attempt-3.json`.
4. CPLX-03 reached `ready_to_merge` in the same failed layer and was intentionally not merged. Parallel layer atomicity is preserved; ready siblings are reported as preserved-but-unmerged evidence, not silently published.

Implemented recovery bounds:

- Invalid, missing, or schema-invalid worker JSON gets the separate default envelope re-emission budget of two retries against the existing worker head/output evidence.
- Envelope retries are recorded as agent calls and attempt evidence but do not consume the implementation-attempt budget.
- Mechanical `command_failed` slice checks get at most one targeted in-scope slice-repair attempt after normal worker attempts would otherwise become terminal.
- Scope violations are not auto-repaired, auto-reverted, or auto-authorized.
- Unknown failure classes keep existing retry/block behavior.

Repair authority maxim: A false positive that auto-repairs beyond authority is worse than a false negative that blocks.

Regression evidence added in source tests:

- `worker_attempt_failure_sequence_uses_envelope_retry_and_targeted_repair`
- `invalid_worker_output_final_envelope_failure_preserves_terminal_artifacts`
- Updated invalid-worker-output Pi black-box expectations so envelope retries are counted in economics without inflating implementation attempts.
