# Khazad-Doom failure ledger

Date opened: 2026-07-06  
Revision phase: Phase 0 from `REVISION_PLAN.md`.

This ledger turns preserved evidence into design inputs. It is not a speculation backlog: every entry names observed evidence, the invariant relationship, current coverage, and disposition.

## Scope

Bounded evidence scope for this ledger:

1. `docs/design/worker-run-complexity-audit.md`.
2. The Phase -1 evidence harvest: `docs/design/evidence/run-evidence-2026-07-06.md` and committed repo-local raw artifacts.
3. The 2026-07-06 daemon state-store snapshot across all recorded repositories, kept local-only for privacy and summarized publicly in the Phase -1 evidence harvest.
4. Commits after `2a6fc7c` through freeze boundary `3d640c6`.
5. Freeze-exception evidence commit `fc86574`.
6. Roadmap/matrix drift in `docs/roadmap/pi-native/00-matrix.md` and workpackages.

## Evidence grades

- **A** — raw run artifact, final report, incident, committed evidence summary, or local-only state-store snapshot.
- **B** — written audit with cited run/commit evidence.
- **C** — reconstruction from commit diff and commit message.
- **D** — operator recollection or narrative note.

## Root-cause classes

- **User mistake** — operator issued an incorrect command or misunderstood a documented behavior.
- **Repo setup gap** — environment, repo, toolchain, profile, or worktree setup blocked correct workflow execution.
- **Daemon bug** — daemon state transition, scheduling, merge, close, resume, or artifact behavior was wrong.
- **Design complexity** — behavior was technically possible but too implicit, overloaded, poorly owned, or spread across seams.
- **Workflow-governance/process gap** — Khazad-Doom's own planning/status/dogfooding process failed to reflect reality.

## Ledger entries

### F-001 — Self-dogfooding stopped while cross-repo use continued

- **Evidence grade:** A/C.
- **Sources:** Phase -1 harvest; git history `2a6fc7c..3d640c6`; state-store snapshot summarized in `docs/design/evidence/run-evidence-2026-07-06.md`.
- **Symptom:** No Khazad-Doom-produced commit appears after `2a6fc7c`/slice-041 on 2026-06-26, while the daemon state store shows continued use on repo-B..repo-E through 2026-07-05.
- **Root-cause class:** workflow-governance/process gap.
- **Invariant involved:** Missing/enforcement gap. The Pi-native matrix required workpackages to become JSON Issue Slices, but the process did not enforce dogfooding or record bypass reasons.
- **Current regression coverage:** None. A bypass leaves only normal git history unless the operator records it.
- **Remaining design gap:** The framework did not make routing around itself visible until Phase -1 reconstructed the gap.
- **Disposition:** Keep as ledger entry #1. Phase 5 must require redesign slices to run through Khazad-Doom again or record an explicit freeze exception.

### F-002 — Deterministic environment/auth failures burned retries and looked like implementation failures

- **Evidence grade:** A/B.
- **Sources:** R1 cargo-path failure in Phase -1 harvest; cross-repo auth failure runs summarized in Phase -1; `docs/design/worker-run-complexity-audit.md`; commit `55bb0ac`.
- **Symptom:** Deterministic setup failures consumed worker attempts and ended as `failed`: R1's daemon verify shell lacked `cargo`; three Pi auth launch failures each burned 3 attempts, ended failed, and recorded no `run_incident`.
- **Root-cause class:** repo setup gap + design complexity.
- **Invariant involved:** D2 — Truthful environmental failure. Existing invariant covers the desired behavior; implementation/runtime evidence shows it did not consistently hold in observed runs.
- **Current regression coverage:** PI-01 source fix exists, but production evidence has not observed its acceptance criteria holding. Cause remains undetermined: stale daemon binary vs classifier missing the `did not become ready` path.
- **Remaining design gap:** Launch/check failure classification must be verified against the actual daemon path operators run, not merely source tests.
- **Disposition:** Phase 1 truth audit must mark PI-01 according to executable tests and observed runtime behavior. Add a production-observation or install-fidelity check before considering PI-01 done.

### F-003 — Worker/repair authority crossed verification and slice fences

