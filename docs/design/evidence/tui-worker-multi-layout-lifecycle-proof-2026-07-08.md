# Native Pi TUI multi-worker layout/lifecycle proof — 2026-07-08

## Scope and evidence boundary

Multi-worker run: `kd-20260708-030047-bc43bb8c`.

Command:

```bash
target/debug/khazad-doom run --cockpit herdr --experimental-pi-tui-worker --parallel 4 --slice TUI-MULTI-01A --slice TUI-MULTI-01B --slice TUI-MULTI-01C --slice TUI-MULTI-01D
```

This was a real KD daemon run with four independent slices in one parallel layer. Herdr is observability only. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Terminal outcome

The run completed and merged all four slices:

```text
TUI-MULTI-01A  merged  attempt 1  645b2916
TUI-MULTI-01B  merged  attempt 1  1e36e313
TUI-MULTI-01C  merged  attempt 1  6ff3fd26
TUI-MULTI-01D  merged  attempt 1  3b3d2169
```

Integration branch:

```text
khazad/kd-20260708-030047-bc43bb8c/integration
```

Final report paths after merge to `main`:

```text
.workflow/reports/kd-20260708-030047-bc43bb8c-final-report.json
.workflow/reports/kd-20260708-030047-bc43bb8c-implementation-summary.json
```

## Native TUI placement evidence

Only one worker emitted `cockpit_worker_ready` with native TUI result truth:

```json
{
  "slice_id": "TUI-MULTI-01A",
  "attempt": 1,
  "pane_id": "w45:p4",
  "source_of_truth": "kd_tui_result_artifact",
  "worker_slot_name": "worker-1",
  "worker_slot_index": 1,
  "worker_region": "left-worker-region"
}
```

`TUI-MULTI-01A` produced a native TUI result artifact:

```text
.workflow/runs/kd-20260708-030047-bc43bb8c/outputs/TUI-MULTI-01A.worker.attempt-1.herdr-tui.result.json
```

The other workers generated `.herdr-tui.command.json` and per-attempt extension artifacts, but did not produce `.herdr-tui.result.json` files. They completed through the fallback/direct path after Herdr placement failures.

## Blocker found

The run recorded `cockpit_worker_fallback` incidents while trying to place additional native TUI workers:

```text
herdr pane layout --pane w45:p1 exited with exit status: 1: pane_not_found
herdr pane move w45:p6 --tab w45:t1 --split down --target-pane w45:p1 --ratio 0.50 --no-focus exited with exit status: 1: target_pane_not_found
herdr pane move w45:p8 --tab w45:t1 --split down --target-pane w45:p1 --ratio 0.50 --no-focus exited with exit status: 1: target_pane_not_found
```

This means the daemon preserved workflow correctness by falling back, but the native TUI multi-worker cockpit lifecycle is not ready for default promotion. A four-worker run completing is not the same as a four-native-TUI-worker proof.

## What this proves

- KD can complete and merge four independent slices in one parallel layer while native TUI is requested.
- At least one worker in the layer ran through native TUI with `kd_tui_result_artifact` source of truth.
- Fallback incidents are daemon-owned evidence and did not alter merge correctness.

## What this does not prove

- It does not prove 2-, 3-, or 4-worker native TUI placement.
- It does not prove all workers used `khazad_worker_submit_worker_result_v1` result artifacts.
- It does not prove default promotion readiness; it identifies a multi-worker layout/lifecycle blocker.
