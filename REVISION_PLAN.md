# Khazad-Doom Revision Plan

We are pausing feature work and product implementation work on Khazad-Doom.

The current codebase is a strong first prototype: it proves the core idea, exposes real daemon/worker/Pi failure modes, and gives us enough evidence to redesign deliberately. The next phase is not to add more features. The next phase is to revise the system from observed failures.

## Immediate rule

Do not continue product implementation work until the evidence ledger, invariant diff, and next redesign slices are agreed.

Allowed during the freeze:

- urgent operational fixes needed to prevent damage or runaway runs;
- documentation/evidence preservation;
- roadmap truth reconciliation;
- architecture review and redesign planning.

Any exception must be recorded with a reason. If work bypasses Khazad-Doom, that bypass itself is evidence.

## Governing method

For each observed failure, decide whether:

1. the existing invariants already cover it and the implementation failed the invariant;
2. the invariant exists but is not mechanically enforced or tested;
3. one small invariant/mechanism is missing;
4. the idea is speculative and should be explicitly deferred.

No new mechanism enters the redesign without observed evidence. Preventive doctrine is allowed only for trust/safety boundaries, and must be labeled as preventive rather than evidence-driven.

Use `docs/design/worker-run-complexity-audit.md` as the house template: observed failure, design lens, recommendation, scoped implementation slice, tests, considered alternatives, and reconsider-only-if conditions.

## Keystone finding: dogfooding stopped

The first ledger entry must be the dogfooding gap.

The last commit visibly produced through Khazad-Doom was `2a6fc7c` (`slice-041`, 2026-06-26). Subsequent commits, including incident surfacing, failure forensics, guardrails, Pi-native implementation, profile rework, and worktree setup, were made by hand even though the Pi-native matrix says workpackages should be converted into JSON Issue Slices for dogfooding.

This reframes roadmap drift as a symptom, not the root failure. Matrix rows became stale because the workflow that closes slices and records runs stopped being exercised.

The failure ledger must answer, for each post-2026-06-26 commit in scope: why did this not go through a Khazad-Doom run? Each answer is either a legitimate exemption or an observed workflow failure such as slowness, brittleness, auth friction, worktree setup friction, unclear status, or missing replan support.

The revision succeeds only when redesign slices execute through Khazad-Doom again. The future commit log should make workflow governance visible again: slice branches, integration commits, run reports, and closed slice records.

## Phase -1 — Preserve transient evidence now

Harvest evidence before ignored runtime artifacts disappear.

Create committed evidence summaries under `docs/design/evidence/`. Do not rely on `.workflow/runs/` remaining present; those artifacts are transient by invariant.

Sources:

- surviving `.workflow/runs/` directories;
- committed `.workflow/reports/` summaries;
- `docs/design/worker-run-complexity-audit.md`;
- relevant git history after `2a6fc7c`;
- pending memory candidates only when they clarify already-observed events.

Preserve, at minimum:

- run ids and terminal states;
- incidents and blocked/failed reasons;
- worker attempt counts;
- repair attempts;
- verification/gate outcomes;
- worktree/setup errors;
- status/monitor confusion if recorded;
- whether the run produced commits or was bypassed.

Done when:

- every surviving run directory has a committed summary or an explicit "not useful/no data" note;
- the existing audit is linked as processed evidence;
- the evidence can be reviewed without access to `.workflow/runs/`.

## Phase 0 — Failure ledger

Produce `docs/design/failure-ledger.md`.

Scope is intentionally capped to avoid archaeology drift:

1. the existing worker-run complexity audit;
2. the surviving June 26 run directories and reports;
3. commits after `2a6fc7c` up to and including the freeze boundary `3d640c6` (2026-07-06), especially hand-made fixes and Pi-native work; commits made after the boundary under the freeze-exception rule join the scope through their recorded exception;
4. the roadmap/matrix drift itself;
5. the 2026-07-06 daemon state-store snapshot across all recorded repositories as grade-A evidence, bounded to the snapshot artifacts under `docs/design/evidence/raw/` (cross-repo `state-*` snapshots are local-only/gitignored for privacy on this public repo; the committed summary `docs/design/evidence/run-evidence-2026-07-06.md` is the public record and uses the local pseudonym lookup).

Ledger entry format:

```text
ID
Evidence grade
Source: run id / report / audit / commit / operator note
Symptom
Root-cause class
Invariant involved: caught / failed / missing / not applicable
Current regression coverage
Remaining design gap
Disposition
```

Evidence grades:

- **A** — raw run artifact, final report, incident, or committed evidence summary;
- **B** — written audit with cited run/commit evidence;
- **C** — reconstruction from commit diff and commit message;
- **D** — operator recollection or narrative note.

Root-cause classes:

- user mistake;
- repo setup gap;
- daemon bug;
- design complexity;
- workflow-governance/process gap for meta-failures such as stopped dogfooding or stale roadmap state.

Rules:

