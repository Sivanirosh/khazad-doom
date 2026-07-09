# AF-06 — Envelope-delegated auto-accept: `promote` and `run` levels

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-06. Status: `planned`
(blocked by: AF-00 evidence bar met with AF-04 shadow data; AF-05; RW-2 notifications).

## Scope

The only slice that grants the daemon decision authority — and only this much: for a
proposal classified **Tier 1** by AF-03, within budget, on a run whose envelope says
`promote` or `run`, the daemon may record an accept decision itself:

```json
"operator_decision": {
  "decision": "accepted",
  "authorizer": "envelope:kd-<run-id>",
  "source": "frontier_policy",
  "rationale": "<tier reason codes>",
  "applied": true
}
```

Application is byte-for-byte the AF-05 engine — auto-accept adds no second apply path.

- `promote`: generated slices are created, committed, and listed, but **not** executed;
  the run completes with them queued for a future run/operator start.
- `run`: generated slices are appended to the frontier and executed serially; the loop
  is: run next → verify/merge/close → ingest discoveries → classify → auto-accept Tier 1
  within budget → append → continue. All of that already exists as manager checkpoints;
  this slice only closes the loop at the "decide" step.
- Stop rules (from AF-00): Tier-3 pending → `awaiting_replan` + RW-2 origin
  notification; budget/depth/generated exhausted → structured pending-attention with
  reason (`frontier_budget_exhausted` etc.) and the exact commands to raise or finish;
  no frontier → normal completion; cancel → existing cancel semantics.
- Every auto-decision consumes `max_auto_promotions` budget durably *before* apply
  (crash between consume and apply resumes per AF-05 semantics, never double-consumes).
- Auto-accept is structurally impossible for: any non-Tier-1 classification, any change
  kind other than `add_followup_slice`, any draft matching a rejected/deferred proposal
  (Tier 3 by AF-03 ordering), any run without an envelope.

## Out of scope

Parallel autonomous frontier (serial only; explicitly deferred). Auto-reject/auto-defer
(Tier 2/3 always wait for a human). Envelope changes mid-run. Raising authority for
repair/verify/policy changes (their auto-tier remains empty).

## Data model changes

Decision `authorizer`/`source` vocabulary extended; budget counters consumed durably.
`ReplanStatus.auto_approvable` finally gets populated (Tier-1-classified pending
proposals on promote/run-level runs) instead of being permanently empty.

## API changes

None new; `decideReplanProposal` remains operator-only over IPC — auto-decisions are
internal manager actions recorded through the same store, so audit surfaces are shared.

## UI states

- Auto-accepted proposal (authorizer + reason codes visible), Tier-3 frontier pause
  (attention item + notification), budget-exhausted stop (structured reason + commands),
  promote-level completion ("N slices generated, not run"), operator-override window:
  status shows Tier-1 pending items briefly before the checkpoint applies them, with a
  cancel command that always works.

## Migration / backward compatibility

`off`/`shadow` runs are untouched. All prior decisions remain operator-authorized
records; the authorizer vocabulary is additive.

## Permissions

Delegated, bounded, revocable: authority exists only inside one run, only within the
envelope and budget, only for Tier 1, and `cancel` immediately stops the frontier.
Workers still never self-authorize; the daemon's authority comes from the operator's
recorded envelope, and every use of it is attributable.

## Test plan

E2e (fake runner): T1 auto-promoted and executed (run level); T1 created-not-run
(promote level); T3 pauses with notification-evidence event; budget 1 then exhaustion
stop; depth-1 chain (generated slice proposes another candidate → classified but stops
at depth). Negative: rejected-draft resubmission auto-accept impossible; envelope-less
run never auto-accepts. Crash: kill between budget-consume and apply → resume without
double-spend. Real-Pi gated smoke: one auto-promotion end-to-end.

### Workflow acceptance test

```text
1. Operator starts a run (autonomy run, max_auto_promotions 1, max_depth 1); S-1's
   worker emits C-A (Tier 1) and C-B (Tier 1, but budget will be spent).
2. At the checkpoint the daemon auto-accepts C-A (authorizer envelope:<run>, reason
   codes recorded), applies via the AF-05 engine, and runs it to closure.
3. C-B classifies Tier 1 but the budget is exhausted → frontier stops with
   frontier_budget_exhausted, C-B stays a pending proposal, origin notification fires.
4. Edge condition: the operator answers by accepting C-B manually — operator authority
   is not budget-bound; the run continues and completes.
5. Handoff: promotion graph shows C-A auto-accepted/applied, C-B operator-accepted;
   budget 1/1 consumed; both generated slices carry provenance generation=1.
6. Invariants: every applied change has a decision record with an attributable
   authorizer; no auto-decision exists for any non-Tier-1 proposal; cancel issued
   during step 2 in a rerun of this test stops before C-A's worker launches.
```

## Definition of done

- [ ] Auto-decision recording + budget consumption durable and crash-safe.
- [ ] API: no new mutation surface; audit parity between auto and operator decisions.
- [ ] All five UI states implemented via shared projection; notification path proven.
- [ ] Migration: off/shadow behavior byte-identical (regression fixture from AF-04).
- [ ] Full e2e matrix above green; real-Pi gated smoke recorded.
- [ ] Workflow acceptance test passes.
- [ ] Docs: RFC authority section marked implemented; evidence-bar citation with the
      actual AF-04 shadow numbers embedded in the slice close record.
- [ ] Invariants: AD1 (one channel), AD6 (fail toward asking), replan-RFC operator-
      approval list untouched for all other change kinds.

## Open questions

- Should `run`-level auto-apply wait one render cycle so the operator override window
  in the UI is real, or apply immediately at the checkpoint? Recommendation: immediate
  (the envelope was the consent); the cancel path is the override.
- Notification transport when no origin target exists (headless run): recommendation —
  rely on status/attend as today; do not block AF-06 on new transports, but record the
  stop prominently in run-summary.
