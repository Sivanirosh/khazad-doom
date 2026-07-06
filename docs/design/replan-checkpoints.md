# Replan checkpoints RFC

Date: 2026-07-06  
Status: accepted for redesign planning; implementation is deferred to Phase 5 dogfooded slices after the Phase 4 architecture review.

This decision record defines the smallest replan mechanism justified by the failure ledger. It is intentionally not a broad adaptive-workflow doctrine document. The workflow invariants remain the normative law; this RFC prices the mechanism that will enforce the Phase 2 plan/queue-revision invariant.

## Evidence and constraints

Primary evidence:

- **F-008 — operators bailed out when queue/integration trust was low.** Operators cancelled or routed around runs to validate manually or because `--all` appeared to rerun already completed work.
- **F-004 — close-record/report promotion gaps.** The daemon/integration branch and main-branch slice truth diverged, causing closed-slice reruns and stale queue state.
- **F-009 — integration-gate failure after slice merges.** Per-slice `merged` state did not clearly communicate run-level acceptance/closure, feeding later reruns.
- **F-003 — worker/repair authority crossed fences.** Good repairs and bad policy mutations both lacked an explicit approval seam.
- **F-006 — overloaded `blocked`.** Operator-action blockers, wrong-queue blockers, and already-done blockers shared one terminal label.
- **F-011 — long runs can work.** The redesign must preserve successful 70–90 minute multi-slice throughput and avoid adding an always-on planning tax.

Constraints accepted in Phase 2:

- Slice JSON plus daemon/run state are the live source of workflow status.
- Plan and queue revisions are durable facts, not silent edits.
- Actionable findings need terminal disposition.
- Repair authority is bounded by existing authorization.
- Terminal blocked/failed states need structured primary-reason data.
- Automated planner authority, daemon-internal autonomous replanning, runtime mission objects, and auto-blocking complexity telemetry are explicitly deferred.

## Decision summary

Introduce a **replan checkpoint** as a daemon-owned pause point where an actionable finding can become a recorded, operator-authorized plan revision.

V1 is deliberately conservative:

1. Replan evaluation is **findings-triggered only**. No finding or proposed follow-up means no replan pass.
2. Workers, repair workers, daemon checks, and future planner agents may **propose** plan changes. They may not apply them.
3. No planner proposal is auto-applied in v1. The auto-approvable tier for queue mutation is intentionally empty until production evidence proves a safe mechanical class.
4. Operator approval is required before any queue/slice/verification change is applied.
5. The approval path must be cheap: status/watch/monitor show the exact proposal, evidence, and copy-pasteable accept/reject/defer command.
6. Accepted revisions are applied atomically by the daemon at explicit checkpoints, recorded as durable run artifacts/events, and attested in handoffs/reports.
7. Rejected and deferred proposals remain durable findings with rationales/revisit conditions; they are not silently dropped.

## Terms

- **Actionable finding:** a worker, repair worker, daemon check, or operator note that asks for intent or proposes changing scope, verification, queue, dependencies, or workflow policy.
- **Replan proposal:** a durable, unapplied proposal linked to one or more actionable findings and evidence artifacts.
- **Plan revision:** an accepted proposal applied by the daemon. It records the exact queue/slice/workflow change and the authorization that made it valid.
- **Replan checkpoint:** a run point where the daemon can pause before dispatching more work, applying repair, or resuming after a terminal blocked/failed state, so the operator can decide a pending proposal.
- **Intent-affecting change:** any change to goal/mission, areas, acceptance, verification, dependencies that alter required work, deletion/closure of work without completed-run evidence, or addition of new runnable work.

## Trigger model

A replan checkpoint is created only when there is a concrete trigger:

| Trigger | Creates proposal? | Notes |
|---|---:|---|
| Worker returns `ask-user`/blocked finding with proposed follow-up | Yes | Proposal references worker output and slice id. |
| `ask_operator` unavailable/timed out and worker falls back to blocked JSON | Yes, if the blocked output includes a proposed plan/queue/scope change | The question itself remains evidence. |
| Integration repair would need out-of-area or workflow-policy changes | Yes | This prices the R4 trade-off: good repairs outside authority become cheap approval requests, not silent mutations. |
| Daemon detects selected slices are already closed or queue state contradicts run evidence | Yes for non-trivial correction; otherwise existing closed-dependency skip remains normal selection behavior | Repairs F-004/F-009 without making docs the truth source. |
| Terminal blocked/failed primary reason is wrong-queue/partial-state/needs-follow-up | Yes | Proposal is visible on `status --include-terminal` and resume. |
| Operator explicitly asks to record a replan proposal | Yes | This captures manual trust bail-outs without requiring the operator to edit slice JSON silently. |
| No finding/proposed follow-up | No | Preserves F-011 throughput; no ambient planner pass. |
| Complexity telemetry alone | No | Explicitly deferred in Phase 2; telemetry may inform human review, not trigger replanning. |

