# Native Pi TUI worker proof seam

Date: 2026-07-08

## Purpose

Test the smallest safe version of the requested UX change: a real interactive Pi TUI session visible in Herdr, steered by Khazad-Doom, without replacing the current daemon-owned JSON wrapper path.

The proof target is intentionally narrower than production worker replacement. It proves the result channel and launch shape can exist without terminal scraping; production adoption still requires daemon-owned supervision for timeout, cancellation, and merge/gate integration.

Follow-up code proof: `docs/design/evidence/pi-tui-worker-lifecycle-proof-2026-07-08.md` adds an opt-in daemon-owned TUI runner path and packaging policy, but still keeps the wrapper path as default/fallback until a real daemon dogfood run proves production behavior.

## Grounded design

- Launch a native Pi TUI in Herdr with `herdr agent start <proof-id> -- ... pi ...`.
- Load the Khazad worker extension per launch with `pi --extension <repo>/extensions/khazad-worker`.
- Keep built-in tools disabled for the proof with `--no-builtin-tools --tools submit_worker_result,ask_operator`.
- Pass KD context through environment variables, including `KHAZAD_RUN_ID`, `KHAZAD_SLICE_ID`, `KHAZAD_ATTEMPT`, `KHAZAD_WORKER_TOKEN`, and `KHAZAD_WORKER_RESULT_PATH`.
- Submit authoritative worker output through a terminating `submit_worker_result` extension tool that writes a JSON artifact directly.

## Added proof assets

- `extensions/khazad-worker/index.js`
  - Adds `submit_worker_result`.
  - Preserves existing `ask_operator` behavior.
  - Writes an atomic artifact at `KHAZAD_WORKER_RESULT_PATH`.
  - Rejects invalid worker statuses and slice-id mismatches without terminating.
- `extensions/khazad-worker/index.test.mjs`
  - Covers tool registration, missing result-path behavior, atomic artifact writes, validation failures, daemon-backed `ask_operator`, and `ask_operator` timeout responses.
- `scripts/proof-pi-tui-worker`
  - Launches a Herdr-hosted native Pi TUI proof session.
  - Does not touch daemon worker dispatch or replace the existing JSON-wrapper path.

## Evidence boundary

Authoritative for the proof:

- The JSON artifact written at `KHAZAD_WORKER_RESULT_PATH`.
- Node extension tests that call the extension tools directly.

Not authoritative:

- Terminal text.
- Herdr scrollback.
- Herdr agent state.
- Pi TUI visual state.

## Manual smoke command

Dry-run the launch command without opening Herdr/Pi:

```bash
scripts/proof-pi-tui-worker --dry-run
```

Launch a real Herdr/Pi proof pane and wait up to 120 seconds for the submitted result artifact:

```bash
scripts/proof-pi-tui-worker --wait-seconds 120
```

The script prints the proof directory and result path before launch.

## Live proof run

Executed from the repo without starting or resuming a Khazad-Doom daemon run:

```bash
scripts/proof-pi-tui-worker --wait-seconds 180
```

Observed Herdr launch response:

```text
agent name: kd-pi-tui-proof-20260708-001447
workspace_id: w7
pane_id: w7:pE
terminal_id: term_6560e652d524817d
argv: pi --no-extensions --extension /home/sivanirosh/git_repos/khazad-doom/extensions/khazad-worker --no-builtin-tools --tools submit_worker_result,ask_operator --name kd-pi-tui-proof-20260708-001447 @/home/sivanirosh/git_repos/khazad-doom/.workflow/runs/kd-pi-tui-proof-20260708-001447/prompt.md
```

Observed authoritative result artifact:

```text
.workflow/runs/kd-pi-tui-proof-20260708-001447/result.json
```

The artifact had `source: "khazad_worker_submit_worker_result_v1"`, `run_id: "kd-pi-tui-proof-20260708-001447"`, `slice_id: "TUI-PROOF-01"`, `attempt: 1`, and a valid worker-result object written by the TUI session's `submit_worker_result` tool.

## What this proves

- A native Pi TUI session can be launched by Herdr with the KD worker extension loaded per launch.
- A KD worker result can be submitted through an explicit artifact channel without reading terminal output.
- `ask_operator` remains a daemon IPC tool when the KD worker environment is present, and timeout responses remain a blocked-contract signal.

## What remains unproven before production replacement

- Daemon-owned cancellation of a Herdr-hosted interactive Pi process/session.
- Daemon-owned worker attempt timeout semantics for an interactive Pi session.
- Full worker attempt lifecycle integration: attempt events, invalid-output preservation, envelope retries, scope checks, verification gates, repair budget, and merge/handoff.
- Packaging/install policy for the worker extension if KD wants it available outside explicit `pi --extension` launches.
- Whether `herdr agent start` should replace the current `pane split`/`pane run` cockpit adapter for worker panes, or only be used by the experimental TUI runner.

## Safety conclusion

This proof justifies continuing toward an experimental native-TUI worker runner, but it does not justify replacing the current JSON-wrapper path yet. The wrapper remains the fallback and production path until daemon-owned TUI supervision and lifecycle integration are proven.
