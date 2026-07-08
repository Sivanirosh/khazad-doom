# Native Pi TUI targeted repair proof — 2026-07-08

## Scope and evidence boundary

This proof category is **not promotion-ready** as of 2026-07-08.

The daemon's shared targeted slice repair path is covered by deterministic regression tests, but a native Pi TUI run that accepts an initial TUI result artifact, fails slice verification, launches a targeted repair worker, reruns verification, and merges only after post-repair pass has not yet been proven. Live native TUI timeout and multi-worker proofs found a pane-slot lifecycle blocker first.

Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Evidence that shared targeted repair still works

Commands run:

```bash
cargo test -q worker_attempt_failure_sequence_uses_envelope_retry_and_targeted_repair
cargo test -q repair
```

Results: passed.

The focused repair regression exercises the daemon-owned failure sequence for invalid output, scope/verification failures, targeted in-scope slice repair, verify rerun, and post-repair success. The broader `repair` test filter also passed.

These tests prove the downstream daemon repair policy remains covered, including bounded repair and post-repair verification. They do not prove native TUI placement/lifecycle for the repair worker.

## Native TUI-specific blocker

A full native TUI targeted-repair proof would require at least two native TUI worker launches in one slice lifecycle:

1. initial slice worker returns an accepted `kd_tui_result_artifact`;
2. slice verify fails;
3. targeted slice repair launches as a native TUI worker;
4. post-repair verify passes;
5. merge happens only after post-repair verification.

The timeout proof run `kd-20260708-025608-0b5ce10e` showed that after the first native TUI pane closed, later attempts could not reopen slot 1 and recorded `cockpit_worker_fallback` with `cockpit layout root pane is not available for TUI worker slot 1`.

The multi-worker run `kd-20260708-030047-bc43bb8c` similarly completed only one of four workers through a live `kd_tui_result_artifact`; other workers used fallback after Herdr placement failures.

Because targeted repair depends on reliable subsequent worker launches, native TUI repair remains blocked by the same lifecycle/layout issue.

## Current conclusion

- Targeted slice repair and post-repair verification are still covered in daemon tests.
- Native TUI targeted repair has not been proven end-to-end.
- Default promotion must wait for a follow-up fix and a real proof showing targeted repair output from a KD-owned native TUI artifact path, verify rerun, post-repair pass, and merge after post-repair verification.

## What this proves

- Shared daemon targeted repair behavior remains regression-covered.
- The current evidence distinguishes slice repair from integration repair.
- The native TUI path is not ready for default promotion without a repair-specific proof.

## What this does not prove

- It does not prove native Pi TUI targeted repair end-to-end.
- It does not prove that repair workers can reliably launch in the v2 Herdr layout after a failed first attempt.
- It does not prove promotion readiness; it records the blocker.