## When the plan may change

The plan may change only at explicit checkpoints:

1. **Before dispatching new worker attempts.** The daemon may pause after a finding and before launching the next slice/layer.
2. **Before launching integration repair.** If repair would exceed authorization, it must produce a proposal and wait.
3. **At a terminal blocked/failed resume point.** A completed terminal run may expose proposals; applying one requires `resume` or an equivalent explicit continuation command.
4. **After a successful integration checkpoint, before advancing to the next dependency layer.** Only if a finding/proposal exists.

The plan must not change mid-worker, mid-command, or by silently editing `.workflow/slices/` while a run is active.

## Proposal and authorization authority

| Actor | May propose | May authorize/apply |
|---|---|---|
| Worker | Follow-up slice draft, narrowed scope question, blocked finding disposition, evidence-backed queue concern | No |
| Integration repair worker | Repair revision request, follow-up slice draft, explanation of why existing authorization is insufficient | No |
| Daemon | Mechanical contradiction proposal, e.g. run evidence says a slice was closed but source branch lacks the close record | No, except existing non-replan behavior such as skipping known closed dependencies |
| Future planner agent | Candidate queue/slice revisions with evidence and risk classification | No |
| Operator | Any proposal or disposition | Yes |

### What a planner may propose without changing intent

A planner may propose, but not apply:

- a follow-up slice draft that preserves the original product goal and declares new bounded scope;
- marking a finding as answered by existing evidence;
- deferring a finding with a revisit condition;
- rejecting a finding as duplicate or out-of-scope with rationale;
- narrowing the runnable queue to exclude already-closed or already-merged work when supported by run evidence;
- pausing repair and asking for an operator-approved revision when repair would cross the authorized fence.

### What always requires operator approval

Operator approval is mandatory for:

- expanded `areas`;
- changed acceptance criteria;
- changed verification commands or profiles;
- changed dependencies that alter required work;
- adding new runnable slices to the current run queue;
- deleting slices or closing slices without completed-run evidence;
- changed mission/goal/product intent;
- workflow policy/profile mutations;
- any repair outside the existing authorized slice set/areas;
- applying a previously rejected or deferred proposal.

## Proposal record

A replan proposal should be recorded as a daemon-owned artifact/event with a stable id. Shape is illustrative, not final schema:

```json
{
  "id": "rp-20260706-001",
  "run_id": "kd-...",
  "state": "pending",
  "source": { "kind": "repair", "slice_id": "S-1", "attempt": 1 },
  "trigger_finding_ids": ["finding-1"],
  "evidence": [
    { "kind": "worker_output", "path": ".workflow/runs/.../worker.json" },
    { "kind": "gate_result", "path": ".workflow/runs/.../gate.json" }
  ],
  "risk": "intent_affecting",
  "proposed_changes": [
    {
      "kind": "add_followup_slice",
      "slice_id": "S-1-followup",
      "rationale": "Repair requires files outside S-1 areas"
    }
  ],
  "operator_decision": null,
  "created_at": "2026-07-06T00:00:00Z",
  "updated_at": "2026-07-06T00:00:00Z"
}
```

Accepted revision records add:

- operator identity/source (`cli`, future TUI, or explicit daemon operator action);
- decision rationale;
- applied patch/diff summary;
- before/after queue snapshot hash;
- `applied_at`;
- checkpoint id before application;
- checkpoint id after application.

Rejected records add `rejected_by`, `rejected_at`, and rationale. Deferred records add `deferred_by`, `deferred_at`, and a required revisit condition.

## State model

Replan proposal states are intentionally not run statuses:

| State | Meaning | Run behavior |
|---|---|---|
| `pending` | Operator decision required | Active run pauses with progress phase `awaiting_replan`, or terminal run exposes proposal for resume. |
| `accepted` | Operator approved and daemon applied the revision atomically | Run may continue/resume from the post-apply checkpoint. |
| `rejected` | Operator chose not to apply | Queue remains unchanged; if original condition still blocks progress, run remains blocked/awaiting another disposition. |
| `deferred` | Operator chose not now, with revisit condition | Queue remains unchanged; run may continue only if the deferred finding is not required for current safety/intent. |
| `superseded` | A later proposal replaced this one | Status links to the active proposal/revision. |

