# Herdr layout v2 dogfood proof — 2026-07-08

## Scope and evidence boundary

Dogfood run: `kd-20260708-021402-0ac6eb09`.

This is a real Khazad-Doom daemon run for `LAYOUT-05` after `LAYOUT-04` closed in `kd-20260708-011930-c21b036d`. The current run's `preflight.json` records `worker_interface: "native_pi_tui"`, `experimental_pi_tui_worker: true`, selected slice `LAYOUT-05`, and base SHA `08672fb37a410b8f8c8fd0c1e4c64b1712dec181`, whose history includes the `LAYOUT-04` merge/closure.

Authoritative worker truth for this dogfood is the daemon-owned TUI result artifact only. Terminal text, Herdr scrollback, screenshots, pane labels, and Pi visual state are excluded as correctness evidence. Herdr CLI pane metadata below is used only to observe the cockpit layout shape.

## Run command and terminal state

The daemon run selected `LAYOUT-05` and launched a native Herdr-hosted Pi TUI worker. The exact invoking shell line is not persisted in the run artifacts; the reproducible KD command equivalent is:

```bash
khazad-doom run --cockpit herdr --experimental-pi-tui-worker --slice LAYOUT-05
```

The exact worker command launched by KD is recorded in:

```text
/home/sivanirosh/git_repos/khazad-doom/.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.command.json
```

That artifact records `contract: "khazad-owned-herdr-pi-tui-worker-v1"`, cwd:

```text
/home/sivanirosh/.khazad-doom/worktrees/9afe9527241f/kd-20260708-021402-0ac6eb09/LAYOUT-05
```

and argv:

```text
pi --provider openai-codex --model gpt-5.5 --thinking xhigh --no-extensions --extension /home/sivanirosh/git_repos/khazad-doom/.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.extension --name kd-tui-kd-20260708-021402-0ac6eb09-LAYOUT-05-attempt-1 @/home/sivanirosh/git_repos/khazad-doom/.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.prompt.md
```

At observation time, `khazad-doom status --run kd-20260708-021402-0ac6eb09` reported phase `worker_running`, command `pi`, attempt `1`, and no operator attention. `herdr pane process-info` reported:

- worker pane `w42:p4`: foreground process `pi`, cwd equal to the `LAYOUT-05` worktree;
- dashboard pane `w42:p2`: foreground process `khazad-doom monitor --run kd-20260708-021402-0ac6eb09 --interval-ms 1000`.

No terminal text or scrollback was used for this proof.

## Daemon cockpit events

`khazad-doom status --run kd-20260708-021402-0ac6eb09` recorded these layout-relevant daemon events:

```json
{
  "type": "cockpit_ready",
  "payload": {
    "adapter": "herdr",
    "mode": "herdr",
    "panes": ["Dashboard"],
    "planner": "cockpit_layout_v2_observability_only",
    "source_of_truth": "daemon_state",
    "workspace": "Khazad-Doom kd-20260708-021402-0ac6eb09"
  }
}
```

```json
{
  "type": "cockpit_worker_ready",
  "payload": {
    "adapter": "herdr",
    "agent_name": "kd-tui-kd-20260708-021402-0ac6eb09-LAYOUT-05-attempt-1",
    "attempt": 1,
    "layout_planner": "cockpit_layout_v2",
    "mode": "herdr",
    "pane": "worker-1: Worker kd-20260708-021402-0ac6eb09/LAYOUT-05 attempt 1",
    "pane_id": "w42:p4",
    "slice_id": "LAYOUT-05",
    "source_of_truth": "kd_tui_result_artifact",
    "terminal_id": "term_656100fb256401b1",
    "worker_region": "left-worker-region",
    "worker_slot_index": 1,
    "worker_slot_name": "worker-1",
    "workspace": "Khazad-Doom kd-20260708-021402-0ac6eb09"
  }
}
```

The worker pane id/slot observed for this single-worker dogfood is therefore `w42:p4`, `worker-1`, slot index `1`, region `left-worker-region`.

## Layout observation

Herdr observation command:

