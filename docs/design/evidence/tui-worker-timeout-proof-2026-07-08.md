# Native Pi TUI worker timeout proof — 2026-07-08

## Scope and evidence boundary

Sacrificial timeout run: `kd-20260708-025608-0b5ce10e`.

This was a real Khazad-Doom daemon run launched with the experimental native Pi TUI worker path. Herdr is treated only as observability/focus. Correctness evidence comes from KD daemon state, events, and artifacts. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi display state are not correctness evidence.

## Command and temporary config

The run was started after temporarily changing `.workflow/khazad.json` only in the working tree:

```diff
-  "worker_attempt_timeout_seconds": 0,
+  "worker_attempt_timeout_seconds": 20,
```

Command:

```bash
target/debug/khazad-doom run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --slice TUI-TIMEOUT-01
```

The config was restored to `worker_attempt_timeout_seconds: 0` after the run. The run preflight records `allow_dirty: true`, `status_porcelain: "M .workflow/khazad.json"`, `experimental_pi_tui_worker: true`, and `worker_interface: "native_pi_tui"`.

## Daemon-owned timeout evidence

`khazad-doom status --run kd-20260708-025608-0b5ce10e --events-limit 200` reported terminal status `failed` with error:

```text
slice TUI-TIMEOUT-01 did not become ready: worker attempt 1 exceeded worker_attempt_timeout_seconds=20; secondary failures: worker attempt 2 exceeded worker_attempt_timeout_seconds=20; worker attempt 3 exceeded worker_attempt_timeout_seconds=20
```

The daemon emitted `cockpit_worker_ready` for attempt 1 with:

```json
{
  "slice_id": "TUI-TIMEOUT-01",
  "attempt": 1,
  "pane_id": "w43:p4",
  "source_of_truth": "kd_tui_result_artifact",
  "worker_slot_name": "worker-1",
  "worker_slot_index": 1,
  "worker_region": "left-worker-region"
}
```

The daemon emitted three `worker_attempt_timeout` events, one for each bounded worker attempt:

```text
worker attempt 1 exceeded worker_attempt_timeout_seconds=20
worker attempt 2 exceeded worker_attempt_timeout_seconds=20
worker attempt 3 exceeded worker_attempt_timeout_seconds=20
```

Primary artifact paths:

```text
.workflow/runs/kd-20260708-025608-0b5ce10e/outputs/run-summary.json
.workflow/runs/kd-20260708-025608-0b5ce10e/outputs/TUI-TIMEOUT-01.worker.attempt-1.failure.json
.workflow/runs/kd-20260708-025608-0b5ce10e/outputs/TUI-TIMEOUT-01.worker.attempt-1.herdr-tui.command.json
.workflow/runs/kd-20260708-025608-0b5ce10e/outputs/TUI-TIMEOUT-01.worker.attempt-1.herdr-tui.prompt.md
```

No accepted `.herdr-tui.result.json` was present for the timed-out attempt. The slice and integration branches remained at the base SHA `bb9b404c283d87866443e80b295f96e2cc149640`; no slice merge occurred.

## Cleanup evidence

After terminal failure, Herdr reported the attempt-1 worker pane absent:

```bash
herdr pane get w43:p4
# {"error":{"code":"pane_not_found","message":"pane w43:p4 not found"},"id":"cli:pane:get"}
```

`herdr pane list --workspace w43` showed only the Dashboard pane. This is observability evidence for pane cleanup; it is not worker-result correctness evidence.

## Blocker found

Timeout cleanup itself worked for the first native TUI attempt: KD cancelled the worker, emitted `worker_attempt_timeout`, closed the worker pane, preserved failure artifacts, and failed the run without accepting a result.

However, attempts 2 and 3 recorded `cockpit_worker_fallback` incidents:

```text
cockpit layout root pane is not available for TUI worker slot 1
```

That means the v2 layout did not preserve/recreate an available worker slot after closing the first TUI worker pane. Later timeout retries fell back instead of opening new native TUI panes. This is a default-promotion blocker for native Pi TUI retry/lifecycle behavior.

## What this proves

- A real native Pi TUI worker attempt can be bounded by `worker_attempt_timeout_seconds`.
- KD emits daemon-owned `worker_attempt_timeout` events and terminal failed status.
- KD closes the Herdr worker pane after timeout cancellation.
- No terminal text, Herdr scrollback, or Pi display state was used as result evidence.

## What this does not prove

- It does not prove native TUI retries remain native TUI after timeout; this run found the opposite blocker.
- It does not prove default-promotion readiness.
- It does not prove success-result validation, targeted repair, or multi-worker geometry.
