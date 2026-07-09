# AF-00 — Frontier doctrine RFC + proposed invariant amendments

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-00. Status: `ready`.

## Scope

Docs only. Produce `docs/design/frontier-autonomy.md` (RFC, same register as
`docs/design/replan-checkpoints.md`) and proposed amendment records appended to
`docs/workflow-invariants.md` in the Phase-2 amendment format (proposed invariant text,
ledger entries, enforcement mechanism, violation-detecting test, status).

The RFC must define, precisely enough that AF-03's classifier is a transcription:

1. **MissionEnvelope fields and validation** — goal, allowed_areas (area-contract literal
   prefixes), non_goals, verify_profile, max_auto_promotions, max_depth,
   max_generated_slices, autonomy_level (`off|shadow|promote|run`), must_ask_if.
2. **Tier semantics** — Tier 0 attest-inline (already lawful under the in-fence learning
   invariant; classifier only recognizes and attests it), Tier 1 auto-promote, Tier 2
   queue-pending (today's default), Tier 3 ask-operator. Every tier rule gets a stable
   machine-readable reason code.
3. **Authority ladder and evidence bar** — the numeric gate for turning on `promote`/`run`,
   e.g.: over ≥ N shadow-classified Tier-1 candidates across ≥ M distinct runs, operator
   accepted ≥ X% unchanged and no Tier-1 misclassification would have crossed the envelope.
   Pick and justify N/M/X in the RFC; AF-06 cites the measured values from AF-04.
4. **Stop rules** — budget exhausted, depth exhausted, Tier-3 pending, no frontier, cancel.
   Each maps to an existing structured pending-attention state or terminal reason kind; no
   new RunStatus/SliceStatus values.
5. **Provenance requirements** — generated slice JSON carries `provenance`
   (parent_slice_id, origin_proposal_id, generation, created_by, created_at); promotion
   decisions carry authorizer + reason codes; reports carry the graph.
6. **Deferral reopenings** — one section per reopened Phase-2 deferral (runtime mission
   object; automated planner queue authority; daemon-internal replan engine) quoting the
   recorded reconsider condition and the evidence that fires it. The ambient-planner
   rejection is explicitly NOT reopened (AD5).

## Out of scope

Any runtime behavior, schema, or code change (→ AF-01..AF-06). Cross-run envelopes
(explicitly deferred at epic level).

## Data model / API / UI / migration / permissions

None — docs only. The amendment records are marked `proposed` until the operator accepts
them; acceptance of this RFC is the gate for AF-02..AF-06.

## Test plan

- `scripts/roadmap-truth-check` passes with the new matrix.
- Doc review: every tier rule in the RFC has a reason code; every reopened deferral quotes
  its reconsider condition verbatim from `docs/workflow-invariants.md`.

### Workflow acceptance test

```text
1. Reviewer opens frontier-autonomy.md and, without reading any source code, fills in
   the expected classifier output (tier + reason codes) for the ten AF-03 test-table
   scenarios listed in the matrix row.
2. A second pass against AF-03's (later) test table must match 10/10.
3. Edge condition: a scenario the RFC does not cover unambiguously (e.g. candidate area
   equals an envelope area with different trailing-slash form) is discovered.
4. The RFC is amended before AF-03 starts; ambiguity resolves to the higher tier (AD6).
5. Invariant: the RFC never grants any actor apply authority — only decision-recording
   authority bounded by the envelope.
```

## Definition of done

- [ ] RFC exists with all six sections; amendment records appended in Phase-2 format.
- [ ] Data model / API / UI / migration: explicitly not needed (docs only).
- [ ] `scripts/roadmap-truth-check` and `git diff --check` pass.
- [ ] Workflow acceptance test above executed and recorded in the RFC's done-when section.
- [ ] Invariants checked: no runtime claims; single-authority-channel wording consistent
      with the replan RFC.

## Open questions (resolve before `in_progress` → `done`)

- Exact N/M/X for the evidence bar (proposal: N=10 candidates, M=3 runs, X=80%).
- Whether `must_ask_if` on the envelope is free-text (matching slice `must_ask_if`) or a
  closed enum; recommendation: free-text matched by the worker prompt, plus closed reason
  codes for the deterministic rules the daemon can check itself.
