# Autonomous Frontier — Master Traceability Matrix

Date: 2026-07-09
Owner: sivanirosh
Scope: let a run continue past its initial slice queue by promoting worker-proposed
follow-up slices into runnable work, under a machine-checkable mission envelope,
without creating a second plan authority beside the replan mechanism.

Roadmap Markdown is checked planning output, not workflow truth. `scripts/roadmap-truth-check`
applies: no row below may claim done/closed/accepted without closed slice JSON plus named
daemon report evidence. Every implementation task must reference a Slice ID from this matrix.
When a slice moves to `ready`, convert its workpackage into a JSON Issue Slice under
`.workflow/slices/`.

## Core invariant

```text
The current slice never expands silently.
The daemon may generate and run follow-up slices only inside the approved
mission envelope, only through the replan proposal/decision channel, and
only with recorded provenance. Outside the envelope, it stops and asks.
```

## Product decisions

- **AD1 — One authority channel.** Every candidate follow-up is a replan proposal
  (`add_followup_slice`). There is no parallel "frontier pipeline" beside the replan store.
  Autonomy changes *who may record an accept decision* (operator vs. envelope-delegated
  daemon policy), never what proposing, deciding, or applying means. Rejected/deferred
  proposals are never auto-reapplied.
- **AD2 — Decision/application split.** Promotion policy is a pure, deterministic function
  (envelope + slice graph + proposal + budget → tier + reason codes). Application is one
  idempotent engine shared by operator-accepted and auto-accepted proposals. The workflow
  manager applies side effects; policy code never touches git, state, or IPC.
- **AD3 — Evidence-gated authority ladder.** `autonomy_level`: `off` → `shadow` → `promote`
  → `run`. Default is `off`. Each step up is unlocked by recorded evidence, matching the
  replan RFC's reconsider conditions (the auto-approve tier stays empty until shadow-mode
  data proves a mechanically safe class). No level is skipped in production.
- **AD4 — Envelope bounds delegation, not truth.** The MissionEnvelope is a daemon-owned
  run record that bounds what the daemon may auto-accept. Slice JSON plus daemon/run state
  remain the only live source of workflow truth; the envelope is authorization scope, not a
  competing plan ledger.
- **AD5 — Findings-triggered only.** Candidates come only from worker/repair outputs and
  operator commands at existing replan checkpoints. No ambient planner pass, no daemon
  self-prompting for ideas (preserves F-011 throughput doctrine).
- **AD6 — Fail toward asking.** Any classification ambiguity resolves to a higher tier.
  A false negative that queues or asks is acceptable; a false positive that auto-promotes
  beyond authority is an invariant violation (same asymmetry REPAIR-01 accepted for repair).

## Governance reconciliation (must be accepted in AF-00 before any runtime slice starts)

Three Phase-2 explicit deferrals are reopened, each by firing its recorded reconsider
condition — not by ignoring it:

| Deferral | Recorded reconsider condition | Evidence this epic must cite or produce |
|---|---|---|
| Runtime mission object | "recorded slice revisions cannot express the operator's durable intent" | Auto-promotion needs bounds on *not-yet-existing* slices (allowed areas, budgets, non-goals); no slice revision can express that. AF-00 argues this; AF-02 implements the minimal record. |
| Automated planner authority to mutate queues | "production evidence that manual approval is the bottleneck and accepted changes are mechanically safe" | AF-04 shadow mode produces the classifier-vs-operator agreement data. AF-06 stays blocked until AF-00's evidence bar is met. |
| Daemon-internal autonomous replan engine | "repeated recorded findings show a mechanical replan pattern that humans approve unchanged" | Same shadow-mode evidence; scope stays findings-triggered (AD5), so the "ambient planner" rejection stands. |

## Status state machine

`planned` → `ready` → `in_progress` → `done`
Any state → `blocked` (blocker named explicitly).
Decision not to implement → `explicitly_deferred` (rationale + revisit condition in the workpackage).
No hidden states: no "mostly done", no "wired later".

## Runway (blockers that are not AF slices)

