# Frontier autonomy RFC

Date: 2026-07-09  
Status: proposed for AF-00; AF-01..AF-06 runtime pieces are implemented incrementally, with later hardening deferred to AF-07+.

This RFC defines the smallest autonomous-frontier behavior allowed by the roadmap. It is an execution-grade design for later slices. The daemon remains the workflow owner. Slice JSON plus daemon/run state remain workflow truth. Frontier behavior only changes who may record a bounded accept decision for one kind of existing replan proposal.

AF-04 implements the shadow recording surface on top of the durable AF-02 envelope and AF-03 pure classifier. `off` runs do not classify. `shadow` runs classify pending typed `add_followup_slice` replan proposals at existing replan checkpoints and persist tier, reason codes, envelope hash, budget snapshot, classified-at, plus a `frontier_classified` event. This is record-only: no decisions, queue mutation, slice writes, proposal channel, or apply path are introduced. AF-06 enables `promote` and `run` to auto-accept only Tier-1 `add_followup_slice` proposals within the envelope and budget.

## Evidence and constraints

Source plan:

- `docs/roadmap/auto-frontier/00-matrix.md`
- `docs/roadmap/auto-frontier/01-doctrine.md`
- Existing replan mechanism in `docs/design/replan-checkpoints.md`
- Existing workflow invariants in `docs/workflow-invariants.md`

Constraints this RFC preserves:

1. The current slice never expands silently.
2. Candidate follow-up slices are recorded as existing `add_followup_slice` replan proposals.
3. There is no separate frontier authority channel, proposal store, decision store, or apply path.
4. The MissionEnvelope bounds only envelope-delegated decision recording. It is not a second plan ledger.
5. One idempotent apply engine handles both operator-accepted and envelope-authorized accepted proposals.
6. Candidate generation is findings-triggered only: worker output, repair output, or operator command at an existing replan checkpoint.
7. Ambiguity fails upward toward queueing or asking, never toward auto-acceptance.

## Decision summary

Introduce a per-run `MissionEnvelope` and a deterministic frontier classifier.

- Workers and repair workers may emit candidate follow-up slice drafts.
- The daemon converts each candidate into a pending `add_followup_slice` replan proposal, with evidence.
- The classifier reads the envelope, slice graph, proposal, and budget counters and returns a tier plus stable reason codes.
- At `off`, proposals wait for the operator exactly as replan v1 does today.
- At `shadow`, the daemon records what the classifier would have done, but mutates no queue, writes no generated slice, and records no decision.
- At `promote`, the daemon may record an envelope-authorized accept for Tier-1 `add_followup_slice` proposals, generate and commit the slice, and leave it open for a future run.
- At `run`, the daemon may record the same bounded accept, generate and commit the slice, append it serially to the current run, and execute it.

The classifier is policy only. It never touches git, state files, IPC, queues, or workers. Applying an accepted proposal is workflow-manager work.

## Terms

- **MissionEnvelope:** a daemon-owned per-run authorization record that bounds what the daemon may auto-accept.
- **Candidate follow-up slice:** a worker/repair/operator-produced draft for new bounded work.
- **Replan proposal:** the existing daemon proposal record. Every candidate follow-up slice becomes an `add_followup_slice` replan proposal before any decision is made.
- **Envelope-authorized accept:** an accepted decision recorded by the daemon because the proposal is Tier 1 and the active envelope delegates that decision.
- **Generated slice:** a slice JSON file written by applying an accepted `add_followup_slice` proposal.
- **Frontier:** the remaining runnable queue reachable from the original queue plus accepted generated follow-up slices. This is a queue view, not a workflow owner.

## MissionEnvelope fields

Illustrative shape for later schema work:

```json
{
  "goal": "Complete the docs-only frontier doctrine workpackage",
  "allowed_areas": ["docs/design/", "docs/workflow-invariants.md"],
  "non_goals": ["runtime behavior changes", "new authority channel"],
  "verify_profile": "default",
  "max_auto_promotions": 2,
  "max_depth": 1,
  "max_generated_slices": 3,
  "autonomy_level": "shadow",
  "must_ask_if": ["candidate changes workflow policy"]
}
```

