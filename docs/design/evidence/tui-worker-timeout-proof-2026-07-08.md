# Native Pi TUI worker timeout proof — 2026-07-08

## Scope and evidence boundary

Authoritative timeout proof run: `kd-20260708-075931-ea500eb4`.

This was a real Khazad-Doom daemon run launched through the native Pi TUI worker path after the cockpit anchor fix. Herdr is observability/focus only. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command and temporary config

The run was started after temporarily changing `.workflow/khazad.json` only in the working tree:

```diff
-  "worker_attempt_timeout_seconds": 0,
+  "worker_attempt_timeout_seconds": 15,
```

Command:

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --slice TUI-TIMEOUT-01
```

The config was restored to `worker_attempt_timeout_seconds: 0` after the run.

## Daemon-owned timeout evidence

`khazad-doom status --run kd-20260708-075931-ea500eb4 --events-limit 1000` reported terminal status `failed` with error:

```text
slice TUI-TIMEOUT-01 did not become ready: worker attempt 1 exceeded worker_attempt_timeout_seconds=15; secondary failures: worker attempt 2 exceeded worker_attempt_timeout_seconds=15; worker attempt 3 exceeded worker_attempt_timeout_seconds=15
```

The daemon emitted three native `cockpit_worker_ready` events, one per bounded attempt. Each event had `source_of_truth: "kd_tui_result_artifact"`:

```text
attempt 1 pane w46:p4 source_of_truth kd_tui_result_artifact
attempt 2 pane w46:p6 source_of_truth kd_tui_result_artifact
attempt 3 pane w46:p8 source_of_truth kd_tui_result_artifact
```

The daemon emitted three `worker_attempt_timeout` events:

```text
worker attempt 1 exceeded worker_attempt_timeout_seconds=15
worker attempt 2 exceeded worker_attempt_timeout_seconds=15
worker attempt 3 exceeded worker_attempt_timeout_seconds=15
```

No `cockpit_worker_fallback` incident was emitted for this run.

Primary artifact paths:

```text
.workflow/runs/kd-20260708-075931-ea500eb4/outputs/run-summary.json
.workflow/runs/kd-20260708-075931-ea500eb4/outputs/TUI-TIMEOUT-01.worker.attempt-1.failure.json
.workflow/runs/kd-20260708-075931-ea500eb4/outputs/TUI-TIMEOUT-01.worker.attempt-1.herdr-tui.command.json
.workflow/runs/kd-20260708-075931-ea500eb4/outputs/TUI-TIMEOUT-01.worker.attempt-1.herdr-tui.prompt.md
```

## Cleanup evidence

After terminal failure, panes `w46:p4`, `w46:p6`, and `w46:p8` were absent after cleanup. This is observability evidence for pane cleanup only; it is not worker-result correctness evidence.

## What this proves

- Real native Pi TUI worker attempts are bounded by `worker_attempt_timeout_seconds`.
- Retries after timeout relaunch as native Pi TUI workers instead of falling back.
- KD emits daemon-owned `worker_attempt_timeout` events and terminal failed status.
- No terminal text, Herdr scrollback, or Pi display state was used as result evidence.

## What this does not prove

- It does not prove success-result validation, targeted repair, or multi-worker geometry; those are covered by separate proof runs.