- **RW-1 — Area contract lands.** The in-flight area-contract work (`.workflow/AREA_CONTRACT.md`,
  `scripts/validate-workflow-areas`, slice schema pattern, `src/artifact.rs`) must merge and be
  dogfooded first. Envelope containment (`candidate.areas ⊆ envelope.allowed_areas`) reuses its
  literal-prefix semantics; building on an uncommitted contract is forbidden.
- **RW-2 — Pending-attention notifications.** Tier-3 stops in a long autonomous run must not
  rot unseen (F-010: the first production `ask_operator` timed out in 5 minutes because nothing
  notified). The HERDR-06B follow-up (origin notification on `awaiting_operator`/`awaiting_replan`,
  dedupe per terminal transition) is a hard dependency of AF-06, not of the earlier slices.

## Matrix

| Product Decision | Required Feature | Slice ID | Files / Modules Likely Touched | Success Criteria | Required Tests | Status | Explicit Deferrals |
|---|---|---|---|---|---|---|---|
| AD1–AD6 | Frontier doctrine RFC + proposed invariant amendments | AF-00 | `docs/design/frontier-autonomy.md` (new), `docs/workflow-invariants.md`, this matrix | RFC defines envelope fields, tier semantics, autonomy ladder, stop rules, provenance requirements, and the numeric evidence bar for enabling `promote`/`run`; each reopened deferral cites its reconsider condition; amendment records follow the Phase-2 format | Doc review; `scripts/roadmap-truth-check` passes; grep: no runtime behavior claims | `ready` | No runtime behavior; acceptance of the RFC is the gate for AF-02..AF-06 |
| AD1 | Typed follow-up slice drafts through the replan channel | AF-01 | `src/workflow/schema.rs`, `src/domain.rs`, `src/workflow/manager.rs`, `src/workflow/prompts.rs`, `.workflow/schema/slice.schema.json`, tests | Worker output accepts `candidate_followup_slices[]` (full draft: id/title/goal/areas/acceptance/verify/depends_on/must_ask_if/rationale); daemon converts each into a `pending` replan proposal with a typed draft payload and evidence links; slice schema gains a `provenance` block; drafts are validated with the same rules as real slices (incl. area contract) before a proposal is created | Unit: draft validation matrix (bad areas, empty acceptance, dup id, cycle); fake-runner e2e: worker emits candidate → pending proposal visible in status/feed with decision commands; invalid draft → finding, no proposal | `planned` (blocked by RW-1) | No classification, no application, no autonomy; malformed drafts never crash the run |
| AD4 | Mission envelope record + budgets in run state | AF-02 | `src/domain.rs`, `src/state.rs`, `src/cli.rs`, `src/ipc.rs`, `src/workflow/projection.rs`, report/handoff generation, tests | Run start records a durable MissionEnvelope (goal, allowed_areas, non_goals, verify_profile, max_auto_promotions, max_depth, max_generated_slices, autonomy_level default `off`, must_ask_if); survives restart/resume; rendered identically in status/watch/monitor/report/handoff; absent envelope == `off` for old runs | Unit: serde/defaults/validation (areas pass contract); restart fixture preserves envelope + budget counters; projection snapshot | `planned` (blocked by RW-1) | Envelope grants no authority at any level in this slice; no classifier |
| AD2, AD6 | Deterministic promotion policy (pure classifier) | AF-03 | new `src/workflow/frontier.rs`, tests | Pure function `classify(envelope, slice_graph, proposal, budget_state) → TierDecision{tier, reason_codes[]}`; Tier 0 attest-inline / Tier 1 auto-promote / Tier 2 queue-pending / Tier 3 ask-operator; every rule has a machine-readable reason code; ambiguity resolves upward (AD6); not wired to any runtime path | Exhaustive table tests: inside-envelope→T1; outside-area→T3; new dependency edge→T3; envelope must_ask_if hit→T3; no verify→T2; duplicate of open/closed slice→T2/reject; duplicate of rejected/deferred proposal→T3; budget/depth exhausted→stop; non_goal overlap→T3; property test: no input panics | `planned` | No IPC/state/git access from the module; no LLM critic in the authorization path (may be added later as advisory-only) |
| AD3, AD5 | Shadow mode: classify, record, measure — never mutate | AF-04 | `src/workflow/manager.rs`, `src/workflow/attention.rs`, `src/workflow/projection.rs`, report generation, tests | With `autonomy_level=shadow`, every candidate proposal is classified at existing replan checkpoints; tier + reason codes recorded on the proposal and as events; report gains a frontier section with would-have-promoted list and classifier-vs-operator agreement (T1-classified proposals later operator-accepted-unchanged vs. not); zero queue mutation at any autonomy level | Fake-runner e2e: shadow run records tiers, queue byte-identical to `off`; agreement metric fixture; projection snapshot for shadow annotations | `planned` | No creation/commit/run of slices; no notifications changes |
| AD1, AD2 | Apply engine for accepted `add_followup_slice` (operator-authorized) | AF-05 | `src/workflow/manager.rs`, `src/artifact.rs`, `src/gitutil.rs`, `src/state.rs`, `src/cli.rs`, tests | Completes RPL-01's deferred apply path: operator `replan accept` on a follow-up draft → daemon validates draft, writes `.workflow/slices/<id>.json` with provenance in the integration worktree, commits it before any worker runs it, extends the remaining queue at an explicit checkpoint, records before/after queue snapshots + `applied_at` + checkpoint ids; idempotent under crash/resume (`replan_apply_incomplete` semantics per RFC) | Unit: idempotent re-apply; crash-between-accept-and-apply fixture resumes correctly; e2e: operator accepts mid-run → generated slice runs → closes with `closed_by_run`; rejected/deferred drafts stay unapplied | `planned` | Operator authority only — autonomy still cannot accept; parallel-layer insertion deferred (append serially after current layer) |
| AD3, AD6 | Envelope-delegated auto-accept: `promote` and `run` levels | AF-06 | `src/workflow/manager.rs`, `src/workflow/frontier.rs`, `src/workflow/attention.rs`, `src/domain.rs`, tests | For Tier-1 proposals within budget, daemon records an accept decision with `authorizer: "envelope:<run_id>"`, `source: "frontier_policy"`, and applies via the AF-05 engine; `promote` creates+commits generated slices but does not run them; `run` appends them to the frontier and continues serially; stop rules enforced (budget/depth exhausted → structured pending-attention/terminal reason; Tier 3 → `awaiting_replan` + origin notification); every auto-decision cites tier reason codes | E2e (fake runner): T1 auto-promoted and run; T3 pauses with notification evidence; budget exhaustion stops with structured reason; auto-accept of a previously rejected draft is impossible by construction; kill/resume mid-promotion is idempotent | `planned` (blocked by AF-00 evidence bar, RW-2) | Parallel autonomous frontier deferred (serial only); auto-accept for any change kind other than `add_followup_slice` deferred (empty tier remains for areas/verify/policy mutations) |
| AD1, AD4 | Frontier attestation in reports and handoffs | AF-07 | report/handoff generation, `src/workflow/read_model.rs`, `src/workflow/projection.rs`, `scripts/roadmap-truth-check`, tests | `plan_revisions` extended with promotion graph: envelope snapshot, generated slices with parent/child edges + origin proposal ids, tier decisions with reason codes, budget consumption, deferred-fog list, operator-needed stops; handoff readiness still blocked by pending proposals; generated slices satisfy roadmap-truth-check like hand-written ones | Snapshot: report/handoff with one auto-promoted + one rejected + one deferred candidate; lint fixture: generated slice closure passes truth-check | `planned` | No new truth store; graph is derived from proposal/decision/slice records only |
| all | Dogfood proof: staged autonomy on a real mission | AF-08 | `.workflow/slices/`, `docs/design/evidence/`, this matrix | Stage A: shadow run on a small mission with one seeded discovery — candidate classified T1, queue untouched, report shows would-have-promoted. Stage B: `run`-level rerun — daemon auto-promotes, generated slice runs and closes, report shows provenance chain and envelope compliance. Stage C: one candidate engineered outside the envelope → Tier-3 stop fires, notification observed, operator decides, run resumes | The run evidence itself + evidence doc under `docs/design/evidence/`; all three stages attested from daemon state, not prose | `planned` | Multi-follow-up chains (depth > 1) demonstrated only if Stage B naturally produces one; not required for closure |