Fields:

| Field | Meaning | Required validation |
|---|---|---|
| `goal` | Human-readable mission intent for this run. | Non-empty string after trimming. It is context for bounded decisions, not a replacement for slice goals. |
| `allowed_areas` | Repo-relative literal path prefixes the envelope may cover. | Non-empty array. Each entry must pass the slice area contract: no glob characters, no parent traversal, no absolute paths, no leading `./`, no leading/trailing whitespace. Directory prefixes must end in `/`; exact files do not. |
| `non_goals` | Work the operator explicitly excludes from auto-acceptance. | Array of non-empty strings. Deterministic literal/tag matches classify as Tier 3 with `non_goal_overlap`; ambiguous textual overlap also classifies as Tier 3. |
| `verify_profile` | The daemon verification profile the run is expected to use. | Must name a configured profile or the repo default. Candidate follow-ups must include their own `verify` commands; changing this profile is operator-only. |
| `max_auto_promotions` | Maximum accepted decisions the envelope may record. | Integer `>= 0`. `0` means no envelope-authorized accepts. |
| `max_depth` | Maximum generated-slice depth from an original slice. | Integer `>= 0`. Original queued slices are depth `0`; a follow-up from them is depth `1`. |
| `max_generated_slices` | Maximum generated slice files the run may create. | Integer `>= 0`. This caps output even if `max_auto_promotions` is larger. |
| `autonomy_level` | Authority ladder level. | Enum: `off`, `shadow`, `promote`, `run`. Missing envelope or missing level is treated as `off`. |
| `must_ask_if` | Operator stop rules attached to this mission. | Array of non-empty strings. When a candidate or its evidence says one fired, classify Tier 3 with `envelope_must_ask_hit`. Deterministic built-in rules still use closed reason codes. |

Validation failures block or reject the run configuration before worker launch. They do not become classifier inputs for auto-acceptance.

### Area containment rule

A candidate area is inside the envelope only when every declared candidate area is contained by at least one validated `allowed_areas` entry:

- Envelope directory prefix `src/foo/` contains `src/foo/bar.rs` and `src/foo/bar/`.
- Envelope exact file `README.md` contains only `README.md`.
- Envelope `src/foo/` does not contain `src/foo`.
- Envelope `src/foo` does not contain `src/foo/`.
- Envelope `src/foo/` does not contain `src/foobar.rs`.

No path normalization beyond the area contract is allowed. Different trailing-slash forms are not guessed. If containment cannot be decided mechanically, the classifier returns Tier 3 with `area_ambiguous`.

## Autonomy authority ladder

| Level | Decision authority | Side effects |
|---|---|---|
| `off` | Operator only. | Candidate follow-ups become pending replan proposals. No classifier decision is required to continue existing behavior. |
| `shadow` | Operator only. | Classifier tier and reason codes are recorded on proposals and reports as would-have-done data. Queue, slice files, and decisions are unchanged. |
| `promote` | Envelope may accept Tier-1 `add_followup_slice` proposals within budget; all other decisions remain operator-only. | Generated slice JSON is committed with `worker+daemon` provenance but is not appended to or run in the current queue. |
| `run` | Envelope may accept Tier-1 `add_followup_slice` proposals within budget; all other decisions remain operator-only. | Generated slice JSON is committed, appended serially, verified/merged through the existing worker loop, and reported through the same replan/apply surfaces. |

The ladder is one-way for production runs: `off` -> `shadow` -> `promote` -> `run`. No production deployment may skip a level. Operators may lower a level at any time.

### Evidence bar for enabling promote and run

AF-04 must produce the measured evidence. AF-06 or later code may cite the evidence but may not weaken these numbers without amending this RFC and the invariants.

