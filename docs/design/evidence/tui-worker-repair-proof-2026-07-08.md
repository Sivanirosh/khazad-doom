# Native Pi TUI targeted repair proof — 2026-07-08

## Scope and evidence boundary

Authoritative targeted-repair proof run: `kd-20260708-081133-0b908fed`.

This was a real Khazad-Doom daemon run launched through native Pi TUI workers. Herdr is observability/focus only. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --slice TUI-REPAIR-01
```

The slice intentionally failed normal verification until the targeted slice repair path added marker `native-tui-targeted-repair-marker`.

## Daemon-owned repair evidence

`khazad-doom status --run kd-20260708-081133-0b908fed --events-limit 1000` reported terminal status `completed`.

The daemon emitted four native `cockpit_worker_ready` events with `source_of_truth: "kd_tui_result_artifact"`:

```text
normal attempt 1 pane w49:p4 source_of_truth kd_tui_result_artifact
normal attempt 2 pane w49:p6 source_of_truth kd_tui_result_artifact
normal attempt 3 pane w49:p8 source_of_truth kd_tui_result_artifact
slice repair 1 pane w49:pA source_of_truth kd_tui_result_artifact
```

Targeted repair evidence:

```text
.workflow/runs/kd-20260708-081133-0b908fed/outputs/TUI-REPAIR-01.worker.attempt-3.slice-repair-1.herdr-tui.result.json
.workflow/runs/kd-20260708-081133-0b908fed/outputs/TUI-REPAIR-01.worker.attempt-3.slice-repair-1.json
.workflow/runs/kd-20260708-081133-0b908fed/outputs/TUI-REPAIR-01.check.attempt-3.slice-repair-1.json
```

The repair artifact used the native submit contract:

```json
{
  "source": "khazad_worker_submit_worker_result_v1"
}
```

The daemon emitted `slice_repair_completed` with `status: "fixed"`, then merged `TUI-REPAIR-01` at repair commit `3082a220b2faa3b4fa48a1c70140d8230423eaf0` (`Add targeted TUI repair marker`).

No `cockpit_worker_fallback` incident was emitted for this run.

## What this proves

- Accepted native TUI worker results can flow into normal slice verification.
- Repeated verify failures can lead to a targeted slice repair worker launched through native Pi TUI.
- The repair result is daemon-owned via `submit_worker_result` / `kd_tui_result_artifact`.
- KD reruns verification after repair and merges only after post-repair success.
- Terminal text, Herdr scrollback, Herdr metadata, and Pi display state were not used as correctness evidence.

## What this does not prove

- It does not prove timeout handling, invalid-result retry, or multi-worker geometry; those are covered by separate proof runs.