## Dependency order

```text
RW-1 (area contract)      — merge in-flight work first; AF-01/AF-02 build on it
AF-00 (doctrine RFC)      — no code deps; gates AF-02..AF-06 by acceptance
AF-01 (typed drafts)      — after RW-1; pure schema/proposal plumbing
AF-02 (mission envelope)  — after RW-1, AF-00; independent of AF-01
AF-03 (pure classifier)   — after AF-01 + AF-02 (needs both types); no runtime wiring
AF-04 (shadow mode)       — after AF-03; produces the evidence for AF-06
AF-05 (apply engine)      — after AF-01; independent of AF-03/AF-04 — may run in parallel
                            with them; valuable standalone (operator-driven follow-ups)
RW-2 (attention notify)   — any time before AF-06
AF-06 (auto-accept)       — after AF-04 evidence bar + AF-05 + RW-2; the only slice that
                            grants the daemon decision authority
AF-07 (attestation)       — after AF-05; extend once AF-06 lands
AF-08 (dogfood)           — last; exercises shadow → run → Tier-3 stop
```

## Cross-slice workflow acceptance test

Proves the slices connect into one coherent operator path. Run after AF-06.

```text
1. Operator starts `khazad-doom run` with an envelope: goal, allowed_areas
   ["src/foo/", "tests/"], max_auto_promotions=2, max_depth=1, autonomy_level=run.
2. Slice S-1's worker completes and emits two candidate_followup_slices:
   C-A (areas inside the envelope, acceptance testable, verify present) and
   C-B (touches "src/policy/", outside allowed_areas).
3. Daemon converts both into pending replan proposals with evidence links, then
   classifies at the post-integration checkpoint: C-A → Tier 1 (reason codes
   recorded), C-B → Tier 3 (reason: area_outside_envelope).
4. C-A is auto-accepted with authorizer "envelope:<run_id>", written to
   .workflow/slices/, committed in the integration worktree BEFORE any worker
   sees it, appended to the queue, and run to completion.
5. C-B pauses the frontier in awaiting_replan; the origin notification fires;
   status/watch/monitor show the proposal, tier reasons, and exact
   accept/reject/defer commands.
6. Edge condition: the daemon is restarted while C-B is pending and C-A's
   generated slice is mid-flight. On restart the run is interrupted; resume
   restores the envelope, remaining budget (1 of 2 consumed), the pending C-B
   proposal, and does not re-promote or duplicate C-A.
7. Operator rejects C-B with rationale; run completes; handoff shows the
   promotion graph (S-1 → C-A applied, C-B rejected), budget 1/2 used,
   envelope snapshot, and provenance on the generated slice.
8. Invariants: no slice JSON changed without a decision record; C-A's slice
   file carries provenance (parent S-1, origin proposal id, generation 1);
   queue snapshots before/after promotion hash-match the applied change;
   a rerun of publication creates no duplicate close/report commits.
```

## Explicit deferrals and rejections (epic level)

| Item | Decision | Rationale | Reconsider condition |
|---|---|---|---|
| Parallel autonomous frontier | Explicitly deferred | Serial-first isolates promotion correctness from layer-atomicity complexity (CPLX-03 lesson) | Serial dogfood evidence clean across ≥3 missions and a concrete throughput need |
| Auto-accept for change kinds beyond `add_followup_slice` | Explicitly deferred | Areas/verify/policy mutation stays operator-only per replan RFC "always requires operator approval" list | Never for policy mutation; others only with new RFC + evidence |
| LLM critic in the authorization path | Rejected | Authorization must be deterministic and testable (AD2); an LLM may annotate proposals as advisory evidence only | Never as authorizer; advisory critic is a future additive slice |
| Cross-run / standing mission envelopes | Explicitly deferred | Current need is bounding one run's autonomy; standing missions reintroduce the runtime-mission-object scope Phase 2 rejected | Recorded evidence that per-run envelopes force repeated re-entry of identical intent |
| Worker-initiated envelope edits | Rejected | The envelope is operator intent; workers propose slices inside it, never changes to it | Never |