- no hypothetical failures in the ledger;
- a category with no observed instance gets an explicit "no observed instance as of 2026-07" line;
- ledger entry #1 is the dogfooding gap;
- every post-`2a6fc7c` scoped commit is classified as Khazad-run, legitimate exemption, or bypass/failure.

Done when:

- every scoped run/report/commit is accounted for;
- every root-cause class and every keystone failure mode (slowness, brittleness, auth friction, worktree setup friction, unclear status, missing replan support) has at least one cited entry or an explicit no-instance line;
- the dogfooding gap has per-commit explanations;
- no reader needs ignored runtime artifacts to understand the evidence.

## Phase 1 — Roadmap truth audit

Reconcile `docs/roadmap/pi-native/00-matrix.md` and related workpackages with reality.

Rules:

- A row is `done` only if its own success criteria, Required Tests, and docs are satisfied.
- Existing code is not enough. Run the row's declared tests where they are executable: parser tolerance for PI-02, profile precedence for PI-03, operator question lifecycle for PI-04, projection snapshots/parity for PI-05, and so on. Inspection alone is acceptable only for doc-only criteria such as PI-00's.
- There is no "mostly done" state. Split remaining work into new rows or mark explicitly deferred with rationale and revisit condition.
- Work completed outside any slice/matrix row must receive a disposition: retroactively documented as evidence, converted into a follow-up row, or marked as a legitimate exemption.

Structural truthfulness amendment to evaluate:

> Live roadmap status has one source of truth: slice JSON and daemon/run state. Roadmap documents may summarize or reference that state, but must not become a competing status ledger.

If accepted, enforce this with a doc-lint, slice-close check, or generated matrix status instead of relying on discipline.

Done when:

- PI-00..PI-05 statuses match their tests and implemented behavior;
- stale rows are split, corrected, or explicitly deferred;
- work done outside Khazad-Doom has a recorded disposition;
- the matrix no longer claims a state that slice/run evidence contradicts.

## Phase 2 — Doctrine diff, not a parallel doctrine doc

Do not create a fresh doctrine document that duplicates `docs/workflow-invariants.md`. Amend the invariants only where the ledger proves a gap.

Every proposed amendment must include:

```text
Proposed invariant text
Ledger entries it answers
Enforcement mechanism
Test that would detect violation
Status: accepted / rejected / explicitly_deferred
```

Pre-registered hypotheses, not conclusions:

- **Likely to survive if evidence supports it:** recorded plan revisions; hard disposition for findings; structural roadmap/status truthfulness; advisory complexity telemetry (daemon-computed diff/dependency/module deltas surfaced to reviewers and run reports as indicators, never as automatic verdicts — evidence basis to test: complexity findings so far have required episodic manual audits).
- **Likely to defer unless evidence proves otherwise:** runtime mission object; daemon-internal replan engine; automated planner authority to mutate queues; complexity telemetry that blocks work automatically.

Finding disposition rule to evaluate:

> Every finding must reach exactly one terminal disposition: answered, folded into a slice, explicitly deferred with revisit condition, or rejected with rationale.

Done when:

- every accepted invariant amendment cites ledger evidence, enforcement, and tests;
- every rejected/deferred idea has a reconsider condition;
- no new doctrine duplicates an existing invariant in weaker language;
- preventive trust/safety rules are clearly labeled as preventive.

## Phase 3 — Replan checkpoint RFC

Write one focused decision record: `docs/design/replan-checkpoints.md`.

This RFC exists because the genuine new delta appears to be controlled adaptation of the remaining slice queue. It must not become a broad adaptive-workflow doctrine doc.

Questions it must answer:

- What triggers replan evaluation? Recommended cheap default: findings-triggered only; no finding/proposed follow-up means no replan pass.
- When may the plan change: only at checkpoints, terminal blocked states, or both?
- Who may propose changes?
- Who may authorize changes?
- What can a planner propose without human intent changes?
- What always requires operator approval: expanded areas, changed acceptance, changed verification, deleted slices, changed mission/goal.
- What happens when a proposal is rejected?
- How is every queue change recorded: what changed, why, who/what authorized it, and which evidence caused it?
- How does resume behave if the daemon restarts mid-replan?
- How do status/watch/monitor render pending, accepted, rejected, and deferred replan state?
- How does handoff attest the queue history?

Default stance until evidence demands more:

- planner agents may propose changes, not apply them;
- daemon validates proposals mechanically;
- every applied change requires operator approval until the RFC defines the auto-approvable tier; intent-affecting changes (expanded areas, changed acceptance, changed verification, deleted slices, changed mission) always remain operator-approved;
- defining that tier boundary is the RFC's central decision — it is what lets routine adaptations stop requiring manual prompting without surrendering intent control;
- queue mutation is explicit and recorded;
- silent `.workflow/slices/` edits during a run are not considered a valid replan mechanism.

The RFC must terminate. It ends by producing invariant amendments, matrix-style redesign slices, and explicit deferrals; then it is marked decided.

Done when:

- the RFC is accepted, rejected, or explicitly deferred;
- accepted outcomes are translated into invariant diffs and redesign slice rows;
- open questions are not left as ambient design debt;
- status/rendering and resume behavior are specified for every accepted replan state.