- **Evidence grade:** A.
- **Sources:** R1 worker attempt 2; R4 integration repair in Phase -1 harvest.
- **Symptom:** A worker attempted to fix an environment failure by editing `.workflow/khazad.json` verification commands. Separately, integration repair made a real semantic fix outside the four slices' declared areas.
- **Root-cause class:** design complexity.
- **Invariant involved:** Slice lifecycle fence and D5 separation of worker evidence from daemon/human attestation. The worker case violated the fence; the repair case exposes that repair authority has a broader, less explicit fence than normal workers.
- **Current regression coverage:** Later scope checks catch some worker file changes outside declared areas. Repair authority remains less clearly bounded.
- **Remaining design gap:** Authority model must distinguish worker implementation, daemon-owned environment repair, integration repair, and operator-approved policy changes.
- **Disposition:** Feed Phase 2 invariant diff and Phase 4 architecture review. Do not add broad repair machinery; clarify authority and tests first.

### F-004 — Close-record/report promotion gaps caused status drift and closed-slice reruns

- **Evidence grade:** A.
- **Sources:** R5/R6/R8 in Phase -1 harvest; cross-repo closed-slice rerun class; four `slice_close_skipped` incidents in local-only state snapshot.
- **Symptom:** R8 closed slice-041 and wrote reports on the integration branch, but main received only the implementation commit; slice-041 remained open on main. Earlier and cross-repo runs re-requested or re-ran closed work. Sometimes only the LLM worker noticed the handoff marked the slice closed.
- **Root-cause class:** daemon bug + design complexity + workflow-governance/process gap.
- **Invariant involved:** Closed dependencies must not rerun; completed runs close slice JSON before final reports; roadmap/status truth should have one source. Existing invariants cover the desired behavior, but promotion/merge mechanics dropped the truth.
- **Current regression coverage:** Partial. State store records `skipped_closed_slices` and `slice_close_skipped` incidents in some paths; main-branch slice truth can still diverge from daemon/integration branch truth.
- **Remaining design gap:** The source of truth for closed slices and report promotion is split between daemon state, integration branch, main branch, and docs.
- **Disposition:** Phase 1 roadmap truth audit and Phase 4 architecture review. Candidate structural invariant: live status has one generated/source-of-truth path; docs may summarize, not compete.

### F-005 — Fake-runner results were not distinguishable in final reports

- **Evidence grade:** A.
- **Sources:** R5 final report in Phase -1 harvest.
- **Symptom:** A fake-runner smoke run completed and its final report did not identify the runner/profile, making fake output look like real implementation evidence.
- **Root-cause class:** design complexity.
- **Invariant involved:** D1 says `FakeRunner` is a deterministic test double, not a real harness. Report/attestation surfaces did not preserve that distinction.
- **Current regression coverage:** `run_started` events carry agent metadata; final report/handoff/profile summary coverage must be verified under PI-03.
- **Remaining design gap:** Attestation/report surfaces need consistent runner/profile identity everywhere.
- **Disposition:** Phase 1 truth audit for PI-03. If not covered, add a redesign slice for report attestation/profile consistency.

### F-006 — `blocked` semantics are overloaded

- **Evidence grade:** A.
- **Sources:** Cross-repo blocked examples in Phase -1 harvest; R1/R5/R6 context.
- **Symptom:** `blocked` covered at least needs-operator-intent, already-closed/wrong-queue, and worker says work is present but outside current authority. These are operationally different states.
- **Root-cause class:** design complexity.
- **Invariant involved:** Existing status taxonomy is too coarse even when the run status is technically correct. D3/D6 imply actionable operator attention but do not fully classify blocked reasons.
- **Current regression coverage:** Incidents and terminal summaries may preserve details in newer runs; no consistent blocked-reason vocabulary is proven.
- **Remaining design gap:** Operator-facing status needs structured reason/kind without creating new terminal statuses unnecessarily.
- **Disposition:** Phase 2 invariant diff should evaluate failure-kind/blocked-kind requirements. Phase 4 should ensure projection/status uses one interpretation layer.

### F-007 — Scope fence works, but a correct catch can still cost a full failed run