- **Enable `promote`:** over at least **N=10** shadow-classified Tier-1 candidates across at least **M=3** distinct real Khazad runs, operators must have accepted at least **X=80%** unchanged. There must be **zero** Tier-1 classifications that would have crossed the envelope, changed an operator-only field, duplicated a rejected/deferred proposal, lacked required provenance inputs, or missed an envelope `must_ask_if` stop.
- **Enable `run`:** over at least **N=20** shadow-classified Tier-1 candidates across at least **M=5** distinct real Khazad runs, operators must have accepted at least **X=90%** unchanged. There must be **zero** unsafe Tier-1 classifications by the same definition, and at least one prior `promote`-level mission must have produced generated slices with correct provenance and idempotent apply evidence before production `run` is enabled.

Rationale: `promote` is allowed at 80% because it still stops before running generated work. `run` executes new work in the same mission, so it requires more observations, more distinct runs, and higher operator agreement. The zero-unsafe rule is stricter than the acceptance percentage because a false positive across the envelope is an invariant violation, while a false negative only queues or asks.

## Tier semantics and stable reason codes

The classifier returns the highest required tier plus all applicable reason codes. Reason codes are stable machine-readable strings; changing or reusing a reason code requires an RFC amendment or migration note.

| Tier | Name | Meaning | Mutating authority |
|---|---|---|---|
| Tier 0 | attest-inline | The worker already completed a discovery inside the current slice fence. The classifier may attest that no follow-up slice is needed. | None. This is not a proposal accept. |
| Tier 1 | auto-promote | The proposal is a mechanically safe `add_followup_slice` within the envelope and within budget. | After AF-06, `promote`/`run` may record an envelope accept decision. In AF-04 this is measured only. |
| Tier 2 | queue-pending | The proposal is plausible but not safe enough for envelope-authorized acceptance, or the current level is `off`/record-only shadow. | Operator decision through the replan proposal channel. |
| Tier 3 | ask-operator | Continuing automatically would risk intent, authority, policy, or envelope violation. | Operator decision required before this frontier can proceed. |

### Classifier rule order

AF-03 implements this as `src/workflow/frontier.rs` / `promotion_policy::classify_followup_proposal`: a pure classifier over an explicit proposal view, `MissionEnvelope`, slice/proposal graph view, and
`FrontierBudgetState`. It is policy only. AF-04 may call it to record shadow classifications, but the classifier itself creates no proposal, decision, apply, queue, git, IPC, filesystem, worker, or clock authority.

1. If there is no candidate follow-up, return no frontier work. Existing queue behavior continues.
2. If the item is an in-slice discovery with no new runnable work, classify Tier 0.
3. If `autonomy_level` is `off`, classify Tier 2 with `frontier_disabled`.
4. If any stop-rule budget is exhausted, return the matching stop result instead of Tier 1.
5. Apply all Tier-3 rules. Any match makes the result Tier 3.
6. Apply all Tier-2 rules. If no Tier-3 rule matched and any Tier-2 rule matched, return Tier 2.
7. If every Tier-1 positive condition is present, return Tier 1.
8. Otherwise return Tier 3 with `classification_ambiguous`.

### Reason code table

