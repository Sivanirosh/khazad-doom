# AF-03 — Deterministic promotion policy (pure classifier)

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-03. Status: `planned`
(after AF-01 + AF-02: needs both type families).

## Scope

One new module, `src/workflow/frontier.rs`, exporting a pure function:

```rust
pub fn classify(
    envelope: &MissionEnvelope,
    graph: &SliceGraphView,      // open/closed slices + pending/decided proposals
    candidate: &FollowupSliceDraft,
    budget: &FrontierBudgetState,
) -> TierDecision              // { tier: Tier, reason_codes: Vec<ReasonCode> }
```

No IPC, state, git, filesystem, or clock access. The module is a transcription of the
AF-00 RFC tier rules; every rule carries a stable `ReasonCode` string that flows into
proposals, events, and reports.

Rule set (evaluation order matters; first Tier-3/stop rule wins, AD6):

- budget/depth/generated-slices exhausted → `Stop(budget_exhausted|depth_exhausted|…)`
- duplicate of a rejected/deferred proposal → Tier 3 (`duplicate_of_rejected_proposal` —
  the replan RFC makes reapplying those operator-only)
- area outside `allowed_areas` (area-contract prefix containment) → Tier 3
- overlaps a `non_goal` → Tier 3
- matches an envelope `must_ask_if` reason code → Tier 3
- introduces a dependency edge to a slice outside the run's graph, or any new
  external/package dependency claim → Tier 3
- API/security/data-migration ambiguity markers on the draft → Tier 3
- duplicate of an open/closed slice (id or goal-equivalent) → Tier 2 (`duplicate_open`) /
  reject-recommend (`duplicate_closed` — closed-slice reruns are the F-004/F-009 wound)
- no verify command and no `verify_profile` → Tier 2 (`unverifiable`)
- acceptance not testable (empty/prose-only heuristics defined in the RFC) → Tier 2
- everything above clean → Tier 1 (`inside_envelope`)
- Tier 0 exists only as recognition: work already implemented inside the current fence is
  attested by existing invariants; the classifier never assigns Tier 0 to a *draft*.

## Out of scope

Wiring into the manager (AF-04), any decision recording (AF-06), LLM/critic input
(rejected as authorizer at epic level), goal-similarity embeddings (duplicate detection
is id + normalized-goal string match in v1).

## Data model changes

`Tier`, `ReasonCode`, `TierDecision`, `SliceGraphView` (borrowed read-only view assembled
by the caller). No persisted-state changes.

## API / UI / migration / permissions

None — pure library code, unreachable from runtime paths in this slice.

## Test plan

Exhaustive table test mirroring the RFC scenario table (the ten matrix scenarios plus
every reason code at least once); property test: arbitrary drafts never panic and always
yield ≥1 reason code; ordering test: a draft that is both out-of-area and unverifiable
reports Tier 3, with both codes listed.

### Workflow acceptance test

```text
1. A reviewer takes the AF-00 RFC scenario table and, independently of the code,
   writes expected (tier, reason_codes) for all rows.
2. The table test encodes the same rows; both are compared — 100% match required.
3. Edge condition: candidate area "src/foo" (no trailing slash) vs. envelope area
   "src/foo/" — prefix containment must follow the area contract exactly; the case is
   in the table with a documented outcome.
4. A deliberately contradictory fixture (inside envelope AND duplicate of a rejected
   proposal) classifies Tier 3, proving Tier-3 precedence.
5. Invariant: classify() is referentially transparent — the table test runs twice in
   one process with identical outputs, and the module has no unsafe/global state.
```

## Definition of done

- [ ] Module lands with zero runtime callers (dead-code-allowed until AF-04).
- [ ] Data model: tier/reason types documented in the RFC.
- [ ] API/UI/migration/permissions: explicitly not needed.
- [ ] Table + property + ordering tests pass; every ReasonCode covered.
- [ ] Workflow acceptance test (RFC↔code table match) recorded.
- [ ] Docs: RFC scenario table cross-references test names.
- [ ] Invariants: purity (no I/O imports), AD6 upward-resolution proven by the
      contradictory fixture.

## Open questions

- Goal-equivalence for duplicate detection: exact normalized string vs. token overlap
  threshold. Recommendation: normalized-exact in v1; anything fuzzier is Tier 2 with
  `possible_duplicate` so a human looks.
- Where `SliceGraphView` is assembled (manager vs. read_model). Recommendation:
  `read_model`, since it already aggregates slices + proposals for projection.
