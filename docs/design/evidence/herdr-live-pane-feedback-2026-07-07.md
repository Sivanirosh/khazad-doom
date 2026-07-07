# Herdr live worker pane feedback — 2026-07-07

Scope: UX feedback from the completed Herdr/RPL/Pi-proof dogfood sequence, especially the Herdr worker panes opened during `kd-20260707-070851-bd30eb80`.

## Observation

The Herdr worker panes are mostly silent today. That is an expected consequence of HERDR-02's correctness fence: the worker pane runs the Khazad-owned wrapper, and the wrapper redirects Pi stdout/stderr/status/result into daemon-owned artifacts instead of leaving the worker's live stream on the terminal tty.

Correctness impact: none. This is the desired truth path: Khazad-Doom reads wrapper artifacts, not terminal scrollback or Herdr UI state.

UX impact: high. The operator sees live daemon progress in the monitor pane, but the worker pane itself does not show the Pi agent's ongoing activity. During long attempts this makes the visible cockpit feel stalled even though the daemon is receiving live worker events.

## Evidence

The live data already exists on disk and is rich enough to paint:

- `khazad-doom monitor` showed fresh worker activity such as `Last worker event: 0s ago (stdout)` during live attempts, proving the daemon already observes wrapper stdout activity.
- Completed run `kd-20260707-070851-bd30eb80` preserved large live Pi event streams in wrapper artifacts, for example:
  - `RPL-01.worker.attempt-1.herdr.stdout.ndjson`: 33,071 lines.
  - `PI-PROOF-01.worker.attempt-1.herdr.stdout.ndjson`: 32,527 lines.
  - `RPL-02.worker.attempt-1.herdr.stdout.ndjson`: 28,251 lines.
- Event census for `RPL-01.worker.attempt-1.herdr.stdout.ndjson` shows token/tool/turn-level data:
  - `message_update`: 32,055
  - `message_start`: 225
  - `message_end`: 225
  - `tool_execution_start`: 117
  - `tool_execution_update`: 113
  - `tool_execution_end`: 117
  - `turn_start`: 107
  - `turn_end`: 107

## Design disposition

Create `HERDR-04 — Live worker activity painter`.

The fix should be display-only:

- Add a read-only worker activity painter that tails the wrapper `stdout.ndjson` artifact with tail-F semantics.
- Parse Pi event lines only through `src/pi_contract.rs`.
- Throttle/compact high-volume `message_update` floods for human display.
- Keep wrapper artifacts as the only correctness input.
- Never parse Herdr pane scrollback, terminal text, or Herdr agent-status metadata.
- Never accept operator input in the worker pane.
- If the painter crashes or is killed, the Pi worker and daemon-owned attempt must continue or fail exactly as they would without the painter.

Implementation shape: keep the wrapper/pid handshake and artifact writes as the load-bearing worker path, but run a foreground read-only painter in the Herdr pane that follows the same daemon-owned stdout artifact. The wrapper still writes stdout/stderr/status/exit/result artifacts, the daemon still reads those artifacts for correctness, and the painter ignores pane input and exits non-fatally when its display work is done or fails.

## Follow-up

Create `HERDR-05 — Gate and repair activity painter` after HERDR-04 if the worker painter proves useful. The gate/repair pane currently duplicates monitor summary more than it shows command activity. The same display-only model can tail daemon-owned gate/shell output artifacts for the current gate or repair command, while retaining a gate-scoped summary when no command is active.

HERDR-05/HERDR-05B disposition: the pane now runs a hidden read-only `cockpit paint-gate-activity` renderer. Its input is daemon `status`/`feed` data, including bounded shell progress tails owned by the daemon; it does not inspect Herdr pane text, scrollback, or UI state. When no active integration gate or repair command is present, the renderer uses the shared projection/gate-summary seam to show verification profile, latest gate result/failure, repair policy/state, handoff readiness, and relevant next commands instead of falling back to the generic monitor feed.

HERDR-04B disposition: the worker pane painter now treats Pi JSON as typed activity rather than stream-envelope noise. It suppresses message/turn envelope events, coalesces assistant text deltas into bounded readable text, labels only Pi-emitted reasoning/progress payloads, and renders bounded/redacted tool identity plus state/outcome when the typed event carries those fields. The raw stdout/stderr/status/exit/result artifacts remain the only daemon-owned correctness path and are not reduced by the painter.
