# Native Pi TUI invalid-result retry proof — 2026-07-08

## Scope and evidence boundary

This proof category is **not promotion-ready** as of 2026-07-08.

The daemon already has deterministic invalid worker output coverage for the shared worker-result validation and envelope retry path, but the native Pi TUI adversarial invalid-result run was not completed because live native TUI lifecycle proofs exposed a stronger blocker first: after a TUI pane is closed, later attempts/workers can fall back instead of opening another native TUI pane.

Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are explicitly excluded as correctness evidence.

## Evidence that shared invalid-output retry still works

Command run:

```bash
cargo test -q invalid_worker_output
```

Result: passed.

The black-box integration test `invalid_worker_output_pi_attempt_is_preserved_and_counted_black_box` exercises malformed worker output, durable `invalid_worker_output` artifacts/events, bounded envelope re-emission, and a later valid result. It verifies that terminal text is not parsed as fallback result evidence.

Historical daemon evidence also exists under:

```text
.workflow/runs/kd-20260707-153202-9f41ac7c/outputs/CPLX-04.worker.attempt-1.invalid-output.json
.workflow/runs/kd-20260707-153202-9f41ac7c/outputs/CPLX-04.worker.attempt-1.json
.workflow/runs/kd-20260707-153202-9f41ac7c/outputs/CPLX-04.worker.attempt-2.json
.workflow/runs/kd-20260707-153202-9f41ac7c/outputs/CPLX-04.worker.attempt-3.json
```

Those artifacts prove the daemon preserves invalid output evidence in the established wrapper path. They are not native TUI promotion evidence by themselves.

## Native TUI-specific blocker

Two live native TUI runs exposed lifecycle/layout gaps before an invalid-result retry run was attempted:

- Timeout run `kd-20260708-025608-0b5ce10e`: attempt 1 opened a native TUI worker with `source_of_truth: "kd_tui_result_artifact"`, timed out, and closed pane `w43:p4`; attempts 2 and 3 then recorded `cockpit_worker_fallback` because `cockpit layout root pane is not available for TUI worker slot 1`.
- Multi-worker run `kd-20260708-030047-bc43bb8c`: four slices completed, but only `TUI-MULTI-01A` emitted `cockpit_worker_ready` with `source_of_truth: "kd_tui_result_artifact"`; other workers recorded `cockpit_worker_fallback` incidents during native TUI placement and completed through fallback artifacts.

Because invalid-result retry requires a second worker attempt after the first TUI result artifact is rejected, this slot-recreation/fallback behavior means native TUI retry cannot be called promotion-ready yet.

## Current conclusion

- `invalid_worker_output` preservation and envelope retry remain covered for the shared daemon path.
- A native TUI result artifact with an invalid worker-result payload has not yet been proven through a full daemon retry sequence.
- The default-promotion gate remains blocked until a follow-up fix proves that retries after timeout, invalid result, and verify failure launch native TUI workers again instead of falling back.

## What this proves

- Existing daemon invalid-output retry behavior still passes regression tests.
- Terminal text, Herdr scrollback, Herdr metadata, and Pi display state are not used as fallback result evidence.
- Current native TUI lifecycle evidence is insufficient for default promotion.

## What this does not prove

- It does not prove native Pi TUI invalid-result retry end-to-end.
- It does not prove bounded retry count for repeated native TUI invalid artifacts.
- It does not prove promotion readiness; it identifies a blocker.
