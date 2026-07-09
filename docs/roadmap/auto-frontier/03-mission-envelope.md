# AF-02 — Mission envelope record + budgets in run state

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-02. Status: `planned`
(blocked by RW-1 and AF-00 acceptance).

## Scope

A daemon-owned, durable, rendered MissionEnvelope on the run — pure record, zero
authority. This implements the minimal runtime mission object whose Phase-2 deferral
AF-00 reopens: it exists only to bound future auto-acceptance, not to describe work.

- `MissionEnvelope { goal, allowed_areas, non_goals, verify_profile,
  max_auto_promotions, max_depth, max_generated_slices, autonomy_level, must_ask_if }`
  in `src/domain.rs`; stored with the run in `src/state.rs`.
- Budget counters (`auto_promotions_used`, `generated_slices`, `max_generation_reached`)
  live in durable run state next to the envelope, restart/resume-safe.
- CLI: `khazad-doom run --envelope <file.json>` (or inline flags for goal/areas/level);
  `--autonomy off|shadow|promote|run` with default `off`. Validation at run start:
  allowed_areas pass the area contract; budgets are non-negative; unknown fields warn.
- Rendered in `status`/`watch`/`monitor` (one envelope block from the shared projection)
  and embedded in run-summary, final report, and handoff JSON.

## Out of scope

Any behavior keyed off the envelope (`shadow` gains meaning only in AF-04, `promote`/`run`
only in AF-06 — until then all levels behave as `off` and status labels them "recorded,
not yet active"). Cross-run envelopes. Envelope mutation mid-run (operator cancels and
re-runs, or a future RFC adds an operator-only revision path).

## Data model changes

New `MissionEnvelope` + `FrontierBudgetState` structs; `Run` gains optional envelope.
Absent envelope ≡ `autonomy_level: off` with empty bounds.

## API changes

Run-start IPC params carry the envelope; `RunDetails`/feed expose it read-only. No
mutation endpoint.

## UI states

- Envelope present: block with goal, areas, budgets, level (success).
- Envelope absent (legacy/old runs): surfaces render "no envelope (autonomy off)" (empty).
- Invalid envelope at start: run refuses to start with the exact validation error (error).
- Level > off before AF-04/AF-06 land: rendered with "recorded, not yet active" caveat.

## Migration / backward compatibility

Old persisted runs deserialize with `envelope: None`. Old CLI invocations unchanged.

## Permissions

Only the operator sets the envelope, at run start. Workers see the envelope (read-only,
injected into the prompt context so candidates can aim inside it) but cannot change it.

## Test plan

Unit: serde defaults, validation matrix (bad area, negative budget, unknown level),
restart fixture preserving envelope + counters. Projection snapshot for all four UI
states. E2e: run with envelope completes identically to a run without one (behavioral
no-op proven by comparing event streams modulo envelope-recorded events).

### Workflow acceptance test

```text
1. Operator starts a run with an envelope (allowed_areas ["src/foo/"], autonomy shadow,
   max_auto_promotions 2).
2. Status/watch/monitor all render the same envelope block; the worker prompt contains
   the envelope context section.
3. The run proceeds exactly as an envelope-less run (no classification yet); report and
   handoff embed the envelope snapshot with counters 0/2.
4. Edge condition: daemon restart mid-run; resume restores the envelope and counters
   from durable state, not from CLI flags.
5. Operator inspects handoff JSON: envelope present, budgets untouched.
6. Invariant: event stream (minus envelope_recorded) is identical to the same mission
   run without an envelope — the record grants and changes nothing.
```

## Definition of done

- [ ] Data model + durable state applied; restart fixture green.
- [ ] Run-start API documented; no mutation path exists.
- [ ] All four UI states implemented via the shared projection.
- [ ] Backward compatibility: legacy run state deserializes; old CLI unchanged.
- [ ] Unit tests + e2e no-op equivalence pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs: RFC envelope section marked implemented; CLI help text.
- [ ] Invariants: envelope is not a truth store (AD4); no authority at any level yet.

## Open questions

- Envelope file format: dedicated JSON schema under `.workflow/schema/`? Recommendation:
  yes, `mission-envelope.schema.json`, validated by `khazad-doom slices validate --repo .`
  companion or a new `envelope validate` subcommand.
- Should `verify_profile` on the envelope override candidate drafts lacking verify?
  Recommendation: no override — a draft without verify is Tier 2 by policy; the envelope
  value is the default the *worker prompt* suggests for drafts.
