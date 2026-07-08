# Native Pi TUI multi-worker layout/lifecycle proof — 2026-07-08

## Scope and evidence boundary

Authoritative post-anchor multi-worker run: `kd-20260708-082104-2724f3e9`.

Command:

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --parallel 4 --slice TUI-MULTI-02A --slice TUI-MULTI-02B --slice TUI-MULTI-02C --slice TUI-MULTI-02D
```

This was a real KD daemon run with four independent slices in one parallel layer. Herdr is observability/focus only. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Terminal outcome

The run completed and merged all four slices:

```text
TUI-MULTI-02A  merged  attempt 1  18d27fe8cbefa8f8ee26b491aa4028064cdb763d
TUI-MULTI-02B  merged  attempt 1  57e4bc6b9fc254444b7e41ae6f8b0f263746fe21
TUI-MULTI-02C  merged  attempt 1  8fd9703aeadc251557b6e359df152c6d4f9dc0d3
TUI-MULTI-02D  merged  attempt 1  11cc7d6c11816c88819d72b02c10787d16a4138d
```

Integration branch:

```text
khazad/kd-20260708-082104-2724f3e9/integration
```

Final report paths:

```text
.workflow/reports/kd-20260708-082104-2724f3e9-final-report.json
.workflow/reports/kd-20260708-082104-2724f3e9-implementation-summary.json
```

## Native TUI placement evidence

The daemon emitted four `cockpit_worker_ready` events with `source_of_truth: "kd_tui_result_artifact"`:

```text
TUI-MULTI-02A pane w4A:p4 worker-1 source_of_truth kd_tui_result_artifact
TUI-MULTI-02D pane w4A:p6 worker-2 source_of_truth kd_tui_result_artifact
TUI-MULTI-02B pane w4A:p8 worker-3 source_of_truth kd_tui_result_artifact
TUI-MULTI-02C pane w4A:pA worker-4 source_of_truth kd_tui_result_artifact
```

Each worker produced an accepted native TUI result artifact using `source: "khazad_worker_submit_worker_result_v1"`:

```text
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02A.worker.attempt-1.herdr-tui.result.json
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02B.worker.attempt-1.herdr-tui.result.json
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02C.worker.attempt-1.herdr-tui.result.json
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02D.worker.attempt-1.herdr-tui.result.json
```

No `cockpit_worker_fallback` incident was emitted for this run.

## Historical superseded run

Earlier run `kd-20260708-030047-bc43bb8c` completed four slices but is **not** native TUI promotion evidence for workers B-D because those workers fell back. It remains useful failure evidence for the stale-anchor/layout bug fixed by `COCKPIT-ANCHOR-01`.

## What this proves

- KD can launch four native Pi TUI workers in one parallel layer after the cockpit anchor fix.
- All four workers reported daemon-owned `kd_tui_result_artifact` truth.
- All four workers submitted accepted results via `khazad_worker_submit_worker_result_v1`.
- No terminal text, Herdr scrollback, or Pi display state was used as correctness evidence.

## What this does not prove

- It does not prove timeout, invalid-output retry, or targeted repair; those are covered by separate proof runs.