- **Evidence grade:** A.
- **Sources:** repo-E M0 scope violation summarized in Phase -1 harvest.
- **Symptom:** Worker changed files outside slice areas; daemon caught it and failed the run after attempts. The immediately following run succeeded.
- **Root-cause class:** design complexity.
- **Invariant involved:** Scope violations must become structured failures. The invariant held.
- **Current regression coverage:** Scope-fence enforcement works for this observed case.
- **Remaining design gap:** No immediate mechanism gap proven. The cost is evidence for better planning/slice shaping, not for weakening the fence.
- **Disposition:** Keep as positive/negative evidence. Do not add a new mechanism unless repeated ledger entries show preventable scope-shape failures.

### F-008 — Operators bailed out when queue/integration trust was low

- **Evidence grade:** A.
- **Sources:** Cross-repo operator cancellation notes in Phase -1 harvest.
- **Symptom:** Operators stopped runs to validate manually or because `--all` appeared to rerun already completed work.
- **Root-cause class:** workflow-governance/process gap + design complexity.
- **Invariant involved:** Cancellation is explicit and durable, so the daemon recorded the bail-outs. The missing part is trustworthy queue/resume/replan visibility before the operator resorts to manual validation.
- **Current regression coverage:** Cancellation events are recorded. No replan checkpoint mechanism exists.
- **Remaining design gap:** Need explicit plan revision/replan state and visible queue truth before long runs proceed.
- **Disposition:** Primary evidence for Phase 3 replan checkpoint RFC, especially findings-triggered/manual-approval defaults.

### F-009 — Integration-gate failure after slice merges left partial state hard to reason about

- **Evidence grade:** A.
- **Sources:** repo-E integration gate failure summarized in Phase -1 harvest.
- **Symptom:** Many slices showed `merged` in slice state while the overall run failed at the final integration gate. Later work re-ran some already-merged work.
- **Root-cause class:** design complexity.
- **Invariant involved:** Gate before handoff held, but slice/run state readability was insufficient for downstream selection and operator trust.
- **Current regression coverage:** Checkpoints and terminal summaries exist in newer runs; their selection/resume implications need audit.
- **Remaining design gap:** Distinguish per-slice merged evidence from run-level accepted/closed state in status, resume, and slice selection.
- **Disposition:** Phase 4 architecture review; likely related to F-004 closed-state truth and F-006 blocked/exit-state classification.

### F-010 — `ask_operator`/operator escalation has no production evidence

- **Evidence grade:** A.
- **Sources:** Phase -1 harvest: `worker_questions` has 0 rows across 32 runs.
- **Symptom:** D3/PI-04 path exists in source but has not fired in observed production runs. Worker stops still became blocked/failed.
- **Root-cause class:** design complexity or no observed instance.
- **Invariant involved:** D3 is accepted doctrine, but production evidence is absent.
- **Current regression coverage:** Source tests exist for extension/IPC pieces; Phase 1 must run PI-04 declared tests before status changes.
- **Remaining design gap:** Unknown until Phase 1: maybe no slice hit `must_ask_if`, maybe extension not loaded, maybe workers did not call the tool.
- **Disposition:** Phase 1 truth audit. Treat as unproven until executable tests and at least one deterministic workflow scenario pass.

### F-011 — Long multi-slice runs show the core loop can work

- **Evidence grade:** A.
- **Sources:** Cross-repo positive evidence in Phase -1 harvest.
- **Symptom:** 12-slice runs completed in roughly 70–90 minutes; retries recovered real failures; checkpoint and cleanup events became common after hardening.
- **Root-cause class:** not a failure; positive control evidence.
- **Invariant involved:** Core JSON-slice, checkpoint, retry, gate, and cleanup invariants can work at meaningful scale.
- **Current regression coverage:** Existing integration tests plus production run evidence.
- **Remaining design gap:** Preserve these working properties while fixing failure modes.
- **Disposition:** Phase 4 must avoid redesign that destroys proven long-run throughput.

### F-012 — Worktree/setup friction required daemon-owned setup hardening

