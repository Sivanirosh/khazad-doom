# AF-08 autonomous frontier dogfood evidence (2026-07-09)

This file records the real-Pi dogfood proof for slice AF-08. The runs used the local daemon binary at `target/debug/khazad-doom`, `--agent pi`, `--cockpit direct`, `--json-wrapper-worker`, an isolated daemon home at `/tmp/kd-af08-dogfood-home-20260709`, and repo-local run artifacts under `.workflow/runs/`.

Temporary dogfood setup slices and support config were committed only while each proof run was launched, then reset before the AF-08 evidence commit. They are not intended product changes.

## Evidence index

| Stage | Run id | Key artifact(s) | Result |
|---|---|---|---|
| A: shadow/manual accept | `kd-20260709-205426-cbd36dbb` | `.workflow/runs/kd-20260709-205426-cbd36dbb/outputs/af08-stage-a-awaiting-replan-status.json`, `af08-stage-a-final-status.json`, `af08-stage-a-handoff-dry-run.json`, `final-report.json` | Shadow mode recorded a Tier-1 proposal, blocked for manual agreement, accepted via operator decision, appended and ran the follow-up. |
| B: run auto-promotion/execution/publication | `kd-20260709-205900-746346cd` | `.workflow/runs/kd-20260709-205900-746346cd/outputs/af08-stage-b-final-status.json`, `af08-stage-b-handoff-dry-run.json`, `final-report.json` | Run mode auto-accepted one Tier-1 proposal, appended the generated slice, executed it, and produced handoff/report evidence. |
| C: Tier-3 stop/RW-2 notification/operator decision | `kd-20260709-210214-a95ba257` | `.workflow/runs/kd-20260709-210214-a95ba257/outputs/af08-stage-c-blocked-tier3-status.json`, `af08-stage-c-operator-accept.json`, `af08-stage-c-final-status.json` | Out-of-envelope proposal stopped before queue mutation and notified the origin; operator then explicitly accepted and resumed. |
| D: duplicate-closed rerun guard | `kd-20260709-210915-9d9232e6` | `.workflow/runs/kd-20260709-210915-9d9232e6/outputs/af08-stage-e-duplicate-closed-blocked-status.json`, `af08-stage-e-operator-leave-blocked.json` | A unique-id proposal whose goal matched a closed slice was classified `duplicate_closed_slice`, did not auto-promote, and remained blocked by operator choice. |

## Stage A — shadow/manual accept

Start command pattern:

```bash
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom run \
  --repo . --slice AF08-DOGFOOD-MISSION-A --agent pi --cockpit direct \
  --json-wrapper-worker --envelope /tmp/af08-stage-a-shadow-envelope.json \
  --origin-notification-target kd-tui-kd-20260709-154914-9408be43-AF-08-attempt-1
```

Daemon state before the manual decision (`af08-stage-a-awaiting-replan-status.json`):

```json
{
  "run_id": "kd-20260709-205426-cbd36dbb",
  "status": "blocked",
  "proposal_id": "rp-20260709-001",
  "frontier_classification": {
    "tier": "tier_1",
    "autonomy_level": "shadow",
    "reason_codes": [
      "shadow_observation_only",
      "add_followup_slice_only",
      "within_budget",
      "within_depth",
      "inside_allowed_areas",
      "not_duplicate",
      "acceptance_present",
      "verify_present"
    ]
  },
  "record_only": true,
  "queue_mutated": false,
  "slice_mutated": false
}
```

Operator decision was requested with `ask_operator`. The operator answered `accept-and-resume`, and the exact command executed was:

```bash
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom \
  replan accept kd-20260709-205426-cbd36dbb rp-20260709-001 \
  --reason "AF-08 Stage A operator agreement: Tier-1 shadow candidate accepted unchanged"
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom \
  resume --run kd-20260709-205426-cbd36dbb --agent pi --cockpit direct --json-wrapper-worker
```

Final daemon state (`af08-stage-a-final-status.json`):

