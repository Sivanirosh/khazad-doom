# Native Pi TUI worker cancellation proof — 2026-07-08

## Scope and evidence boundary

Sacrificial cancellation run: `kd-20260708-025730-90360651`.

This was a real Khazad-Doom daemon run launched with the experimental native Pi TUI worker path and then cancelled through KD daemon IPC. Herdr is observability only. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command and cancellation

Run command:

```bash
target/debug/khazad-doom run --cockpit herdr --experimental-pi-tui-worker --slice TUI-CANCEL-01
```

After `cockpit_worker_ready`, cancellation command:

```bash
target/debug/khazad-doom cancel --run kd-20260708-025730-90360651 --reason "native TUI cancellation cleanup proof"
```

The cancel command returned:

```json
{
  "run_id": "kd-20260708-025730-90360651",
  "status": "cancel_requested",
  "active": true
}
```

## Daemon-owned cancellation evidence

`khazad-doom status --run kd-20260708-025730-90360651 --events-limit 200` reported terminal status `cancelled` and error/reason `native TUI cancellation cleanup proof`.

The daemon emitted `cockpit_worker_ready` with:

```json
{
  "slice_id": "TUI-CANCEL-01",
  "attempt": 1,
  "pane_id": "w44:p4",
  "source_of_truth": "kd_tui_result_artifact",
  "worker_slot_name": "worker-1",
  "worker_slot_index": 1,
  "worker_region": "left-worker-region"
}
```

The daemon emitted `run_cancelled`:

```json
{
  "reason": "native TUI cancellation cleanup proof"
}
```

Primary artifact paths:

```text
.workflow/runs/kd-20260708-025730-90360651/outputs/run-summary.json
.workflow/runs/kd-20260708-025730-90360651/outputs/TUI-CANCEL-01.worker.attempt-1.failure.json
.workflow/runs/kd-20260708-025730-90360651/outputs/TUI-CANCEL-01.worker.attempt-1.herdr-tui.command.json
.workflow/runs/kd-20260708-025730-90360651/outputs/TUI-CANCEL-01.worker.attempt-1.herdr-tui.prompt.md
```

There is no accepted `.herdr-tui.result.json` for this cancelled attempt.

## Cleanup and no-merge evidence

After cancellation, Herdr reported the worker pane absent:

```bash
herdr pane get w44:p4
# {"error":{"code":"pane_not_found","message":"pane w44:p4 not found"},"id":"cli:pane:get"}
```

`herdr pane list --workspace w44` showed only the Dashboard pane. This is observability evidence for pane cleanup only.

No slice merge occurred. Both branches remained at the base SHA `bb9b404c283d87866443e80b295f96e2cc149640`:

```text
khazad/kd-20260708-025730-90360651/TUI-CANCEL-01  bb9b404
khazad/kd-20260708-025730-90360651/integration    bb9b404
```

This is no slice merge evidence for the cancelled proof run.

## Blocker relation

Cancellation cleanup closed the worker pane and prevented merge. The timeout proof in `kd-20260708-025608-0b5ce10e` separately showed that closing the slot-1 TUI pane can leave the v2 layout unable to launch another native TUI worker in that slot. That lifecycle gap remains a blocker before native TUI default promotion.

## What this proves

- KD daemon IPC cancellation can terminate a real Herdr-hosted native Pi TUI worker attempt.
- KD records `run_cancelled` with the requested reason.
- KD closes the worker pane and does not accept a late worker result or merge the slice.
- Worker truth remains `kd_tui_result_artifact`; terminal text and Herdr scrollback are excluded.

## What this does not prove

- It does not prove retry after cancellation.
- It does not prove native TUI timeout, invalid-result retry, targeted repair, or multi-worker placement.
- It does not remove the slot recreation blocker found by the timeout/multi-worker proofs.