- **Evidence grade:** C/A.
- **Sources:** Commit `3d640c6`; Phase -1 event census includes `worktree_setup_completed` events.
- **Symptom:** Worktree setup became a first-class daemon concern after observed setup gaps.
- **Root-cause class:** repo setup gap.
- **Invariant involved:** Verification/worktree setup must run with declared context and classify setup failures as daemon/operator environment failures.
- **Current regression coverage:** Source implementation exists; Phase 1 must decide whether this work needs a matrix row or is documented as out-of-matrix completed work.
- **Remaining design gap:** Roadmap truth: this important hardening landed outside a dogfooded slice/matrix row.
- **Disposition:** Phase 1 work-outside-slice disposition; Phase 4 should preserve daemon-owned setup seam.

### F-013 — Status/monitor surface drift and Pi UI churn produced duplicated interpretation pressure

- **Evidence grade:** A/C.
- **Sources:** R4 monitor path repair in Phase -1 harvest; commits `a907099`, `622c760`; PI-05 workpackage.
- **Symptom:** Monitor/status repo path handling needed repair; optional Pi monitor overlay was added then removed/replaced; status projection appears in source while matrix status still says planned.
- **Root-cause class:** design complexity.
- **Invariant involved:** D6 requires daemon-owned explicit feedback and one shared feed projection. The desired invariant exists; roadmap/status implementation truth is unclear.
- **Current regression coverage:** `src/workflow/projection.rs` and feed adapter tests exist, but PI-05 declared tests/status must be audited.
- **Remaining design gap:** Ensure renderers are painters and roadmap docs do not claim stale state.
- **Disposition:** Phase 1 truth audit for PI-05 and out-of-matrix UI work; Phase 4 only if tests reveal remaining drift.

### F-014 — Pi-native matrix drifted from implementation reality

- **Evidence grade:** C/A.
- **Sources:** `docs/roadmap/pi-native/00-matrix.md`; source files `src/pi_contract.rs`, `src/agent_profile.rs`, `src/workflow/projection.rs`, `workerAsk` paths; Phase -1 harvest.
- **Symptom:** Matrix rows PI-02..PI-05 remain planned while corresponding source surfaces appear implemented or partially implemented. Some work has no clear matrix row.
- **Root-cause class:** workflow-governance/process gap.
- **Invariant involved:** The matrix itself says no hidden states/no mostly done. It failed its own status rule.
- **Current regression coverage:** None structural. Manual review found drift.
- **Remaining design gap:** Roadmap status must derive from slice/run state or be linted against it; docs must not become a second status ledger.
- **Disposition:** Phase 1 roadmap truth audit. Candidate structural invariant: one live status source for roadmap work.

## Scoped commit accounting

Post-`2a6fc7c` commits through the freeze boundary are accounted below. `Bypass/failure` means the work did not run through Khazad-Doom and is evidence for F-001 even if the change itself was useful.

| Commit | Summary | Disposition | Ledger links |
|---|---|---|---|
| `640b507` | Implement runtime economics and gate-driven repair | Bypass/failure: product/runtime work by hand | F-001, F-003, F-006, F-009 |
| `ef75ce2` | Polish README badges | Legitimate exemption candidate: doc/cosmetic | F-001 |
| `ef69f7c` | Surface run incidents and harden lifecycle recovery | Bypass/failure: daemon/runtime hardening by hand | F-001, F-006, F-009 |
| `d6ac4b8` | Harden workflow failure forensics | Bypass/failure: daemon/runtime hardening by hand | F-001, F-002 |
| `acb7440` | Strengthen Khazad workflow guardrails | Bypass/failure: workflow policy/guardrail work by hand | F-001, F-003 |
| `750cff7` | docs: audit worker launch failure complexity | Legitimate exemption candidate: evidence/design doc | F-002 |
| `55bb0ac` | fix: block non-retryable pi auth failures | Bypass/failure: PI-01 implementation by hand; not production-observed working | F-001, F-002, F-014 |
| `2b37cf1` | docs: add pi-native roadmap slices | Legitimate planning doc, later drifted | F-014 |
| `2f03af7` | feat: add pi-native worker surfaces | Bypass/failure: product/runtime work by hand | F-001, F-010, F-014 |
| `907ae7f` | fix: align pi contract with source vocabulary | Bypass/failure: PI contract work by hand | F-001, F-014 |
| `7cf3e4b` | chore: remove pi legacy event aliases | Bypass/failure or cleanup exemption to classify in Phase 1 | F-001, F-014 |
| `a907099` | Remove obsolete Pi monitor extension | Bypass/failure: UI/product removal by hand | F-001, F-013 |
| `29e33af` | Use operator-wide agent profiles | Bypass/failure: profile behavior by hand | F-001, F-002, F-014 |
| `8b76af2` | Remove repo-local worker profiles | Bypass/failure: profile behavior by hand | F-001, F-002, F-014 |
| `622c760` | Add explicit Pi feed widget adapter | Bypass/failure: UI/adapter work by hand | F-001, F-013, F-014 |
| `3d640c6` | Add daemon-owned worktree setup | Bypass/failure: worktree setup hardening by hand | F-001, F-012, F-014 |
| `fc86574` | Add revision plan and Phase -1 run evidence harvest | Allowed freeze exception: evidence preservation | F-001 |

