# PI-05 — Render-ready status projection

Matrix row: [00-matrix.md](00-matrix.md) → PI-05. Status: `planned` (minor open questions below).
Depends on: PI-00. Independent of PI-01..PI-03. Should land before PI-04 reaches `done`.

## Problem being removed

Today two full renderers interpret the same status JSON independently: ~800–1000 lines of Rust
in `src/cli.rs` (`render_todos`, `render_activity`, `render_economics`, `activity_line`, …) and
~1,350 lines of JS in `extensions/khazad-monitor/index.js` that re-implements the same
interpretation (its own `statusRole`, feed blocks, economics formatting). Every new event kind,
incident type, or phase must be interpreted twice, in two languages. The activity-feed
vocabulary was already aligned by hand once; this slice makes that alignment structural.

## Scope

- New module (proposed `src/workflow/projection.rs`) that converts `RunDetails` into a
  **render-ready projection**: ordered blocks with the established vocabulary (Todos, Run,
  Worker/Shell/Merge/Repair, Warn, Economics, Incidents, Activity, Tail), each block a label
  plus lines of `{text, role}` where `role` is a small closed set (`heading`, `info`, `dim`,
  `success`, `warning`, `error`, `attention`). Top-level fields: `feed_version: 1`,
  `summary_line`, `attention` (empty until PI-04 populates it with pending questions).
- Projection is computed daemon-side and returned additively in the `status` IPC response and
  `khazad-doom status --json` output, so both consumers (CLI monitor loop and the Pi extension,
  which execs the CLI) receive it through existing transports. Polling model unchanged.
- CLI monitor (`monitor`, `watch`, `status` human output) becomes a painter: ANSI-colorize
  roles, print blocks. No payload interpretation remains in the painter.
- `extensions/khazad-monitor/index.js` becomes a painter: theme-colorize roles, render blocks
  in the overlay. All event-type and payload knowledge deleted from JS.
- Producer-side compatibility rules mirror D4 from the consumer side: painters must render
  unknown roles as `info` and unknown blocks as plain text — never crash — so daemon and
  extension can be upgraded independently despite shipping in one repo.
- Behavior-preserving: current monitor wording/content is the spec. Divergences must be listed
  in this workpackage before implementation (expected: none beyond incidental whitespace).

## Out of scope

- New UX features (widget, notifications — PI-06).
- Vocabulary changes or new blocks (additive later, through the projection module only).
- Streaming/push transport (deferred at matrix level).
- Projection for offline artifact `inspect` paths (deferred; live `status` only).
- Removing `/khazad-monitor` or the CLI monitor.

## Data model changes

None to SQLite or slice schema. Additive `feed` object in `status` responses.

## API changes

- IPC `status` response and CLI `status --json` gain the additive `feed` field (documented
  shape + `feed_version` in `docs/workflow-invariants.md`).
- Existing raw fields (`run`, `slices`, `progress`, `events`, `economics`, `incidents`) remain
  unchanged for any external consumer.

## UI states (CLI/monitor output)

- **Active run:** identical content to today on both surfaces (parity-tested).
- **Waiting / no runs:** projected waiting block; both surfaces render it.
- **Terminal run:** projected terminal summary, including PI-01's blocked-with-guidance wording
  — fix commands come through the projection so both surfaces show them verbatim.
- **Daemon unreachable / exec failure:** the one state painters own locally (there is no
  projection to paint); each surface keeps its current error hint (`KHAZAD_DOOM_BIN` guidance
  in the extension). Documented as the only permitted painter-owned wording.
- **Unknown role/block from a newer daemon:** rendered as plain `info` text, never a crash.

## Migration / backward compatibility

Additive field; old extension builds that still interpret raw fields keep working during the
transition. Extension and CLI painters switch in the same change since both live in this repo.
Golden/parity tests pin the CLI output before the refactor and assert equivalence after.

## Permissions

Not applicable (read-only view over existing status data).

## Test plan

Unit:
- Fixture snapshots: recorded `RunDetails` (running, blocked-with-guidance, terminal, waiting,
  incident-bearing, economics-bearing) → projection JSON snapshots.
- Role/block closed-set validation; `feed_version` present.
- Painter tolerance: unknown role and unknown block render as plain text (Rust painter unit
  test; JS painter test with a fixture projection).

Integration:
- CLI parity: human `monitor` output on recorded runs is equivalent pre/post refactor.
- `status --json` contains `feed` and raw fields simultaneously.
- Grep checks (in a test): `extensions/khazad-monitor/index.js` contains no daemon event-type
  strings; `src/cli.rs` painter functions contain no payload-key interpretation.

### Workflow acceptance test

```text
1. Operator opens `khazad-doom monitor --latest` in a terminal and `/khazad-monitor` in Pi
   against the same active run.
2. Both surfaces show the same blocks with the same wording (roles differ only in coloring).
3. A new incident kind is introduced via a fixture (simulating a future slice's addition);
   only the projection module is edited; both surfaces show the new incident line without
   any painter change.
4. Edge condition: the daemon returns a projection containing an unknown role and an unknown
   block label (future version); both painters render them as plain text and continue
   polling — no crash, no blank screen.
5. Invariant: at no point do the two surfaces disagree on wording for the same run state, and
   no interpretation logic exists outside src/workflow/projection.rs (grep checks pass).
```

## Acceptance criteria

1. One daemon-side module owns all feed interpretation; painters contain none (grep-verified).
2. `status` IPC/CLI responses carry the versioned projection additively.
3. CLI monitor output is behavior-preserving on the recorded-run corpus.
4. Pi overlay renders exclusively from the projection; its JS interpretation layer is deleted.
5. Unknown roles/blocks degrade to plain text on both painters.
6. Projection shape and version rules documented in `docs/workflow-invariants.md`.

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
      surfaces, painters never crash on forward-version projections.
