# Architecture review — revision Phase 4

Date: 2026-07-06  
Status: complete for Phase 4 planning; no product implementation is authorized by this review.

This review is the gate between the evidence/invariant/RFC phases and future code. It cites only the failure ledger, the roadmap truth audit, and accepted invariants/RFCs. It prefers deepening existing modules over adding new ones, and it rejects redesign that is not evidence-backed.

## Inputs

- `docs/design/failure-ledger.md` — F-001..F-014.
- `docs/design/roadmap-truth-audit-2026-07-06.md` — PI-00..PI-05 status reconciliation and PI-05 renderer duplication proof.
- `docs/workflow-invariants.md` — Phase 2 accepted amendments.
- `docs/design/replan-checkpoints.md` — Phase 3 mechanism decision and RPL-01..RPL-03 candidates.
- Phase 3 review input: replan must not become a nag channel for the known close-record/report-promotion bug; assign that promotion seam before or alongside RPL-01.

## Executive conclusion

Khazad-Doom's core architecture is viable. F-011 shows the main loop can complete long multi-slice runs with retries, checkpoints, gates, and cleanup. The problem is not that the daemon should be replaced by a planner or a broad adaptive workflow engine. The problem is that several workflow truths are split across too many places:

1. successful-run publication truth is split between integration branch, transient artifacts, main branch, reports, and handoff commands;
2. terminal reasons are split between prose errors, incidents, run summaries, and status renderers;
3. status/feed interpretation is split between `src/workflow/projection.rs` and CLI rendering code;
4. plan/queue changes have no durable proposal/decision seam;
5. worker/repair authority is expressed mostly in prompts and post-hoc scope checks;
6. roadmap status is hand-maintained Markdown rather than derived from slice/run evidence.

The architecture should therefore add or deepen a few small interfaces that hide large behavior: completion publication, terminal reason classification, feed projection, replan proposal storage/application, finding disposition, and roadmap truth linting. It should not add a runtime mission object, autonomous replanning engine, auto-applying planner, or auto-blocking complexity telemetry.

## Pressure-point map

| Pressure point | Evidence | Governing-method classification | Architecture response |
|---|---|---|---|
| Self-dogfooding stopped while cross-repo use continued | F-001; Phase 1 audit | Missing enforcement/test for an accepted workflow expectation | Add roadmap/dogfood truth checks; every redesign implementation slice must either run through Khazad-Doom or record an exception. |
| Close-record/report promotion stranded truth on integration branches | F-004; Phase 3 review sequencing risk | Existing invariant covered desired behavior; implementation/promotion seam failed | Add a completion-publication seam before or alongside replan. This is the first implementation priority. |
| Gate failure after slice merges left partial state hard to reason about | F-009 | Invariant exists but status/selection implications lack enforcement/test | Completion publication and terminal reason must distinguish merged evidence from accepted/closed state. |
| Worker/repair authority crossed verification and slice fences | F-003 | Existing slice/D5 invariant was too weakly enforced for repair | Add finding disposition and repair-authority flow; repairs propose out-of-authority changes instead of applying them. |
| `blocked` is overloaded | F-006 | One small mechanism missing: structured terminal reason | Add terminal reason data rendered through projection; avoid new terminal statuses. |
| Status/monitor interpretation drift and Pi UI churn | F-013; Phase 1 PI-05 failed grep | Invariant exists but lacks enforcement/test | Deepen the existing projection seam; CLI becomes a painter and stops matching raw event types. |
| Fake-runner/profiles not consistently visible in reports | F-005; Phase 1 PI-03 gap | Invariant exists but lacks integration tests | Centralize run attestation/profile/report construction in completion publication or a small attestation builder. |
| Pi contract/profile/operator-escalation rows partially implemented by hand | F-010; F-014; Phase 1 PI-02..PI-04 gaps | Roadmap truth failed; source presence is not acceptance evidence | Finish tests/dogfood closure for existing PI rows; do not add a new abstraction unless tests reveal a real seam gap. |
| Deterministic environment/auth failures were stale-daemon-sensitive | F-002; Phase 1 PI-01 installed-binary smoke | Existing invariant now appears implemented, but production/install path lacked proof | Add done-level install/runtime evidence to PI-01/PI-02 closure; not a broad worker-readiness preflight. |
| Worktree setup friction | F-012 | Already addressed by daemon-owned setup seam; preserve it | Keep setup daemon-owned and covered by tests; no new module unless setup failures recur. |
| Runtime economics and repair cost | F-003, F-006, F-009, F-011 | Invariant exists; evidence says keep costs visible, not blocking | Preserve economics reporting; use it to price replan friction but do not auto-block. |
| Advisory complexity telemetry | Phase 2 deferral; F-011 | Explicitly deferred | No architecture seam now; reconsider only with Phase 4/later evidence of missed complexity regressions. |