## Phase 4 — Architecture review

Produce a written architecture review before product implementation resumes.

The review may only cite pressure points that appear in the failure ledger or truth audit. It should map each pressure point to the governing method:

- already covered by invariants, implementation failed;
- invariant exists but lacks enforcement/test;
- one small mechanism missing;
- explicitly deferred.

Review targets:

- shallow modules and low-leverage interfaces;
- duplicated interpretation;
- unclear lifecycle ownership;
- hidden state transitions;
- status/projection drift;
- worker/profile/Pi contract coupling;
- worktree/setup and resume reliability;
- runtime economics and repair behavior;
- dogfooding friction.

Preserve deep modules where they are earning their keep. Prefer deepening existing seams over introducing new ones. The review must explicitly justify any new module by the behavior it hides and the tests it enables.

Done when:

- every named pressure point cites ledger or truth-audit evidence;
- every proposed seam has a smaller interface than the behavior it hides;
- rejected alternatives have reconsider conditions;
- the review produces a short ordered list of redesign slices.

## Phase 5 — Redesign slices and dogfood recovery

Turn the architecture review into a small sequence of redesign slices.

Each slice must use matrix-row form:

```text
Slice ID
Evidence entries addressed
Files/modules likely touched
Success criteria
Required tests
Status
Explicit deferrals
Dogfood/run plan
```

Rules:

- prefer deletions, simplification, and clearer boundaries over new capability;
- no broad rewrites;
- no compatibility layers without evidence;
- no hidden states;
- every implementation slice should be executed through Khazad-Doom unless explicitly exempted with rationale.

Dogfooding is a release criterion, not a nice-to-have. The first redesign implementation slice should prove the revised workflow path itself: JSON slice, daemon-run execution, verification, integration, report, and closed slice state.

Done when:

- each redesign slice has evidence-backed acceptance criteria and tests;
- the first implementation slice is run through Khazad-Doom or has a documented emergency exemption;
- run artifacts and commits make the workflow visible again;
- the dogfooding gap is closed by observed behavior, not by promise.

## Phase 5 scope extension — Herdr cockpit

After PUB-01/PUB-01A/PUB-01B proved publication truth, the live-observability scope is expanded from a Pi attach/feed workaround to a Herdr cockpit model.

Evidence basis: F-013 and the Phase 1 PI-05 audit show status/monitor drift and Pi UI churn. This admits Herdr work as evidence-backed, not speculative dashboard building, but only if it reduces duplicated interpretation and preserves daemon ownership.

Target responsibility split:

```text
Pi:
  start, shape, explain, answer blockers, summarize handoff

Khazad-Doom:
  own run truth, slices, worker authorization, gates, blockers, merge, handoff

Herdr:
  show/focus live agents and run workspaces
```

Accepted scope decisions:

- Herdr is the optional-default live cockpit when `herdr` is available; direct Pi execution remains the default fallback when Herdr is missing or cockpit startup fails.
- Khazad-Doom launches workers via Herdr, not vice versa: KD resolves the slice, prompt, profile, env, token, and command; Herdr provides visible panes.
- Worker results are captured through KD-owned wrapper artifacts under the run directory, not by parsing Herdr terminal scrollback or Herdr agent-status metadata.
- Operator control in Herdr means observe, focus, and request KD-owned cancel/answer actions. Normal worker panes are not an interactive authority channel.
- The gate/repair pane is read-only initially; KD continues to execute gate/repair commands and project their status through the daemon feed.
- The Planner Pi pane is deferred until RPL slices define planner proposal authority.
- The existing Pi extension becomes a thin bridge: start/explain/answer/summarize/open Herdr cockpit; it does not emulate a live multi-agent dashboard.
- Real Herdr smoke/e2e evidence is required for Herdr slices, but normal `cargo test` must remain portable when Herdr is not installed; Herdr-specific checks are explicitly gated.

Revised Phase 5 order after publication truth:

1. FEED-01 — terminal reason and projection authority.
2. HERDR-01 — cockpit contract, config, default workspace, read-only feed/phase panes.
3. HERDR-02 — Herdr worker panes with KD-owned wrapper/result capture and direct fallback.
4. HERDR-03 — Pi bridge opens/focuses Herdr cockpit and stays a painter/explainer.
5. RPL-01, RPL-02, RPL-03 — replan proposal/store/disposition/history.
6. PI-PROOF-01 — Pi-native acceptance evidence closure.

## Plan-level completion criteria

The revision plan is complete when:

- transient evidence has been promoted into committed summaries;
- `docs/design/failure-ledger.md` accounts for scoped runs, reports, audits, and post-`2a6fc7c` commits;
- roadmap status matches slice/run evidence and declared tests;
- every invariant amendment is evidence-backed, enforceable, and testable;
- every rejected or deferred mechanism has a reconsider condition;
- replan checkpoint behavior is decided or explicitly deferred;
- the architecture review produces a bounded redesign sequence;
- redesign implementation resumes through Khazad-Doom itself.