```bash
herdr pane list
herdr pane layout --pane w42:p4
```

For workspace `w42` (`Khazad-Doom kd-20260708-021402-0ac6eb09`), `herdr pane list` returned exactly two panes:

- `w42:p4` labelled as `worker-1: Worker kd-20260708-021402-0ac6eb09/LAYOUT-05 attempt 1`;
- `w42:p2` labelled `Dashboard`.

There was no pane labelled or positioned as an unused root shell, and no `Operator` pane in the current dogfood workspace. `herdr pane layout --pane w42:p4` reported one right split with ratio `0.68`; `w42:p4` occupied the left area (`width: 112`, `height: 46`) and `w42:p2` occupied the right full-height dashboard area (`width: 52`, `height: 46`).

This satisfies the one-worker v2 cockpit shape observed in a real daemon-owned native Pi TUI run: left worker region plus right full-height dashboard, with no unwanted root shell pane and no default Operator column. No remaining one-worker layout limitation was observed that would block default promotion. This run does not itself exercise the 2-4 concurrent-worker geometry; that remains covered by LAYOUT-04 tests and the layout proof script rather than by this single-worker dogfood.

## Worker result artifact and report paths

The TUI launch contract records the authoritative result path as:

```text
/home/sivanirosh/git_repos/khazad-doom/.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.result.json
```

and records:

```json
{
  "result_source": "khazad_worker_submit_worker_result_v1"
}
```

This means the worker result comes from the `submit_worker_result` tool through the `khazad_worker_submit_worker_result_v1` source and a `kd_tui_result_artifact` path. This note intentionally does not use terminal text, Herdr scrollback, screenshots, pane labels, or Pi display state as worker-result evidence.

Expected daemon-owned post-result artifacts for this same run are:

```text
.workflow/reports/kd-20260708-021402-0ac6eb09-final-report.json
.workflow/reports/kd-20260708-021402-0ac6eb09-implementation-summary.json
```

The merge target recorded by daemon state is integration branch:

```text
khazad/kd-20260708-021402-0ac6eb09/integration
```

At worker-authoring time, merge/publication happen after this worker submits the result artifact and daemon verification passes; they are not inferred from pane text.

## Checks

Slice `LAYOUT-05` declares these verification checks:

```bash
test -f docs/design/evidence/herdr-layout-v2-dogfood-2026-07-08.md
grep -q kd_tui_result_artifact docs/design/evidence/herdr-layout-v2-dogfood-2026-07-08.md
grep -q khazad_worker_submit_worker_result_v1 docs/design/evidence/herdr-layout-v2-dogfood-2026-07-08.md
grep -q 'does not prove' docs/design/evidence/herdr-layout-v2-dogfood-2026-07-08.md
```

The worker also used read-only observation commands (`khazad-doom status`, `khazad-doom inspect`, `herdr pane list`, `herdr pane layout`, and `herdr pane process-info`) to document daemon state and layout metadata.

## What this dogfood proves

- A real daemon-owned KD run after `LAYOUT-04` launched `LAYOUT-05` as a native Herdr-hosted Pi TUI worker.
- The daemon emitted a `cockpit_worker_ready` event with layout v2 metadata: `worker-1`, slot index `1`, `left-worker-region`, and `source_of_truth: "kd_tui_result_artifact"`.
- The observed current Herdr workspace had the v2 one-worker cockpit shape: a left worker pane and a right full-height Dashboard, with no unused root shell pane and no default Operator column.
- The worker-result contract is artifact-only via `khazad_worker_submit_worker_result_v1`; terminal text and Herdr scrollback are excluded.

## What this dogfood does not prove

- It does not prove 2-, 3-, or 4-concurrent-worker placement in a live daemon run.
- It does not prove worker cancellation, timeout, retry, or Herdr-unavailable fallback behavior.
- It does not prove anything from screenshots, terminal text, Herdr scrollback, pane labels, or Pi visual state.
- It does not prove daemon merge/publication before this worker submits the result artifact; the daemon final report is the post-result authority for those terminal states.
