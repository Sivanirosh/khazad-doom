# PI-05 — Render-ready status projection

Matrix row: [00-matrix.md](00-matrix.md) → PI-05. Status: `in_progress` after the 2026-07-06 truth audit: projection code exists, but CLI duplication and missing parity/grep tests prevent done-level acceptance.
Depends on: PI-00. Independent of PI-01..PI-03. Should land before PI-04 reaches `done`.

## Problem being removed

Human-facing CLI paths can drift if status, watch, and monitor each interpret run details independently. Every new event kind, incident type, or phase should be interpreted once, daemon-side, and then painted by terminal surfaces. The activity-feed vocabulary was already aligned by hand once; this slice makes that alignment structural and supports read-only adapters without reviving the old Pi monitor overlay.

## Scope

- New module (proposed `src/workflow/projection.rs`) that converts `RunDetails` into a
  **render-ready projection**: ordered blocks with the established vocabulary (Todos, Run,
  Worker/Shell/Merge/Repair, Warn, Economics, Incidents, Activity, Tail), each block a label
  plus lines of `{text, role}` where `role` is a small closed set (`heading`, `info`, `dim`,
  `success`, `warning`, `error`, `attention`). Top-level fields: `feed_version: 1`,
  `summary_line`, `attention` (empty until PI-04 populates it with pending questions).
- Projection is computed daemon-side and returned additively in the `status` IPC response and
  `khazad-doom status --json` output, so CLI monitor/watch/status receive it through existing
  transports. Polling model unchanged.
- CLI monitor (`monitor`, `watch`, `status` human output) becomes a painter: ANSI-colorize
  roles, print blocks. No payload interpretation remains in the painter.
- Producer-side compatibility rules mirror D4 from the consumer side: painters must render
  unknown roles as `info` and unknown blocks as plain text — never crash — so daemon and
  future read-only adapters can be upgraded independently.
- Behavior-preserving: current monitor wording/content is the spec. Divergences must be listed
  in this workpackage before implementation (expected: none beyond incidental whitespace).

## Out of scope

- Rich Pi monitor overlay, auto-discovery, multi-run UI, or adapter-owned workflow state.
- Vocabulary changes or new blocks (additive later, through the projection module only).
- Streaming/push transport (deferred at matrix level).
- Projection for offline artifact `inspect` paths (deferred; live `status` only).
- Removing the CLI monitor.

## Data model changes

None to SQLite or slice schema. Additive `feed` object in `status` responses.

## API changes

- IPC `status` response and CLI `status --json` gain the additive `feed` field (documented
  shape + `feed_version` in `docs/workflow-invariants.md`).
- Existing raw fields (`run`, `slices`, `progress`, `events`, `economics`, `incidents`) remain
  unchanged for any external consumer.

## UI states (CLI/monitor output)

- **Active run:** identical content to today on all CLI surfaces (parity-tested).
- **Waiting / no runs:** projected waiting block; CLI surfaces render it.
- **Terminal run:** projected terminal summary, including PI-01's blocked-with-guidance wording
  — fix commands come through the projection so surfaces show them verbatim.
- **Daemon unreachable / exec failure:** the one state painters own locally (there is no
  projection to paint); surfaces keep their current actionable error hints. Documented as the
  only permitted painter-owned wording.
- **Unknown role/block from a newer daemon:** rendered as plain `info` text, never a crash.

## Migration / backward compatibility

Additive field; raw fields keep working for any external consumer during the transition. Golden/parity tests pin the CLI output before the refactor and assert equivalence after.

## Permissions

Not applicable (read-only view over existing status data).

## Test plan

Unit:
- Fixture snapshots: recorded `RunDetails` (running, blocked-with-guidance, terminal, waiting,
  incident-bearing, economics-bearing) → projection JSON snapshots.
- Role/block closed-set validation; `feed_version` present.
- Painter tolerance: unknown role and unknown block render as plain text in Rust painter tests.

Integration:
- CLI parity: human `monitor` output on recorded runs is equivalent pre/post refactor.
- `status --json` contains `feed` and raw fields simultaneously.
- Grep checks (in a test): `src/cli.rs` painter functions contain no payload-key interpretation.

### Workflow acceptance test

```text
1. Operator opens `khazad-doom monitor --latest`, `khazad-doom watch --run <run-id>`, and
   `khazad-doom status --run <run-id>` against the same active run.
2. All CLI surfaces show the same blocks with the same wording (formatting may differ by surface).
3. A new incident kind is introduced via a fixture (simulating a future slice's addition);
   only the projection module is edited; all surfaces show the new incident line without
   any painter change.
4. Edge condition: the daemon returns a projection containing an unknown role and an unknown
   block label (future version); painters render them as plain text and continue polling —
   no crash, no blank screen.
5. Invariant: at no point do the CLI surfaces disagree on wording for the same run state, and
   no interpretation logic exists outside src/workflow/projection.rs (grep checks pass).
```

## Acceptance criteria

1. One daemon-side module owns all feed interpretation; painters contain none (grep-verified).
2. `status` IPC/CLI responses carry the versioned projection additively.
3. CLI monitor output is behavior-preserving on the recorded-run corpus.
4. Unknown roles/blocks degrade to plain text in painters.
5. Projection shape and version rules documented in `docs/workflow-invariants.md`.

## Open questions (block `ready`)

1. Include `feed` in every `status` response or behind a `--feed`/param opt-in? (Recommend
   always-on: payload is bounded by `events_limit`; one fewer mode to test. Confirm size on a
   large recorded run.)
2. Parity standard for CLI output: byte-identical golden files vs semantic equivalence
   (recommend semantic — assert block/line content, tolerate whitespace).
3. Does the `watch` single-shot path share the same painter as the `monitor` loop today, or
   does it need its own thin adapter? (Read `monitor_once_result` before finalizing scope.)

## Definition of Done

- [ ] Data model changes — explicitly not needed (additive JSON only).
- [ ] IPC/CLI contract for `feed` documented with version rules.
- [ ] All named output states implemented: active, waiting, terminal, unreachable
      (painter-owned), unknown-role/block tolerance.
- [ ] Permissions — not applicable.
- [ ] Backward compatibility: raw fields untouched; parity corpus passes.
- [ ] Unit tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: projection shape in `docs/workflow-invariants.md`.
- [ ] Invariants checked: single interpretation layer (grep), no wording divergence between
      CLI surfaces, painters never crash on forward-version projections.
