# Native Pi TUI default worker proof — 2026-07-08

## Scope and evidence boundary

Default-promotion smoke run: `kd-20260708-084311-c6b24386`.

This run intentionally omitted `--experimental-pi-tui-worker` to demonstrate that native Pi TUI workers are the default after promotion. Herdr is observability/focus only. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --slice TUI-DEFAULT-01
```

## Daemon-owned evidence

The run completed and merged `TUI-DEFAULT-01` at commit `fc376ea10d9c844d494eeba3ff19ded15794b8e2`.

`preflight.json` recorded:

```json
{
  "experimental_pi_tui_worker": true,
  "native_pi_tui_worker": true,
  "worker_interface": "native_pi_tui"
}
```

The daemon emitted `cockpit_worker_ready` with:

```json
{
  "slice_id": "TUI-DEFAULT-01",
  "attempt": 1,
  "source_of_truth": "kd_tui_result_artifact",
  "pane_id": "w4B:p4",
  "worker_slot_name": "worker-1"
}
```

The accepted result artifact was:

```text
.workflow/runs/kd-20260708-084311-c6b24386/outputs/TUI-DEFAULT-01.worker.attempt-1.herdr-tui.result.json
```

and used:

```json
{
  "source": "khazad_worker_submit_worker_result_v1"
}
```

No `cockpit_worker_fallback` incident was emitted for this run.

## What this proves

- Native Pi TUI workers are selected by default when Herdr cockpit placement is available.
- KD-owned `submit_worker_result` / `kd_tui_result_artifact` artifacts remain authoritative, not terminal text.

## What this does not prove

- It does not prove timeout, invalid-result retry, targeted repair, or four-worker placement; those are covered by separate proof runs.
