# PI-00 — Pi-first doctrine

Matrix row: [00-matrix.md](00-matrix.md) → PI-00. Status: `ready`.

## Scope

Record the Pi-first commitment as durable project doctrine so no future design discussion re-litigates it:

- Rewrite the vision statement: daemon state is harness-neutral JSON; worker execution is Pi-native by design; `fake` exists for deterministic testing, not portability.
- Add invariants D1–D5 (see matrix) to `docs/workflow-invariants.md`, including the coupling rule: *Khazad couples only to Pi's versioned, documented surfaces; a Pi behavior change may degrade observability but never correctness, because daemon state stays authoritative.*
- Record the three standing rejections with rationale: Pi acceptance gates, `fallbackModels` silent failover, auto-login.
- Update `README.md` positioning: project-level workflow daemon for Pi, complementary to session-scoped delegation tools such as pi-subagents (do not compete on session orchestration).

## Out of scope

- Any code change. The `Runner` trait, `RunnerSpec`, and CLI `--agent` flag are untouched (PI-03 touches profile assembly).
- Removing the `fake` runner (kept permanently as test seam).

## Data model changes

None.

## API changes

None. Docs only.

## UI states

Not applicable (documentation slice). Verify that no CLI help text or skill prompt (`skills/khazad-doom`) still advertises harness-agnostic worker execution; fix wording if found.

## Migration / backward compatibility

None. Existing `KHAZAD_AGENT=fake` workflows remain valid and documented as the test path.

## Permissions

Not applicable.

## Test plan

- Doc review against the five decisions in the matrix.
- `grep -ri "harness-agnostic\|harness agnostic" --include="*.md" --include="*.rs"` returns no claim that worker execution is harness-agnostic (harness-neutral *state* claims are fine and intended).

### Workflow acceptance test

```text
1. A new contributor (or agent session) reads .pi/memory/VISION.md and docs/workflow-invariants.md.
2. They can answer, without reading code: "may I add a second worker harness?" (no — removed,
   revisit condition documented) and "may workers use fallback models?" (no — rejected, D5).
3. Edge condition: a future slice proposes enabling Pi acceptance gates; the invariants doc
   names that exact rejection so the proposal is closed by citation, not re-argued.
4. Invariant: VISION.md, workflow-invariants.md, and README.md give the same answer to all
   three questions — no contradictory doctrine across documents.
```

## Acceptance criteria

1. VISION.md states Pi-native worker execution + harness-neutral daemon state + fake-as-test-seam.
2. `docs/workflow-invariants.md` contains D1–D5 and the three rejections with revisit conditions.
3. README positioning updated.
4. No doc or prompt claims multi-harness worker support.

## Open questions

None. Decision was made and reviewed on 2026-07-04.

## Definition of Done

- [ ] Data model changes applied — explicitly not needed.
- [ ] API changes — explicitly not needed.
- [ ] All named doc surfaces updated (VISION, PLAN, invariants, README, skill wording check).
- [ ] Permissions — not applicable.
- [ ] Migration behavior — not applicable.
- [ ] Doc-review test performed.
- [ ] Workflow acceptance test passes.
- [ ] Invariants checked: no contradictory doctrine between documents.
