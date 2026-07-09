# AF-05 — Apply engine for accepted `add_followup_slice` (operator-authorized)

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-05. Status: `planned`
(after AF-01; independent of AF-03/AF-04 — parallelizable with them).

## Scope

Complete the apply path that RPL-01 deliberately deferred, for exactly one change kind:
an **operator-accepted** proposal carrying a typed `FollowupSliceDraft` becomes a real,
runnable slice inside the active run. This is the risky machinery (queue mutation,
git commits, resume semantics) shipped under full operator authority — before any
autonomy exists. It is valuable standalone: operators get mid-run follow-ups today.

On `replan accept` of such a proposal, at the next explicit checkpoint the daemon:

1. Re-validates the draft against the *current* graph (a sibling may have taken the id
   or closed equivalent work since proposal time; failure → decision recorded but
   `applied=false` with `apply_refused` reason, proposal surfaces for supersession).
2. Writes `.workflow/slices/<id>.json` with `provenance` in the integration worktree.
3. Commits it (`khazad(slice:<id>): promote follow-up from <parent> via <proposal>`),
   **before any worker can run it** — a worker must never execute an uncommitted contract.
4. Appends the slice to the remaining queue after the current layer (serial append; no
   insertion into an active parallel layer).
5. Records `applied=true`, `applied_at`, checkpoint ids before/after, and before/after
   queue snapshot hashes on the decision (fields RPL-01 already reserved).

Crash safety per the replan RFC: accepted-but-not-applied on resume either re-applies
idempotently (keyed by proposal id + before-hash) or blocks with
`replan_apply_incomplete`; never launches workers against an ambiguous queue.

## Out of scope

Auto-accept (AF-06). Applying any other change kind (areas/verify/policy/dependency
edits stay unapplied, `applied=false`, as today). Running generated slices in parallel
layers with hand-written siblings (serial append only).

## Data model changes

None beyond fields RPL-01 reserved (`applied`, `applied_at`, checkpoint/snapshot fields
on the decision). Queue-extension event `frontier_slice_promoted` with provenance.

## API changes

`decideReplanProposal` semantics extend: accept on an applyable kind triggers apply at
the next checkpoint. New read-only field on RunDetails: generated-slice list.

## UI states

- Accepted-pending-apply ("accepted, applying at next checkpoint"), applied (revision
  summary + new queue), apply-refused (validation error + supersede guidance),
  apply-incomplete after crash (blocked with `replan_apply_incomplete` + resume command).

## Migration / backward compatibility

Proposals accepted before this slice remain `applied=false` historical records; they are
not retro-applied. Runs without accepted drafts behave identically.

## Permissions

Operator only. The daemon applies; workers and repair workers still cannot touch
`.workflow/slices/` (path guards unchanged). The generated slice's own execution is
bounded by its `areas` exactly like a hand-written slice.

## Test plan

Unit: idempotent re-apply (same proposal applied twice → one slice file, one commit);
re-validation failure paths (id collision, closed-equivalent, cycle). Crash fixtures:
kill between accept and apply → resume re-applies; kill between slice-commit and
queue-extension → resume completes without duplicate commit. E2e (fake runner): operator
accepts mid-run → generated slice runs, closes with `closed_by_run`, publication commit
carries its close record at the advertised SHA.

### Workflow acceptance test

```text
1. Operator runs mission with S-1; S-1's worker proposes C-A; the run pauses at the
   post-integration checkpoint awaiting the pending proposal? No — run would complete
   with pending proposal blocking handoff; operator accepts C-A while the run is at
   the checkpoint.
2. Daemon re-validates C-A, writes and commits the slice JSON with provenance in the
   integration worktree, appends it to the queue, records applied=true with snapshots.
3. C-A's worker runs in a fresh worktree, completes, integrates, and the slice closes
   with closed_by_run.
4. Edge condition: daemon is killed after the slice commit but before the queue
   snapshot is recorded. Resume detects the half-applied state via proposal id +
   before-hash, completes the queue extension idempotently, and does not create a
   second slice file or commit.
5. Final publication: one completion commit; C-A's close record and report exist at
   the advertised final SHA; handoff plan_revisions shows the accepted+applied record.
6. Invariants: no worker ever ran C-A before its slice JSON was committed; queue
   before/after hashes match the recorded snapshots; attempt/economics counters count
   C-A like any slice.
```

## Definition of done

- [ ] Apply engine + reserved decision fields populated; events recorded.
- [ ] API semantics documented (RFC apply section supersedes RPL-01's deferral note).
- [ ] All four UI states implemented via shared projection.
- [ ] Migration: pre-existing accepted-unapplied proposals untouched (fixture).
- [ ] Unit + crash + e2e tests pass, including publication-idempotency rerun.
- [ ] Workflow acceptance test passes on the fake runner.
- [ ] Docs: replan RFC implementation note updated; CLI help for accept flow.
- [ ] Invariants: commit-before-run, serial integration, publication atomicity,
      no silent .workflow/slices/ edits (every write has a decision record).

## Open questions

- Apply timing when no checkpoint is imminent (run already past its last layer):
  recommendation — completed-run accepts stay `applied=false` with guidance to start a
  new run naming the draft; only active runs apply.
- Budget interaction: operator-accepted promotions do NOT consume `max_auto_promotions`
  (that budget bounds daemon authority, not operator authority) but DO count toward
  `max_generated_slices` and depth. Confirm in AF-00.
