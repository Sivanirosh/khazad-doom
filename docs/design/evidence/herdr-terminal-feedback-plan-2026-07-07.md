# Herdr terminal feedback callback plan — 2026-07-07

Scope: UX feedback after the HERDR-04/HERDR-05 run. Operators currently need to manually ask the chat agent for a run summary after a Khazad-Doom run reaches a terminal state.

## Verified surfaces

### Herdr documented send surface

`herdr agent --help` documents:

```text
herdr agent send <target> <text>
targets accept terminal ids, unique agent names, detected/reported agent labels, and legacy pane ids
agent send writes literal text; use pane run when you want command text plus Enter
```

Design consequence: terminal feedback must use `agent send` through the `Cockpit` seam, not `pane run`. The notification should land as inert literal text in the origin agent/session; it must not auto-submit or cause the receiving agent to process completion without operator action.

### Daemon terminal choke point

`src/workflow/manager.rs` has one terminal summary choke point:

- `write_terminal_run_summary` writes `.workflow/runs/<run>/outputs/run-summary.json`.
- `execute_run` calls it before updating the run to the terminal status.
- Completed, blocked, failed, and cancelled outcomes pass through this path. Interrupted runs are produced by daemon restart recovery and have stale-origin risk.

Design consequence: terminal notification can fire after durable terminal truth exists, without making Herdr/Pi responsible for lifecycle detection.

## Decisions

- Slice ID: `HERDR-06`.
- Owner: daemon detects terminal transitions; Herdr/Pi is only an optional notification sink.
- Seam: Herdr send invocation lives behind `src/workflow/cockpit.rs`.
- Origin target storage: a run artifact such as `.workflow/runs/<run>/origin.json`, not a new state DB column unless the artifact proves insufficient.
- Inert delivery: use `herdr agent send`, never `pane run`, Enter keystrokes, or any auto-submit path.
- Payload: declarative evidence only — run id, terminal status, primary reason/summary, final SHA/handoff readiness where available, and next commands. No imperative instructions to the receiving agent.
- Dedupe: one notification per terminal transition, keyed by durable marker and surviving restart/resume. A run that blocks and later completes may generate two separate transition notifications.
- V1 terminal states: completed, blocked, failed, cancelled. Exclude interrupted in v1 because restart recovery implies stale-origin risk and the operator is already involved.
- Failure behavior: notification-send failure is a non-fatal visibility incident/event and never changes run status, verification, merge, or handoff readiness.

## Implemented shape

HERDR-06 records the optional origin target at run start in `.workflow/runs/<run>/origin.json` from `--origin-notification-target` / `KHAZAD_ORIGIN_NOTIFICATION_TARGET`. Terminal feedback writes per-transition dedupe records under `.workflow/runs/<run>/notifications/terminal-<status>.json` after `outputs/run-summary.json` exists, then sends declarative JSON evidence through the Cockpit Herdr `agent send` seam. The v1 statuses are completed, blocked, failed, and cancelled; interrupted remains excluded. Runs without an origin target do not notify. Delivery failure, missing Herdr, malformed/stale recorded target evidence is visibility-only.

## Follow-up

`HERDR-07` should reuse the same origin target and notification seam for mid-run attention states: awaiting operator questions, awaiting replan, and pending proposals. This is higher-value than terminal feedback for long runs because unseen questions/proposals can stall the run for hours, but it should not widen HERDR-06.