- Run status: `completed`.
- Selected slices: `AF08-DOGFOOD-MISSION-A,AF08-DOGFOOD-FOLLOWUP-A`.
- Slice commits: mission `957fc2a3a2cfb3b5404255fc5c22cfa4c928bdb8`, follow-up `2d2fc54d2d8e1e5cee1b7c30f2d071661b71586d`.
- Decision source: `cli`; authorizer: `sivanirosh`.
- Generated slice commit: `06a339f4e1d109a417611692a644e876c3d57143`.
- Queue changed from `["AF08-DOGFOOD-MISSION-A"]` to `["AF08-DOGFOOD-MISSION-A", "AF08-DOGFOOD-FOLLOWUP-A"]` only after the operator decision.
- `final-report.json` / `af08-stage-a-handoff-dry-run.json` include `plan_revisions.frontier.summary_line = "frontier activity: candidates_seen=1, generated_slices=1, pending_deferred_rejected=0, operator_stops=0, tier_1_would_promote=1, agreement=1/1 (100%)"`.
- The report/handoff `generated_slice_graph[0]` records parent `AF08-DOGFOOD-MISSION-A`, child `AF08-DOGFOOD-FOLLOWUP-A`, origin proposal `rp-20260709-001`, authorizer `sivanirosh`, decision source `cli`, status `merged`, and commit `06a339f4e1d109a417611692a644e876c3d57143`.

## Stage B — run auto-promotion, execution, and publication

Start command pattern:

```bash
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom run \
  --repo . --slice AF08-DOGFOOD-MISSION-B --agent pi --cockpit direct \
  --json-wrapper-worker --envelope /tmp/af08-stage-b-run-envelope.json \
  --origin-notification-target kd-tui-kd-20260709-154914-9408be43-AF-08-attempt-1
```

Daemon state (`af08-stage-b-final-status.json`) shows proposal `rp-20260709-002` was auto-accepted by frontier policy:

```json
{
  "proposal_id": "rp-20260709-002",
  "frontier_classification": {
    "tier": "tier_1",
    "autonomy_level": "run",
    "reason_codes": [
      "add_followup_slice_only",
      "within_budget",
      "within_depth",
      "inside_allowed_areas",
      "not_duplicate",
      "acceptance_present",
      "verify_present"
    ]
  },
  "operator_decision": {
    "decision": "accepted",
    "source": "frontier_policy",
    "authorizer": "envelope:kd-20260709-205900-746346cd",
    "frontier_budget_before": { "auto_promotions_used": 0, "generated_slices": 0 },
    "frontier_budget_after": { "auto_promotions_used": 1, "generated_slices": 1 },
    "generated_slice_id": "AF08-DOGFOOD-FOLLOWUP-B"
  }
}
```

Final state:

- Run status: `completed`.
- Selected slices: `AF08-DOGFOOD-MISSION-B,AF08-DOGFOOD-FOLLOWUP-B`.
- Slice commits: mission `5e955fd10808e670f8d32939054ba2e2bed0218f`, follow-up `a40dbc02dbb0e3a9263cdef8fc8e101336da3f8e`.
- Generated slice commit: `2f96c7150f607aa5e5477ae8830bf328ec024934`.
- `frontier_slice_promoted` recorded `worker_enqueued: true`, `serial_append: true`, and queue change from `["AF08-DOGFOOD-MISSION-B"]` to `["AF08-DOGFOOD-MISSION-B", "AF08-DOGFOOD-FOLLOWUP-B"]`.
- `final-report.json` and `af08-stage-b-handoff-dry-run.json` were produced and both expose the derived publication truth at `plan_revisions.frontier`.
- The report/handoff promotion graph contains `generated_slice_graph[0] = { parent_slice_id: "AF08-DOGFOOD-MISSION-B", child_slice_id: "AF08-DOGFOOD-FOLLOWUP-B", origin_proposal_id: "rp-20260709-002", authorizer: "envelope:kd-20260709-205900-746346cd", decision_source: "frontier_policy", tier: "tier_1", status: "merged", commit_sha: "2f96c7150f607aa5e5477ae8830bf328ec024934" }`.
- `plan_revisions.frontier.budget_consumption` records `auto_promotions_used: 1`, `generated_slices: 1`, `max_generated_slices: 1`, and `max_depth_reached: 1`; `agreement_ratio` is `1/1`.

## Stage C — Tier-3/RW-2 stop and operator decision

The Stage C envelope allowed only `docs/roadmap/auto-frontier/09-dogfood.md`. The real Pi worker proposed a follow-up for `docs/design/frontier-autonomy.md`, deliberately outside the envelope.