| Code | Tier / result | Rule |
|---|---|---|
| `inline_within_slice_contract` | Tier 0 | The worker's discovery stayed inside the current slice goal, acceptance, areas, and verify authority. |
| `inline_no_new_slice` | Tier 0 | No generated follow-up slice is needed. |
| `frontier_disabled` | Tier 2 | The envelope is absent or `autonomy_level=off`. |
| `shadow_observation_only` | side annotation | `autonomy_level=shadow`; record would-have tier and reason codes without mutation. |
| `inside_allowed_areas` | Tier 1 positive | Every candidate area is contained by `allowed_areas`. |
| `acceptance_present` | Tier 1 positive | Candidate slice has non-empty acceptance criteria. |
| `verify_present` | Tier 1 positive | Candidate slice has non-empty verify commands compatible with the mission verify profile. |
| `within_budget` | Tier 1 positive | `max_auto_promotions` and `max_generated_slices` both have remaining capacity. |
| `within_depth` | Tier 1 positive | Candidate generation depth is `<= max_depth`. |
| `not_duplicate` | Tier 1 positive | Candidate is not a duplicate of an open slice, closed slice, pending proposal, rejected proposal, or deferred proposal. |
| `add_followup_slice_only` | Tier 1 positive | Proposal changes only by adding one follow-up slice. |
| `area_outside_envelope` | Tier 3 | At least one candidate area is not contained by the envelope. |
| `area_ambiguous` | Tier 3 | Area containment cannot be decided mechanically. |
| `non_goal_overlap` | Tier 3 | Candidate work overlaps an envelope non-goal by deterministic tag/literal match or unresolved ambiguity. |
| `candidate_changes_dependencies` | Tier 3 | Candidate adds or changes dependency edges for existing work, or requires a new dependency outside the candidate slice itself. |
| `candidate_changes_acceptance` | Tier 3 | Candidate changes acceptance for existing work instead of adding a follow-up slice. |
| `candidate_changes_verify_profile` | Tier 3 | Candidate changes the run verification profile or verification policy. |
| `candidate_changes_policy_or_schema` | Tier 3 | Candidate changes workflow policy, worker profiles, schemas, or runtime authority. |
| `candidate_hits_must_ask_if` | Tier 3 | The source slice `must_ask_if` fired. |
| `envelope_must_ask_hit` | Tier 3 | The mission envelope `must_ask_if` fired. |
| `operator_only_change_kind` | Tier 3 | The proposal is any kind other than `add_followup_slice`, or contains an operator-only mutation. |
| `duplicate_rejected_or_deferred_proposal` | Tier 3 | Candidate repeats a rejected or deferred proposal without a new operator-provided reconsider condition. |
| `classification_ambiguous` | Tier 3 | Inputs do not match a complete deterministic rule. |
| `frontier_budget_exhausted` | Stop | `max_auto_promotions` or `max_generated_slices` has no remaining capacity. |
| `frontier_depth_exhausted` | Stop | Candidate generation would exceed `max_depth`. |
| `no_frontier` | Stop | No original or generated runnable frontier work remains. |
| `cancel_requested` | Stop | Operator cancellation was requested. |
| `replan_apply_incomplete` | Stop | Accepted proposal application was interrupted before the post-apply checkpoint. |
| `candidate_missing_acceptance` | Tier 2 | Candidate lacks sufficient acceptance criteria. |
| `candidate_missing_verify` | Tier 2 | Candidate lacks verify commands. |
| `duplicate_open_slice` | Tier 2 | Candidate duplicates an open slice; operator can reject or merge intent through replan. |
| `duplicate_closed_slice` | Tier 2 | Candidate duplicates completed work; operator can reject or request a different follow-up. |
| `duplicate_pending_proposal` | Tier 2 | Candidate duplicates an undecided proposal. |
| `proposal_needs_operator_context` | Tier 2 | Candidate is inside the envelope but needs human rationale before acceptance. |

AF-03's minimum test table is fully determined by this table:

| Scenario | Expected result |
|---|---|
| Inside envelope, acceptance present, verify present, depth and budgets available, not duplicate | Tier 1 with positive codes. |
| Candidate area outside `allowed_areas` | Tier 3 `area_outside_envelope`. |
| Candidate changes dependency edges for existing work | Tier 3 `candidate_changes_dependencies`. |
| Source or envelope `must_ask_if` fired | Tier 3 `candidate_hits_must_ask_if` or `envelope_must_ask_hit`. |
| Candidate lacks verify commands | Tier 2 `candidate_missing_verify`. |
| Duplicate open slice | Tier 2 `duplicate_open_slice`. |
| Duplicate closed slice | Tier 2 `duplicate_closed_slice`. |
| Duplicate rejected or deferred proposal | Tier 3 `duplicate_rejected_or_deferred_proposal`. |
| Budget or depth exhausted | Stop with `frontier_budget_exhausted` or `frontier_depth_exhausted`; no auto-accept. |
| Non-goal overlap | Tier 3 `non_goal_overlap`. |

