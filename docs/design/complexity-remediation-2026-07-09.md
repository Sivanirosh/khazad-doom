# Complexity-remediation closure

Date: 2026-07-11

Scope: `ASK-FALLBACK-01`, then `CA-01` through `CA-09`

## Outcome

The program kept the daemon as workflow owner and hardened the existing seams instead of replacing them. `ASK-FALLBACK-01` established the operator-authorized bounded fallback: a worker may supply one exact-option recommendation with rationale and explicit bounded/reversible attestations; the daemon alone resolves the 60-second operator-answer race and records the durable source. Missing, invalid, hard-authorization, stale, interrupted, or out-of-scope cases still block.

CA-01 through CA-09 preserve that authority boundary while closing the nine remediation steps:

| Step | Slice | Closed gap |
|---:|---|---|
| 1 | CA-01 | Verification is observationally pure; completion publication stages only an explicit daemon-owned manifest. |
| 2 | CA-02 | Terminalization and authoritative JSON replacement are durable, ordered, recoverable protocols. |
| 3 | CA-03 | Worker launch identity is an append-only ledger keyed by immutable `launch_id`, separate from retry and execution ordinals. |
| 4 | CA-04 | Question and replan decisions are typed transactional compare-and-set operations with matching events. |
| 5 | CA-05 | Run admission and integration merge use durable intent, compensation, exact authority, and restart reconciliation. |
| 6 | CA-06 | Status, terminal evidence, attention, and actions come from one coherent daemon snapshot and semantic projection. |
| 7 | CA-07 | Output, telemetry, polling, economics persistence, and descendant supervision have measured bounds. |
| 8 | CA-08 | Worker/repair wires, events, selected slices, provenance, follow-up drafts, and attention policy use typed daemon-owned contracts. |
| 9 | CA-09 | Rust and Node consume one reviewed fixture contract; the standard package and CI paths run all shipped tests and workflow checks. |

The implementation and closure commits remain separate for every CA slice. CA-09 closure is authorized only after its full gate passes.

## Confirmed behavioral test seams

Before CA-09 tests were written, the operator confirmed these seams:

1. **Canonical checked-in fixtures.** `tests/fixtures/contracts/v1.json` is the one versioned contract bundle. Rust decodes it through domain/read-model wire types; Node drives the shipped monitor and worker extension boundaries from the same bytes.
2. **Deterministic script check.** `tests/fixtures/contracts/generate --check` regenerates through Rust typed constructors to a temporary file and byte-compares it with the reviewed fixture. `--write` is the only update path. Fixture updates therefore appear as ordinary reviewable diffs.
3. **Standard test discovery.** `npm test` names both extension-local suites and external `tests/*.test.mjs`; GitHub Actions uses Node 22 and runs the same command.

Behavioral TDD evidence:

- RED: `node --test tests/contract-fixtures.test.mjs` failed four tests because the canonical fixture was absent and `package.json` did not discover external tests.
- Gap proof: the old `npm test` still passed 28 tests, demonstrating that the new shipped consumer suite was not in the standard package path.
- GREEN: after the fixture producer/check and package wiring, `npm test` passed all 38 Node tests, including monitor painting, worker-result submission, strict wire shape, typed/legacy events, terminal summary, and blocked/failed/completed/cancelled status cases.

The bundle covers:

- daemon status/read-model and `feed_version: 2` projection values;
- daemon-owned operator actions;
- closed worker and repair wire results with daemon authority fields absent;
- a canonical typed event and a preserved unknown legacy event;
- a terminal implementation summary enriched with daemon-owned launch/attempt/trigger identity;
- representative blocked, failed, completed, and cancelled run projections.

## Invariant-bearing protocols extracted

The remediation extracted or deepened only protocols with observed correctness pressure:

- worktree snapshot/restore and explicit completion-publication manifest;
- atomic artifact replacement and terminalization reconciliation;
- immutable worker-launch ledger and launch-scoped authorization;
- transactional question/replan decision outcomes;
- durable run-launch admission and deterministic merge-operation reconciliation;
- snapshot-rooted status/read-model construction and typed action projection;
- bounded diagnostic retention with append-only raw evidence;
- adaptive polling, coalesced activity, revision-based economics, and shared process-group supervision;
- closed worker/repair wire conversion with daemon-owned enrichment;
- typed event kind/payload pairing with legacy and unknown-event preservation;
- normalized selected-slice order, explicit provenance/follow-up fields, and one attention policy;
- one cross-language fixture producer/reviewer path and one complete shipped Node test path.

These are protocols rather than new workflow owners. State, policy, verification, authorization, merge, recovery, and handoff remain daemon-owned; Pi and Herdr remain execution/display adapters.

## Active frontier and operator truth

AF-06 autonomy is active. `off` does not classify; `shadow` records classifications only. `promote` and `run` may daemon-accept only deterministic Tier-1 typed `add_followup_slice` proposals inside the recorded mission envelope and remaining budgets. Decisions identify `authorizer: envelope:<run-id>` and `source: frontier_policy`, then use the same idempotent apply engine as operator acceptance. `promote` commits a generated slice for a future run; `run` appends and executes it serially.

The daemon does not invent candidates and does not auto-accept verification, policy, area, unsupported-kind, ambiguous, non-Tier-1, exhausted-budget/depth, prior-rejected/deferred, or `must_ask_if` cases. Those stay pending with structured attention/stop evidence until an operator decides. The `ask_operator` timeout fallback is narrower still: it chooses only the worker's original exact declared option inside existing authority; it cannot expand the mission or authorize destructive, credential, permission, release, push, or handoff actions.

## Explicitly deferred speculative abstractions

Measured evidence did not justify:

- a database connection pool;
- an external telemetry service or pub/sub broker;
- unbounded transcript/event retention in memory;
- a generic worker-harness abstraction beside Pi;
- JavaScript copies of daemon status, action, event, or terminal semantics;
- a generic event registry or runtime schema framework;
- splitting `workflow::manager` by phase or line count;
- parallel autonomous frontier execution;
- auto-accept for change kinds beyond typed follow-up slices;
- an LLM critic in an authorization path;
- cross-run/standing mission envelopes;
- worker edits to mission envelopes;
- complexity telemetry that automatically blocks work.

Reconsider any item only with measured repeated pressure and a smaller interface than the behavior it would hide.

## Closure evidence

The full closure gate is:

```text
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets
npm test
cargo run --quiet -- slices validate --repo .
scripts/roadmap-truth-check . docs/roadmap/auto-frontier/00-matrix.md
tests/fixtures/contracts/generate --check
scripts/soak-runtime --quick
```

On 2026-07-11 the gate passed: 414 Rust unit tests, 2 confinement tests, 49 daemon integration tests, and 38 Node tests; strict clippy, all-target check, formatting, fixture regeneration, slice validation, roadmap truth, and diff checks also passed. No test was deleted, ignored, narrowed, or snapshot-updated to obtain the result.

The reproducible 1/3/10-worker method and bounded-runtime interpretation are recorded in [`evidence/complexity-runtime-soak-2026-07-09.md`](evidence/complexity-runtime-soak-2026-07-09.md). The soak uses the same real `PiRunner` child path for baseline and final policies; it does not justify speculative runtime infrastructure.
