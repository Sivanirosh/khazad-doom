# AF-04 — Shadow mode: classify, record, measure — never mutate

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-04. Status: `planned`
(after AF-03).

## Scope

Give `autonomy_level: shadow` its meaning: at the existing replan checkpoints (after
worker output ingestion / after a successful integration checkpoint), the daemon runs
AF-03's classifier over every pending `add_followup_slice` proposal in the run and
records the outcome — nothing else changes.

- Classification result (tier, reason codes, envelope snapshot hash, budget state at
  classification time) is stored on the proposal and emitted as a
  `frontier_classified` event.
- Projection/status render the shadow annotation on pending proposals
  ("shadow: would auto-promote (Tier 1: inside_envelope)").
- Report/handoff gain a `frontier` section: candidates seen, tier distribution,
  would-have-promoted list, and the **agreement metric** — for each Tier-1-classified
  proposal, whether the operator later accepted it unchanged, modified it, rejected,
  or deferred it. This is the evidence stream AF-00's ladder requires for AF-06.
- `off` level skips classification entirely (zero new work on the hot path, AD5/F-011).
- `promote`/`run` levels behave exactly like `shadow` in this slice, rendered with the
  "recorded, not yet active" caveat from AF-02.

## Out of scope

Queue mutation, slice creation, decision recording, notifications. Changing when replan
checkpoints occur (the RFC's trigger table is untouched — shadow classification piggybacks
on proposals that already exist).

## Data model changes

Proposal record gains optional `frontier_classification { tier, reason_codes[],
classified_at, envelope_hash, budget_snapshot }`. Re-classification on resume replaces it
(latest wins; history stays in events).

## API changes

`listReplanProposals` includes the classification. No new methods.

## UI states

- Shadow-classified pending proposal (success), unclassified pending proposal on an
  `off` run (empty — renders as today), classification impossible because the envelope
  is absent (error-annotated event, proposal stays unclassified), stale classification
  after resume (superseded by re-classification, event trail preserved).

## Migration / backward compatibility

Proposals without classification render as today. Runs recorded before AF-04 replay fine.

## Permissions

None granted. The classifier writes annotations, never decisions; operator decision
commands are byte-identical to before.

## Test plan

Fake-runner e2e: shadow run with two candidates → both classified, queue and
`.workflow/slices/` byte-identical to the same mission at `off`; event stream contains
`frontier_classified` with reason codes. Agreement-metric fixture: operator accepts one
Tier-1 and rejects another → report shows 50% agreement with per-proposal rows.
Projection snapshots for all UI states. Resume fixture: classification recomputed, event
history preserved.

### Workflow acceptance test

```text
1. Operator runs a two-slice mission at autonomy shadow; slice S-1's worker emits
   candidates C-in (inside envelope) and C-out (outside area).
2. At the post-S-1 checkpoint both proposals are classified: C-in Tier 1, C-out Tier 3;
   status shows the annotations plus unchanged accept/reject/defer commands.
3. The run continues to S-2 without pausing (shadow never blocks on Tier 3 —
   classification is advisory; the pending proposals block handoff readiness as today).
4. Edge condition: operator accepts C-in mid-run via the normal replan command; the
   decision records applied=false (AF-05 not yet landed); the agreement metric counts
   accepted-unchanged for a Tier-1 classification.
5. Report frontier section: 2 candidates, 1 would-have-promoted, agreement 1/1,
   budget hypothetically 1/2.
6. Invariants: diff of .workflow/slices/ across the run is empty; rerunning the same
   mission at autonomy off produces an identical queue and identical slice states.
```

## Definition of done

- [ ] Data model annotation applied; resume/re-classification semantics tested.
- [ ] API: classification exposed read-only; documented in RFC.
- [ ] All named UI states implemented via shared projection.
- [ ] Migration: legacy proposals/runs unaffected (fixture).
- [ ] Unit + e2e + snapshot tests pass; off-level zero-overhead check.
- [ ] Workflow acceptance test passes.
- [ ] Docs: RFC shadow section marked implemented; report format documented.
- [ ] Invariants: zero mutation proven by byte-identical-queue test; AD5 respected
      (no checkpoint added, no ambient pass).

## Open questions

- Should shadow classification also run retroactively over proposals from earlier runs
  to bootstrap the evidence bar faster? Recommendation: yes as a read-only
  `khazad-doom frontier replay --run <id>` debug command, but only real-run shadow data
  counts toward the AF-06 evidence bar.
