# AF-01 — Typed follow-up slice drafts through the replan channel

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-01. Status: `planned`
(blocked by RW-1: in-flight area-contract work must merge first).

## Scope

Workers can propose complete, validated follow-up slice drafts; the daemon converts each
into a durable `pending` replan proposal. No classification, no application, no autonomy —
this slice only makes the existing `add_followup_slice` change kind carry enough typed data
to ever be safely promotable.

- Extend `WORKER_RESULT_SCHEMA` (and `REPAIR_RESULT_SCHEMA`) with optional
  `candidate_followup_slices[]`.
- Extend the worker prompt contract (`src/workflow/prompts.rs`): when a discovery requires
  new intent/areas/dependencies, emit a candidate draft instead of prose; the existing
  finding + `disposition: "proposed"` rule now points at the created proposal id.
- Daemon (`src/workflow/manager.rs`): on valid worker output, validate each draft with the
  same rules as real slices (slice schema, area contract, id syntax, non-duplicate id,
  acyclic against the current graph) and create a replan proposal whose
  `proposed_changes[]` carries the full draft payload; link worker output and attempt as
  evidence. An invalid draft becomes a warning finding on the output — it must not fail the
  attempt or crash the run.
- Add `provenance` to `.workflow/slices/` slice schema (`slice.schema.json` +
  `src/artifact.rs` types): `parent_slice_id`, `origin_proposal_id`, `generation`,
  `created_by` (`operator|worker+daemon`), `created_at`. Optional; hand-written slices
  remain valid without it.

## Out of scope

Tier classification (AF-03), envelope (AF-02), applying drafts (AF-05), autonomy (AF-06).
Candidate drafts from daemon checks (only worker/repair outputs and the existing operator
`replan create` path produce candidates — AD5).

## Data model changes

- `ReplanProposedChange` gains an optional typed draft payload (new struct
  `FollowupSliceDraft`: id, title, goal, areas, acceptance, verify, verify_profile,
  depends_on, must_ask_if, rationale). Serialized additively; existing prose-only
  proposals keep working (kind/target/summary unchanged).
- Slice type gains optional `provenance` block.

## API changes

`createReplanProposal` IPC accepts the typed payload; `listReplanProposals` returns it.
No new IPC methods.

## UI states (status/feed/report surfaces)

- Pending proposal with a draft renders the draft id/title/goal/areas plus the existing
  exact decision commands (success state).
- Invalid-draft finding renders as a warning with the validation error (error state).
- No candidates → no change to today's rendering (empty state).

## Migration / backward compatibility

Old worker outputs without `candidate_followup_slices` validate unchanged. Old proposals
without draft payloads render as today. Old slice JSON without `provenance` validates.

## Permissions

Workers propose only (enforced already: worker output cannot mutate `.workflow/slices/`;
path guard + RPL-02 authority checks unchanged). Draft validation happens daemon-side.

## Test plan

Unit: draft validation matrix — glob area, out-of-contract area, empty acceptance,
duplicate id vs. open slice, duplicate id vs. closed slice, dependency cycle, bad id
syntax; each yields a warning finding and no proposal. Serde round-trip of the payload;
legacy proposal JSON decodes.

Fake-runner e2e: worker emits one valid + one invalid candidate → exactly one pending
proposal with evidence links; status/feed show it; run completes normally with the
proposal pending (handoff readiness blocked, as today).

### Workflow acceptance test

```text
1. Operator runs a single-slice mission; the (fake) worker output includes candidate
   C-good (valid draft) and C-bad (area "src/**" violating the area contract).
2. Daemon records one pending replan proposal for C-good with the full typed draft and
   worker-output evidence link; C-bad becomes a warning finding naming the exact
   validation error.
3. Operator inspects `status` and sees the draft summary plus accept/reject/defer
   commands; handoff readiness is blocked by the pending proposal.
4. Edge condition: daemon restarts before the operator decides; the run is interrupted
   but `status --include-terminal` still lists the pending proposal with its draft.
5. Operator rejects with rationale; handoff unblocks.
6. Invariants: .workflow/slices/ is byte-identical throughout; attempt history and
   economics count the attempt exactly once; the proposal id is never reusable.
```

## Definition of done

- [ ] Data model changes applied (typed payload, provenance) with serde tests.
- [ ] API contracts documented in the RFC's proposal-record section.
- [ ] All named UI states render from the shared projection (no painter-side wording).
- [ ] Backward compatibility verified against recorded legacy proposal/worker fixtures.
- [ ] Unit + e2e tests pass; `slices validate` accepts provenance-bearing slices.
- [ ] Workflow acceptance test passes on the fake runner.
- [ ] Docs updated: worker prompt contract, RFC proposal-record section.
- [ ] Invariants checked: no queue mutation anywhere in this slice; findings with
      candidates still require terminal disposition (RPL-02 unchanged).

## Open questions

- Draft id namespace: free choice by worker vs. daemon-suffixed (`<parent>-F1`)?
  Recommendation: worker proposes, daemon rejects collisions — rename happens via
  operator edit, not silent daemon rewriting.
- Should `deferred_fog[]` from the original plan exist as a field? Recommendation: no —
  existing `findings` + `assumptions` already carry it; add only if dogfood shows loss.