No new terminal run status is introduced. `awaiting_replan` is a progress phase/pending-attention state, like `awaiting_operator`, not a new `RunStatus` or `SliceStatus`.

## Rejection and deferral semantics

Rejection is a terminal disposition for the proposal, not erasure. The daemon records the rationale and blocks reusing the same proposal id. If the operator rejects a proposal that was necessary to continue safely, the run remains blocked with primary reason `replan_rejected` or the original structured reason.

Deferral is also terminal for the current proposal. It must name a revisit condition such as "after Phase 4 architecture review" or "if this slice family repeats the same close-record failure". A deferred proposal may be copied into a new proposal only with a new id, citing either new evidence or the fired revisit condition.

## Resume and daemon restart behavior

Pending replan proposals are durable daemon state and part of run inspection.

- If the daemon restarts while a proposal is `pending`, existing recovery marks the active run `interrupted`; `status --include-terminal` and `inspect` still show the pending proposal.
- Answering or accepting a proposal against an interrupted/cancelled run is rejected with guidance to `resume` first, unless the future implementation proves an idempotent offline-apply path.
- `resume` re-enters the last checkpoint and restores `awaiting_replan` before dispatching workers or repair.
- If the daemon crashes after operator acceptance but before `applied_at`, resume either reapplies idempotently using the proposal id and before-hash, or marks the run blocked with `replan_apply_incomplete` and the evidence needed for repair. It must not silently launch workers against an unknown queue.
- If the daemon crashes after `applied_at` and the post-apply checkpoint is written, resume continues from that checkpoint.

## Status/watch/monitor rendering

All surfaces render the shared daemon feed projection:

- `pending`: "Awaiting replan decision" with proposal id, source slice/phase, one-line rationale, risk classification, and exact commands:
  - `khazad-doom replan accept <run> <proposal>`
  - `khazad-doom replan reject <run> <proposal> --reason ...`
  - `khazad-doom replan defer <run> <proposal> --until ... --reason ...`
- `accepted`: revision id, applied change summary, authorizer, and post-apply checkpoint.
- `rejected`: proposal id and rejection rationale.
- `deferred`: proposal id, rationale, and revisit condition.
- `superseded`: old proposal id and replacement id.

Terminal summaries, reports, and handoffs include a `plan_revisions` section with every proposal and decision that affected or attempted to affect the run.

## Handoff attestation

A handoff must attest queue history without becoming a second source of truth:

- selected slices and dependency closure were computed from slice JSON + daemon/run state;
- every accepted plan revision is listed with evidence, authorizer, and applied diff summary;
- rejected/deferred proposals are listed as non-applied findings/dispositions;
- any remaining pending proposal blocks handoff readiness unless explicitly marked non-blocking by the operator.

## Cost of bounded repair authority

The Phase 2 repair-authority invariant would have blocked R4's beneficial out-of-area repo-path normalization repair unless an operator approved a revision or follow-up. That is intentional: the same unbounded repair channel also allowed policy/scope risk in F-003.

The cost must be kept low by design:

- repair workers should emit a concise proposal artifact instead of a prose-only blocker;
- status must show the exact accept/reject/defer commands;
- accepting a follow-up or repair revision should be one operator action plus daemon validation;
- rejection/defer should preserve the proposed fix for later review instead of forcing rediscovery.

If approval friction repeatedly causes operators to cancel and hand-apply good repairs, that becomes new evidence to reconsider the empty auto-approve tier.

## Consequences

Positive:

- Converts operator trust bail-outs into durable queue-truth events.
- Keeps worker/repair/planner authority proposal-only until evidence justifies more.
- Makes plan changes inspectable, resumable, and attestable.
- Preserves long-run throughput by avoiding ambient planner passes.
- Gives Phase 5 a small mechanism target instead of a broad autonomous planning system.

Costs:

- Some useful repairs now wait for operator approval.
- Status/projection needs a new pending-attention rendering path.
- Final reports/handoffs need queue-history attestation.
- The daemon needs idempotent proposal application or safe interrupted-state handling.

## Rejected and deferred alternatives