## Stop rules

Stop rules are frontier outcomes, not new `RunStatus` or `SliceStatus` values.

| Stop rule | Condition | Required behavior |
|---|---|---|
| `frontier_budget_exhausted` | `max_auto_promotions` or `max_generated_slices` has no remaining capacity when a Tier-1 candidate appears. | Do not auto-accept. Leave proposal pending and surface existing `awaiting_replan` attention if operator action can continue the run; otherwise record a terminal summary reason that no authorized frontier budget remains. |
| `frontier_depth_exhausted` | Candidate generation would exceed `max_depth`. | Do not auto-accept. Queue/ask through replan with reason code `frontier_depth_exhausted`. |
| `tier3_pending` | Any Tier-3 reason code is present. | Pause at existing `awaiting_replan` pending-attention state with exact accept/reject/defer commands. |
| `no_frontier` | Original queue is complete and there are no pending or accepted generated slices to run. | Complete normally using existing terminal completion behavior. |
| `cancel_requested` | Operator cancels the run. | Use existing cancellation flow and cleanup; frontier state is evidence only. |
| `replan_apply_incomplete` | Accepted proposal application was interrupted before the post-apply checkpoint. | Use the existing replan interrupted/apply-incomplete blocked path; never launch workers against an unknown queue. |

Tier 2 is not a stop by itself. It leaves a normal pending replan proposal for the operator while the daemon follows existing checkpoint behavior.

## Replan proposal, decision, and apply channel

All follow-up candidates use the existing replan proposal/decision/apply channel:

1. Candidate emitted by worker, repair worker, or operator.
2. Daemon validates the draft shape and records a pending `add_followup_slice` replan proposal with evidence. Worker and repair-worker proposals carry the full typed `FollowupSliceDraft` payload on the proposed change; invalid drafts are warning findings, not a second workflow channel.
3. Classifier records a tier and reason codes on that proposal.
4. Operator or envelope-authorized policy records a decision.
5. The single idempotent replan apply engine writes generated slice JSON, commits it, updates queue snapshots, and records checkpoints.

No separate frontier authority channel exists. There is no second proposal store, no second decision model, and no second apply engine. A frontier decision is just a replan decision with a bounded authorizer.

Rejected and deferred proposals are never auto-reapplied. A later proposal may cite the earlier decision only if the operator's revisit condition fired or new evidence exists; the classifier must otherwise return Tier 3 with `duplicate_rejected_or_deferred_proposal`.

## Provenance requirements

Every generated follow-up slice must carry provenance in its slice JSON before any worker can see it:

```json
{
  "provenance": {
    "parent_slice_id": "S-1",
    "origin_proposal_id": "rp-20260709-001",
    "generation": 1,
    "created_by": "operator:sivanirosh",
    "created_at": "2026-07-09T15:00:00Z"
  }
}
```

Required provenance fields:

| Field | Meaning |
|---|---|
| `parent_slice_id` | Slice whose worker/repair output produced the candidate. |
| `origin_proposal_id` | Replan proposal id that authorized the generated slice. |
| `generation` | Depth from original queued work; original slices are depth `0`, first follow-ups are depth `1`. |
| `created_by` | Slice-file creator class: `operator` or `worker+daemon`. Decision authorizer remains on the replan decision record. |
| `created_at` | Daemon timestamp when the slice file was created. |

Promotion decisions must also record:

- `authorizer` (`operator:<id>` or `envelope:<run_id>`);
- `source` (`operator` or `frontier_policy`);
- classifier tier;
- stable reason codes;
- before/after queue snapshot hashes;
- apply checkpoint ids;
- budget counters before and after the decision.