## Deep modules earning their keep

These modules/seams should be preserved and deepened only where the evidence says they leak:

- `agent_profile` is the right seam for worker profile resolution. Phase 1 found missing integration proof, not a need to move profile logic elsewhere.
- `pi_contract` is the right seam for Pi event/stderr knowledge. Phase 1 found missing preflight/fixture proof, not scattered production parsing in `src/`.
- `workflow::gate` + `workflow::shell` is a useful command-execution seam. F-002/F-012 say environment/setup failures need classification and context, not a new gate owner.
- `state::Store` and `artifact::Store` are the durable persistence seams. Replan proposals should reuse/deepen these rather than creating a second store.
- `workflow::projection` is the right feed seam, but it is not yet authoritative because `src/cli.rs` still interprets raw events.
- `workflow::manager` is large but cohesive temporal orchestration. Do not split it by line count. Extract only behavior with an external contract and repeated leakage: completion publication, terminal reasons, and replan proposal application.

## Proposed seams

| Seam/module | Small interface | Behavior hidden behind it | Evidence | Why this is deeper than today's shape | Tests enabled |
|---|---|---|---|---|---|
| `CompletionPublisher` / finalization seam | `publish_successful_run(input) -> PublishedRun` | close completed slices, write/promote final report, copy terminal artifacts, record incidents/warnings, compute final SHA/handoff readiness, ensure handoff commands point at the truth-bearing final commit | F-004, F-009, F-014; Phase 3 sequencing risk | Callers stop knowing the exact order and storage locations for close records/reports/handoff truth | R8-style fixture: final handoff branch/SHA includes implementation + close/report truth; missing slice metadata emits structured incident; handoff refuses incomplete publication. |
| `TerminalReason` / primary reason seam | `TerminalReason::from_run(details/events) -> TerminalReason` | classify blocked/failed/completed reason kind, resolution owner, retryability/operator-action, evidence links, remediation/disposition links | F-004, F-006, F-009; Phase 2 structured-reason invariant | Renderers and reports consume one typed reason instead of scraping prose or event payloads | Fixtures for auth, wrong-queue, already-closed, scope violation, gate failure, cancellation, repair rejected. |
| `StatusFeed` projection seam (existing) | `project_run(details) -> StatusFeed` | all event/progress/incident/terminal-reason wording for CLI/watch/monitor/Pi adapter | F-013; Phase 1 PI-05 grep failure | Layout remains in painters; interpretation stays in one daemon-side module | Grep/parity test: CLI renderers contain no raw event-type matching; snapshot parity across status/watch/monitor/feed adapter. |
| `ReplanStore` / proposal decision seam | `propose`, `decide`, `apply_accepted`, `pending_for_run` | proposal ids/states, accepted/rejected/deferred/superseded transitions, idempotent application, restart semantics | F-008, F-004, F-009; Phase 3 RFC | Manager sees a small proposal lifecycle interface instead of ad-hoc queue mutations | State transition tests; crash-window tests around `applied_at`; daemon restart restores `awaiting_replan`. |
| `FindingDisposition` / authority seam | `validate_findings(result) -> DispositionPlan` | actionable finding detection, terminal disposition requirements, repair authority checks, follow-up/proposal emission | F-003, F-006, F-008; Phase 2 finding/repair invariants | Worker/repair outputs are validated through one contract instead of prompt prose plus scattered checks | Worker/repair output with unresolved actionable finding fails; out-of-area repair creates proposal; rejected/deferred findings remain visible. |
| `RoadmapTruthCheck` / dogfood lint seam | `check_repo_status(repo) -> RoadmapTruthReport` | compare matrix/workpackage status, slice JSON, run/close metadata, recorded exceptions, dogfood evidence | F-001, F-004, F-014; Phase 1 truth audit | Markdown docs stop being the status authority; the lint/generator owns reconciliation | Matrix status mismatch fixture fails; product commit without run/exception disposition is reported. |

Each proposed seam has a smaller interface than the behavior it hides. None introduces a second workflow owner; all remain daemon-owned or lint/report-only.

## Lifecycle ownership assignments