Blocked daemon state before any operator decision (`af08-stage-c-blocked-tier3-status.json`):

```json
{
  "run_id": "kd-20260709-210214-a95ba257",
  "status": "blocked",
  "proposal_id": "rp-20260709-003",
  "frontier_classified_event": {
    "tier": "tier_3",
    "autonomy_level": "run",
    "reason_codes": [
      "add_followup_slice_only",
      "within_budget",
      "within_depth",
      "area_outside_envelope",
      "not_duplicate",
      "acceptance_present",
      "verify_present"
    ],
    "queue_mutated": false,
    "slice_mutated": false,
    "decision_recorded": false
  },
  "attention_events": ["attention_notification_sent", "attention_focus_sent"]
}
```

The notification records used adapter `herdr`, kind `replan_decision_pending`, and proposal id `rp-20260709-003`. This is the RW-2/operator-review path: the daemon stopped before adding the generated slice.

The operator asked for the proposal to be accepted after explanation. The exact command executed was:

```bash
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom \
  replan accept kd-20260709-210214-a95ba257 rp-20260709-003 \
  --reason "AF-08 Stage C operator decision: accept Tier-3 out-of-envelope follow-up explicitly after daemon stop"
KHAZAD_HOME=/tmp/kd-af08-dogfood-home-20260709 target/debug/khazad-doom \
  resume --run kd-20260709-210214-a95ba257 --agent pi --cockpit direct --json-wrapper-worker
```

Final state (`af08-stage-c-final-status.json`):

- Run status: `completed`.
- Decision source: `cli`; authorizer: `sivanirosh`.
- Generated slice `AF08-DOGFOOD-FOLLOWUP-C` was appended only after the explicit operator decision.
- Slice commits: mission `06be17e5747ef54f221c76e09fa5f9af35196a1c`, follow-up `d5c01661ecad430c6cc9f12e9041808f6356c5be`.

## Stage D — duplicate-closed rerun guard

The duplicate-closed proof seeded a closed sentinel slice with goal:

```text
Update docs/design/frontier-autonomy.md with the AF-08 duplicate-closed rerun guard sentinel cross-reference.
```

The real Pi worker emitted candidate `AF08-DOGFOOD-DUP-RERUN` with a unique id but the same normalized goal. This avoids worker-output schema rejection for duplicate ids and exercises the frontier classifier's closed-slice duplicate guard.

Blocked daemon state (`af08-stage-e-duplicate-closed-blocked-status.json`):

```json
{
  "run_id": "kd-20260709-210915-9d9232e6",
  "status": "blocked",
  "proposal_id": "rp-20260709-004",
  "frontier_classification": {
    "tier": "tier_2",
    "autonomy_level": "run",
    "reason_codes": [
      "add_followup_slice_only",
      "within_budget",
      "within_depth",
      "inside_allowed_areas",
      "duplicate_closed_slice",
      "acceptance_present",
      "verify_present"
    ]
  },
  "queue_mutated": false,
  "slice_mutated": false
}
```

The daemon also emitted `attention_notification_sent` and `attention_focus_sent` for proposal `rp-20260709-004`. The operator selected `leave-blocked`, so no replan command was executed and no duplicate follow-up was appended. As expected, `handoff --dry-run` returned exit code `1` with:

```text
khazad-doom: run "kd-20260709-210915-9d9232e6" is blocked; handoff requires completed
```

A setup-only exact-id duplicate attempt (`kd-20260709-210735-f42a1606`) also showed the JSON wrapper rejecting an exact duplicate id before replan with finding text `duplicate slice id "AF08-DOGFOOD-DUP-CLOSED" appears 2 times`; the primary guard evidence is the unique-id/same-goal run above because it reaches `duplicate_closed_slice` classification.

## Conclusion

AF-08's staged proof exercised the intended autonomous frontier behavior with real Pi workers:

1. Shadow mode records the proposal and blocks for manual agreement before mutating the queue.
2. Run mode can auto-promote one Tier-1 in-envelope generated slice, append it serially, execute it, and publish report/handoff evidence.
3. Tier-3/RW-2 review stops before queue/slice mutation and notifies the origin; subsequent mutation requires an explicit operator decision.
4. Duplicate closed work is recognized as `duplicate_closed_slice`, does not auto-promote, and can remain blocked by operator choice.