Reports and handoffs derive the promotion graph from proposal, decision, queue, and slice provenance records. They must not become a separate truth store.

## Deferral reopenings

This RFC reopens exactly three Phase-2 deferrals and leaves the ambient-planner rejection closed.

### Runtime mission object

Recorded reconsider condition: "recorded slice revisions cannot express the operator's durable intent".

Evidence that fires it: auto-promotion must bound not-yet-existing slices by allowed areas, non-goals, budgets, max depth, generated-slice count, autonomy level, and mission-specific `must_ask_if` rules. A slice revision can describe known work, but cannot bound future candidates before they exist. The MissionEnvelope is therefore justified as a per-run authorization envelope.

Limit: the envelope is not workflow truth, not a queue ledger, and not a standing cross-run mission. Slice JSON plus daemon/run state remain authoritative.

### Automated planner authority to mutate queues

Recorded reconsider condition: "production evidence that manual approval is the bottleneck and accepted changes are mechanically safe".

Evidence required to fire it: AF-04 shadow-mode measurements must satisfy the promote/run evidence bars in this RFC. Until those bars are met, the auto-approvable tier remains empty and all candidate follow-ups stay operator-authorized.

Limit: the only auto-accepted change kind is `add_followup_slice` inside the envelope. Areas, acceptance, verification profile, dependencies for existing work, policy, schema, and runtime behavior stay operator-only.

### Daemon-internal autonomous replan engine

Recorded reconsider condition: "repeated recorded findings show a mechanical replan pattern that humans approve unchanged".

Evidence required to fire it: candidates must come from recorded worker/repair/operator findings, and shadow data must show repeated unchanged operator acceptance for Tier-1 candidates. The daemon may classify and decide only that mechanical pattern.

Limit: the daemon still does not invent work. There is no ambient planner pass, no daemon self-prompting for ideas, and no LLM critic in the authorization path.

### Ambient planner rejection remains closed

The roadmap's findings-triggered rule stands. Frontier autonomy does not authorize a daemon planner to search for possible work when no finding or candidate exists.

## Data model, API, UI, migration, and permissions

AF-00 makes no runtime, schema, API, CLI, UI, JavaScript, Rust, or script changes.

Later slices own implementation:

- AF-01: candidate follow-up slice drafts through the replan channel.
- AF-02: durable MissionEnvelope record and budget counters.
- AF-03: pure classifier.
- AF-04: shadow recording and measurements.
- AF-05: idempotent apply engine for accepted follow-up slices.
- AF-06: envelope-delegated auto-acceptance for `promote` and `run`.
- AF-07: report/handoff attestation.
- AF-08: dogfood evidence.

Permissions remain unchanged until those slices land. Acceptance of this RFC is only the doctrine gate.

## Done-when and review checklist

AF-00 is done when:

- This RFC exists and defines MissionEnvelope fields and validation.
- Every tier rule has a stable reason code.
- The promote/run evidence bars name explicit N/M/X values.
- Stop rules map to existing pending-attention states or terminal reason evidence without new run/slice statuses.
- Provenance requirements are explicit for generated slices, promotion decisions, reports, and handoffs.
- Each reopened Phase-2 deferral quotes its reconsider condition.
- `docs/workflow-invariants.md` contains proposed Phase-style amendment records for mission envelopes, envelope-delegated auto-acceptance, and generated follow-up slice provenance.
- Roadmap truth lint still passes for `docs/roadmap/auto-frontier/00-matrix.md`.

Workflow acceptance test for later implementation review:

```text
1. Reviewer maps the ten AF-03 scenarios in this RFC to expected tier + reason codes.
2. AF-03's classifier tests match those expected outputs 10/10.
3. Any scenario not covered unambiguously is classified Tier 3 until this RFC is amended.
4. No actor gains apply authority from this RFC; apply authority remains with the daemon's replan apply engine.
5. No separate frontier authority channel appears in code, state, status, reports, or docs.
```