| Lifecycle concern | Owner after redesign | Notes |
|---|---|---|
| Slice selection/dependency ordering | Existing artifact/state + manager orchestration | Preserve closed-dependency behavior; feed contradictions to completion/replan only when evidence disagrees. |
| Worker execution and retry | Existing manager + agent runner | Preserve at-least-once semantics and non-retryable launch classification. |
| Worktree setup | Existing daemon-owned setup path | Preserve F-012 hardening; setup failures remain operator/daemon environment failures. |
| Integration gate and repair | Existing gate/shell + manager, with `FindingDisposition` for authority | Repair remains gate-driven; out-of-authority changes become proposals. |
| Successful run publication | New `CompletionPublisher` seam | Owns close records, final report, final SHA, handoff readiness, and promotion completeness. |
| Terminal blocked/failed explanation | New `TerminalReason` seam | Owns structured primary reason; statuses remain unchanged. |
| Replan proposals/revisions | New `ReplanStore` seam | Owns proposal state and idempotent application; operator authorizes. |
| Status/watch/monitor wording | Existing `workflow::projection`, made authoritative | CLI/Pi become painters only. |
| Roadmap/matrix truth | `RoadmapTruthCheck` lint/generator | Reports contradictions; does not mutate workflow state without operator action. |

## Ordered redesign slice list

These slices are architecture outputs for Phase 5. They should be converted into JSON Issue Slices and dogfooded unless explicitly exempted.

| Order | Slice ID | Evidence addressed | Files/modules likely touched | Success criteria | Required tests | Status | Explicit deferrals | Dogfood/run plan |
|---:|---|---|---|---|---|---|---|---|
| 1 | PUB-01 — Completion publisher and close-record promotion | F-004, F-009, F-014 | `src/workflow/manager.rs`, `src/artifact.rs`, `src/domain.rs`, `src/gitutil.rs`, tests, docs | Successful run publication is atomic from operator perspective: final handoff branch/SHA contains implementation plus close records/reports; missing close metadata emits structured incidents; handoff/report truth is not stranded on an unadvertised commit | R8-style regression fixture; handoff final SHA includes close/report commit; `slice_close_skipped` path keeps run completed-with-incident or blocks per policy with structured reason; resume does not duplicate publication | `planned` | No auto-push/PR mutation; no roadmap generator yet | First dogfooded implementation slice; if current closure path misbehaves, record it as run evidence and use this slice to fix it. |
| 2 | FEED-01 — Terminal reason and projection authority | F-006, F-009, F-013; Phase 1 PI-05 | `src/domain.rs`, `src/workflow/projection.rs`, `src/cli.rs`, `src/daemon.rs`, tests | `blocked`/`failed` expose structured primary reason; status/watch/monitor/Pi adapter render one feed; CLI no longer interprets raw event types | Terminal reason fixtures; projection snapshots; grep/parity test forbids raw event-type matching in CLI painters | `planned` | No new run/slice statuses; offline inspect projection deferred unless needed | Dogfood after PUB-01 so run summaries can expose structured reasons during later redesign. |
| 3 | RPL-01 — Replan proposal store and projection | F-004, F-006, F-008, F-009 | `src/domain.rs`, `src/state.rs`, `src/daemon.rs`, `src/ipc.rs`, `src/workflow/projection.rs`, `src/cli.rs`, docs | Durable proposal records with `pending/accepted/rejected/deferred/superseded`; status/watch/monitor show exact decision commands; interrupted runs preserve pending proposals | State transition/idempotency tests; daemon restart fixture; projection snapshots for every proposal state | `planned` | No autonomous planner; no new run/slice status | Dogfood using fake worker or daemon fixture that emits a proposal. |
| 4 | RPL-02 — Finding disposition and repair-authority flow | F-003, F-006, F-008 | worker/repair output schemas, `src/workflow/manager.rs`, `src/workflow/prompts.rs`, tests | Actionable findings require terminal disposition; out-of-area or workflow-policy repair emits proposal instead of mutating silently; accept/reject/defer preserve evidence | Integration repair fixture attempts out-of-area change and yields proposal; accept path applies revision/follow-up; reject/defer path remains visible; schema rejects unresolved actionable findings | `planned` | Auto-apply tier remains empty | Dogfood with a controlled out-of-area repair proposal to price operator friction. |
| 5 | RPL-03 — Queue-history handoff and roadmap truth lint | F-001, F-004, F-009, F-014 | final report/handoff generation, lint/generator script, `.workflow/slices`, docs | Handoffs/reports include plan revisions; roadmap status cannot contradict slice/run evidence without lint failure; unresolved pending proposals block handoff unless operator marks non-blocking | Matrix mismatch fixture; report/handoff snapshot includes accepted/rejected/deferred proposal history; unresolved pending proposal blocks handoff readiness | `planned` | Generated matrix can replace manual table later; no rich planning UI | Dogfood on the first run after RPL-01/RPL-02 so queue history appears in real handoff. |
| 6 | PI-PROOF-01 — Pi-native acceptance evidence closure | F-002, F-005, F-010, F-014; Phase 1 PI-02..PI-04 gaps | `src/pi_contract.rs`, `src/agent_profile.rs`, `src/workflow/manager.rs`, `extensions/khazad-worker`, tests/docs | Existing Pi-native rows gain done-level evidence: preflight records Pi contract; profile summary is identical across run/handoff/report/status/economics; fake runner is unmistakable; `ask_operator` black-box ask/answer/timeout/restart cases pass | Preflight contract assertion; profile-surface integration test; fake-runner report attestation test; scripted `ask_operator` workflow and timeout/restart tests | `planned` | No new Pi UI overlay; no fallback models; no auth preflight probe | Dogfood only after projection and completion publication are reliable enough to make acceptance evidence durable. |