## Scoped run/report accounting

| Evidence set | Accounting |
|---|---|
| R1–R8 Khazad-Doom self-runs | Accounted in Phase -1 harvest and ledger F-002, F-003, F-004, F-005, F-013. |
| Committed `.workflow/reports/` for R2/R3/R4/R7 | Accounted in Phase -1 harvest; positive/repair evidence in F-003, F-011, F-013. |
| R5/R8 unpromoted reports and R8 close record | Accounted in F-004 and F-005. |
| Cross-repo state-store snapshot, 24 non-self runs | Accounted in public summary and ledger F-002, F-004, F-006, F-007, F-008, F-009, F-010, F-011. Local-only raw snapshots preserve grade-A details. |
| Worker-run complexity audit | Accounted in F-002 and commit `750cff7`. |
| Roadmap/matrix drift | Accounted in F-014. |

## Coverage against required classes and keystone failure modes

### Root-cause classes

| Class | Coverage |
|---|---|
| User mistake | No clear pure user-mistake instance observed as of 2026-07. Auth failures are classified as operator/repo setup gaps because the system should surface them truthfully regardless of blame. |
| Repo setup gap | F-002, F-012. |
| Daemon bug | F-004. |
| Design complexity | F-003, F-005, F-006, F-007, F-009, F-013. |
| Workflow-governance/process gap | F-001, F-008, F-014. |

### Keystone failure modes

| Failure mode | Coverage |
|---|---|
| Slowness | No direct evidence that slowness caused self-dogfooding bypass. Long runs in F-011 show 70–90 minute successful sessions, so runtime cost remains an advisory economics signal. |
| Brittleness | F-002, F-004, F-009, F-012. |
| Auth friction | F-002. |
| Worktree setup friction | F-012. |
| Unclear status | F-004, F-006, F-009, F-013, F-014. |
| Missing replan support | F-008, plus F-004/F-009 as queue-truth inputs to the replan RFC. |

## Phase 0 done-when check

- Every scoped run/report/commit is accounted for: **yes**, via tables above.
- Every root-cause class and keystone failure mode has a cited entry or explicit no-instance line: **yes**, via coverage tables above.
- Dogfooding gap has per-commit explanations: **yes**, via scoped commit accounting.
- No reader needs ignored runtime artifacts to understand the evidence: **mostly yes**. Public readers can understand all classes from this ledger and the Phase -1 summary; the operator retains local-only grade-A cross-repo snapshots for verification of pseudonymized details.

## Next phase inputs

Phase 1 should start from these checks:

1. PI-01 cannot be called done until executable tests and installed-daemon behavior demonstrate one-attempt `blocked` auth failures with incidents.
2. PI-02..PI-05 statuses must be reconciled against declared tests, not source presence.
3. Out-of-matrix work must be disposed: worktree setup, profile rework, Pi feed widget, monitor removal, guardrails, and failure forensics.
4. Roadmap status should stop being a manually duplicated source of truth if a structural alternative is feasible.