| Alternative | Decision | Rationale | Reconsider condition |
|---|---|---|---|
| Always run a planner at every checkpoint | Rejected | No evidence that runs fail because no planner pass happened when no finding existed; F-011 says preserve throughput. | Repeated evidence of missed safe replans despite explicit findings being absent. |
| Auto-apply planner proposals | Explicitly deferred | Queue mutation changes operator intent; Phase 2 deferred planner authority. | Repeated accepted proposals are mechanical, low-risk, and human approval is proven bottleneck. |
| Runtime mission object | Explicitly deferred | Current evidence is about slice/queue truth, not missing mission representation. | Recorded slice revisions cannot express durable operator intent. |
| Treat complexity telemetry as a replan trigger | Explicitly deferred | Phase 2 found no ledger entry that advisory deltas would have caught. | Phase 4 or later ledger evidence shows repeated complexity regressions missed by manual review. |
| Let repair mutate workflow policy and open follow-ups automatically | Rejected | Violates F-003 lesson and Phase 2 repair-authority invariant. | Never for policy mutation; only reconsider narrow auto-approve after evidence and explicit attestation semantics. |
| Add new terminal run statuses for replan | Rejected | Existing statuses plus progress/feed projection can express pending attention without expanding lifecycle state. | Projection cannot render required operator decisions without ambiguity. |

## Invariant diff outcome

No new invariant text is required beyond Phase 2's accepted amendments in `docs/workflow-invariants.md`. This RFC concretizes those amendments into a proposed mechanism. If implementation later chooses different enforcement, update this RFC or archive it as superseded rather than weakening the invariants.

## Redesign slice candidates

These are design outputs for Phase 5; they are not authorized implementation during the freeze.

| Slice ID | Evidence entries addressed | Files/modules likely touched | Success criteria | Required tests | Status | Explicit deferrals | Dogfood/run plan |
|---|---|---|---|---|---|---|---|
| RPL-01 — Replan proposal store and projection | F-004, F-006, F-008, F-009 | `src/domain.rs`, `src/state.rs`, `src/daemon.rs`, `src/ipc.rs`, `src/workflow/projection.rs`, `src/cli.rs`, docs | Durable proposal records with `pending/accepted/rejected/deferred/superseded`; status/watch/monitor render the same projection and exact decision commands; interrupted runs preserve pending proposals | Unit: state transitions/idempotency; projection snapshots for every state; daemon restart fixture preserves pending proposal; grep/parity guard for projection | `planned` | No autonomous planner; no new run/slice status | Run through Khazad-Doom after Phase 4 using fake worker that emits a proposal. |
| RPL-02 — Finding disposition and repair-authority flow | F-003, F-006, F-008 | worker/repair output schemas, `src/workflow/manager.rs`, `src/workflow/prompts.rs`, `docs/workflow-invariants.md`, tests | Actionable findings require terminal disposition; out-of-area or workflow-policy repair emits proposal instead of mutating silently; accept applies, reject/defer preserves evidence | Integration: repair attempts out-of-area change and blocks with proposal; accept path applies follow-up/revision; reject path remains blocked with rationale; schema test rejects unresolved actionable findings | `planned` | Auto-apply tier remains empty | Dogfood with a slice that intentionally produces an out-of-area repair proposal. |
| RPL-03 — Queue-history handoff and roadmap truth lint | F-001, F-004, F-009, F-014 | final report/handoff generation, roadmap/matrix lint script or generator, `.workflow/slices`, docs | Handoffs/reports include plan revisions; roadmap status cannot contradict slice/run evidence without lint failure; handoff blocked by unresolved pending proposal unless operator marks non-blocking | Fixture: matrix status mismatch fails lint; report/handoff snapshot includes accepted/rejected/deferred proposal history; unresolved pending proposal blocks handoff readiness | `planned` | Generated matrix may replace manual matrix later; no rich planning UI | Dogfood on the first redesign implementation run so closure/report truth is tested immediately. |

## Done-when check

- RFC accepted/rejected/deferred: **accepted for redesign planning**.
- Accepted outcomes translated into invariant diffs: **yes** — Phase 2 invariants already cover the law; this RFC records no additional invariant text needed.
- Redesign slices produced: **yes**, RPL-01..RPL-03 above.
- Open questions left as ambient debt: **none**; deferred alternatives have reconsider conditions.
- Status/rendering and resume behavior specified for every accepted replan state: **yes**, see state, rendering, and resume sections.
