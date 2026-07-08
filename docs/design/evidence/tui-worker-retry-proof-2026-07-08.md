# Native Pi TUI invalid-result retry proof — 2026-07-08

## Scope and evidence boundary

Authoritative invalid-result retry proof run: `kd-20260708-080118-a741f423`.

This was a real Khazad-Doom daemon run launched through native Pi TUI workers. Herdr is observability/focus only. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --slice TUI-RETRY-01
```

The slice intentionally submitted a daemon-invalid native TUI result first, then completed through the daemon retry envelope.

## Daemon-owned invalid-output evidence

`khazad-doom status --run kd-20260708-080118-a741f423 --events-limit 1000` reported terminal status `completed`.

The daemon emitted native `cockpit_worker_ready` events for both worker launches. Each had `source_of_truth: "kd_tui_result_artifact"`:

```text
attempt 1 pane w47:p4 source_of_truth kd_tui_result_artifact
retry launch pane w47:p6 source_of_truth kd_tui_result_artifact
```

The daemon preserved invalid output at:

```text
.workflow/runs/kd-20260708-080118-a741f423/outputs/TUI-RETRY-01.worker.attempt-1.invalid-output.json
```

The `invalid_worker_output` parse error was:

```text
worker JSON failed validation: missing acceptance evidence for "A real native Pi TUI worker attempt writes a kd_tui_result_artifact via submit_worker_result whose result payload is daemon-invalid, producing preserved invalid_worker_output evidence."
```

The rejected payload came through the native TUI artifact envelope with:

```json
{
  "source": "khazad_worker_submit_worker_result_v1"
}
```

The retry completed and merged slice `TUI-RETRY-01` at commit `67765ba323efcb33ff26c345dfeaab91bc49b8ea`.

No `cockpit_worker_fallback` incident was emitted for this run.

## What this proves

- A native Pi TUI worker can submit a `kd_tui_result_artifact` via `submit_worker_result` whose payload is daemon-invalid.
- KD preserves the invalid payload as `invalid_worker_output` evidence.
- KD retries through another native Pi TUI worker launch instead of falling back.
- Terminal text, Herdr scrollback, Herdr metadata, and Pi display state were not used as fallback result evidence.

## What this does not prove

- It does not prove timeout handling, targeted repair, or multi-worker geometry; those are covered by separate proof runs.
