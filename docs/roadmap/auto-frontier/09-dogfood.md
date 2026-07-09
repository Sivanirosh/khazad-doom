# AF-08 — Dogfood proof: staged autonomy on a real mission

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-08. Status: `planned` (last).

## Scope

Prove the epic on Khazad-Doom itself with real Pi workers, in three attested stages on
one small real mission (pick a genuine two-part task where part 2 is naturally
discoverable from part 1 — e.g. a module change whose test-fixture follow-up is obvious;
seeding via the slice goal text is allowed, hard-coding the candidate JSON is not).

- **Stage A — shadow.** Run the mission at `shadow`. Required evidence: worker emitted a
  candidate; daemon classified it Tier 1; queue byte-identical to `off`; report shows
  would-have-promoted + agreement after the operator manually accepts.
- **Stage B — run.** Re-run the mission (fresh clone/branch) at `run`,
  max_auto_promotions=1. Required evidence: auto-accept decision with
  `authorizer: envelope:<run>`, generated slice committed before execution, executed,
  closed with `closed_by_run`, publication truth intact (close record + report at
  advertised SHA), promotion graph in handoff.
- **Stage C — Tier-3 stop.** Same mission with the envelope narrowed so the candidate
  falls outside `allowed_areas`. Required evidence: frontier pauses in
  `awaiting_replan`, origin notification observed (RW-2), operator decision recorded,
  run resumes/completes. This is also the standing regression for F-010-class silent
  stops.

All attestation comes from daemon state, artifacts, and commits — never prose claims.
Write the evidence doc under `docs/design/evidence/frontier-dogfood-<date>.md`.

## Out of scope

Depth>1 chains (attest only if Stage B naturally produces one), parallel frontier,
cross-repo missions (keep it on khazad-doom; cross-repo evidence rules stay local-only).

## Data model / API / UI / migration / permissions

None — this slice changes no product code. Any defect found opens a follow-up slice
(possibly via the frontier itself, which would be its own best evidence).

## Test plan

The three staged runs are the test. Additionally: re-run Stage B's mission once more to
confirm idempotent behavior when the follow-up already exists closed (candidate must
classify `duplicate_closed`, not re-run — the F-004/F-009 regression guard, live).

### Workflow acceptance test

```text
1. Operator executes Stage A; verifies queue-identity and classification evidence.
2. Operator executes Stage B; verifies auto-promotion, execution, publication truth.
3. Operator executes Stage C; the Tier-3 stop notification arrives without watching
   status; operator rejects the out-of-envelope candidate with rationale.
4. Edge condition (Stage B rerun): the same mission runs again after closure; the
   rediscovered candidate classifies duplicate_closed and is NOT promoted or run;
   no closed-slice worker launches.
5. Invariants: across all runs, every .workflow/slices/ mutation has an accepted
   decision record; budget counters in every handoff match the observed promotions;
   attempts/economics attribute generated-slice work to the correct run.
```

## Definition of done

- [ ] All three stages executed with daemon-state evidence; evidence doc committed.
- [ ] Stage-B rerun duplicate guard observed live.
- [ ] Data model/API/UI/migration: explicitly not needed.
- [ ] Matrix rows AF-00..AF-08 statuses reconciled; `scripts/roadmap-truth-check` green.
- [ ] Docs: evidence doc linked from the RFC done-when section and failure ledger if
      any new failure class was observed.
- [ ] Invariants: all epic-level invariants (AD1–AD6) cited against observed evidence.

## Open questions

- Which real mission to use (decide at `ready` time; must be genuinely useful work, not
  a synthetic toy — dogfood value doubles as product progress).