## Existing matrix-row dispositions

- PI-00 and PI-01 do not need new architecture seams. They need dogfooded closure or explicit documentation-only exemption once product work resumes.
- PI-02 and PI-03 should keep their current `pi_contract` and `agent_profile` seams; the missing work is done-level evidence and integration tests.
- PI-04 should not expand into a richer interaction system until the black-box ask/answer/timeout/restart workflow passes and at least one dogfooded scenario exercises it.
- PI-05 is FEED-01. The failed grep criterion is a concrete architecture leak.

## Rejected alternatives

| Alternative | Decision | Rationale | Reconsider condition |
|---|---|---|---|
| Split `workflow::manager` by phase because it is large | Rejected | Size is not the observed failure. F-011 shows orchestration works; failures are specific truth/authority leaks. | If implementation of PUB/RPL/FEED still requires broad edits across unrelated manager paths after seams exist. |
| Make roadmap Markdown authoritative again with stricter manual process | Rejected | F-014 is exactly manual status drift. | Never as sole truth; Markdown may remain generated or audited summary. |
| Let replan proposals auto-fix close-record contradictions | Rejected for v1 | Phase 3 review warns this would nag on a known promotion bug; fix publication first. | After PUB-01, if contradictions are mechanical and approval is proven bottleneck. |
| Add new terminal statuses for wrong-queue/replan/blocked subtypes | Rejected | F-006 needs structured reason, not lifecycle explosion. | If projection + structured reason cannot drive unambiguous operator decisions. |
| Add a pre-run auth/readiness probe | Rejected | Phase 1 shows current PI-01 classification works; Phase 2 rejected credential mutation/preflight overreach. | Repeated current-binary auth failures burn retries despite classifier tests. |
| Add autonomous daemon/planner replan engine | Explicitly deferred | Phase 3 answered this: evidence supports proposal/approval, not autonomous mutation. | Repeated accepted proposals are mechanical and human approval is proven bottleneck. |
| Auto-block based on complexity telemetry | Explicitly deferred | Phase 2 disposed this hypothesis; F-011 says preserve throughput. | Repeated failures correlate with a daemon-computable metric and false-positive cost is understood. |

## Risk notes for Phase 5

- **Operator friction is expected.** Empty auto-apply means PUB/RPL dogfood runs may feel more interactive. That is the mechanism making control visible, not failure, unless operators repeatedly cancel and hand-apply good proposals.
- **Publication must lead replan.** If close/report truth remains split, RPL-01 will turn a known bug into repeated proposals. PUB-01 is therefore first.
- **Projection must precede rich pending states.** `awaiting_replan` should not land while CLI still duplicates raw event interpretation.
- **Dogfooding is part of the evidence.** Failed dogfood attempts are not embarrassment; they are exactly the run evidence the revision plan requires.

## Done-when check

- Every named pressure point cites ledger or truth-audit evidence: **yes**, see pressure-point map and ordered slice list.
- Every proposed seam has a smaller interface than the behavior it hides: **yes**, see proposed seams table.
- Rejected alternatives have reconsider conditions: **yes**.
- Review produces a short ordered list of redesign slices: **yes**, PUB-01, FEED-01, RPL-01, RPL-02, RPL-03, PI-PROOF-01.

## Understanding delta

The main architecture correction from Phase 3 is that replan is not the first mechanism to implement. Replan depends on reliable publication truth. The close-record/report-promotion seam must be fixed first or replan will repeatedly ask the operator to adjudicate contradictions caused by Khazad-Doom itself.
