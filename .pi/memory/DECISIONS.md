# Decisions

## 2026-06-25: Use JSON Issue Slices as the atomic worker unit

Status: accepted.

Decision: Khazad-Doom workers receive one JSON Issue Slice at a time. Slice JSON is authoritative for scope, acceptance, verification, dependencies, and `must_ask_if` escalation triggers. Planning can stay human-native, but worker authorization is JSON.

## 2026-06-25: Khazad-Doom is a local Rust daemon/CLI

Status: accepted.

Decision: Implement Khazad-Doom as a local Rust CLI plus per-user daemon. The daemon owns durable run state, worker dispatch, cancellation, progress, checkpoints, verification, and handoff artifacts. Repos keep `.workflow` source artifacts; global runtime state lives outside repos.

## 2026-06-25: Use adapters and isolated worktrees for workers

Status: accepted.

Decision: Use a worker-agent adapter interface with Pi as the first real adapter and `fake` for deterministic smoke tests. Run slice workers in isolated git worktrees, allow conservative parallelism only for independent slices, and merge serially through an integration branch.

## 2026-06-25: Require structured worker handoffs

Status: accepted.

Decision: Workers must return JSON-only structured results, create per-slice commits, and leave clean worktrees. Khazad-Doom writes repo-local handoff packets before dispatch and synthesized implementation summaries/final reports after integration.

## 2026-06-25: Gate before merge/handoff; bound repair

Status: accepted.

Decision: Do not merge a slice until lightweight checks pass. Run full integration gates before final handoff. Per-slice repair and integration repair are bounded, and the run stops on failed/blocked slices unless resumed after user intervention.

## 2026-06-26: Monitoring is attachable and daemon-owned

Status: accepted.

Decision: `khazad-doom monitor --repo . --latest` is the harness-neutral live progress path. `/khazad-monitor` is an optional Pi overlay over daemon status JSON; closing it only detaches the overlay and never cancels the run.

## 2026-06-26: Treat workflow economics as a release invariant

Status: accepted.

Decision: Khazad-Doom must save operator time. Hidden extra agent turns, duplicate expensive verification, unconditional no-op repair, invisible retries, and unmeasured overhead are release blockers. Prefer gate-first repair and visible runtime/economics reporting.

## 2026-06-26: Prefer YAGNI and surgical implementation

Status: accepted.

Decision: Implementer agents should prefer minimal/surgical fixes and avoid speculative abstraction. One-line fixes are preferred when correct and readable, but correctness, workflow invariants, and tests take priority.

## 2026-06-26: Refactor Khazad-Doom seam-first

Status: accepted.

Decision: `workflow::Manager` is large but currently acts as a deep orchestration module. Refactor only where the new interface is smaller than the behavior it hides; likely first seams are integration gate/shell execution or context structs. Do not split the manager broadly just because it is large.

## 2026-06-27: Formalize exit states without adding gates

Status: accepted.

Decision: For SAW/SAFe-inspired ideas, Khazad-Doom should not add new optional gate machinery or import an 11-agent team model. The useful principles to retain are: make the existing workflow exit states explicit and enforce the separation that a worker must not approve its own evidence. Any reviewer/QA concept should be modeled as read-only attestation over existing run artifacts/evidence, not as a new workflow owner, hidden phase, or second source of truth.

## 2026-07-03: Treat slices as bounded intent, not frozen mini-specs

Status: accepted.

Decision: JSON Issue Slices remain the authoritative worker authorization envelope: goal, acceptance, areas, dependencies, verification, and `must_ask_if`. Acceptance criteria are minimum required evidence, not an exhaustive list of every valid test case. Operational rule: learning is allowed inside the fence; moving the fence requires approval. While a slice is open, workers may implement the smallest TDD/code-inspection discovery that is directly implied by the slice goal or acceptance and stays within declared areas. If a discovery changes product intent, public API semantics, dependencies, verification policy, or required paths outside `areas`, the worker must block with an `ask-user` finding or recommend a follow-up slice. Do not add new workflow phases, gates, or schema fields until real runs prove the prompt/doc rule is insufficient.
