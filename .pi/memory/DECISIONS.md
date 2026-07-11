# Decisions

## 2026-06-25: JSON Issue Slices authorize bounded work

Status: accepted.

Decision: JSON Issue Slices are the atomic worker authorization envelope for goal, acceptance, literal repo-relative area prefixes, dependencies, verification, and `must_ask_if`. Acceptance is minimum evidence, not a frozen mini-spec: learning may stay inside the fence; moving intent, paths, policy, dependencies, public semantics, or security authority requires approval or a typed follow-up proposal. Closed slices are historical accepted work and are not rerun as dependencies.

## 2026-06-25: The local Rust daemon owns workflow truth

Status: accepted.

Decision: Khazad-Doom is a local Rust CLI plus per-user daemon. The daemon owns admission, scheduling, state, worktrees, cancellation, questions, replans, verification, repair, integration, publication, incidents, economics, recovery, and handoff. Repository `.workflow` artifacts remain inspectable, but UI state and worker claims never become a second source of truth.

## 2026-06-25: Isolate workers and integrate deterministically

Status: accepted.

Decision: Workers run in isolated worktrees. Independent area-disjoint slices may execute in bounded parallel batches (default parallelism 3), but the whole layer completes before serial integration; overlapping-area slices serialize between merge checkpoints. Every spawned worker is joined, cancellation propagates to descendants, and no sibling merges from a failed layer.

## 2026-06-25: Require structured worker evidence and daemon attestation

Status: accepted.

Decision: Workers commit clean worktrees and return closed JSON results containing only worker-authored facts. Acceptance entries are claims; daemon checks and gates attest them. The daemon injects immutable slice, attempt, launch, trigger, and authority identity. Worker, renderer, repair, or planner output cannot self-approve evidence or invent daemon-owned state.

## 2026-06-25: Verification and repair are bounded by authority

Status: accepted.

Decision: Lightweight slice checks precede merge; `verify_profile` belongs only to the integration gate. Verification is observationally pure and may not mutate publication inputs. Gate runs before repair. Envelope retries and mechanical repair are bounded, visible, and separate from implementation attempts; repair cannot broaden paths, change intent, weaken checks, or hide operator-environment failures. A false positive that repairs beyond authority is worse than a false negative that blocks.

## 2026-06-26: Monitoring is daemon-owned and non-authoritative

Status: accepted.

Decision: `status`, `watch`, `monitor`, `attend`, the Pi bridge, and Herdr cockpit render one daemon-owned projection. Visibility failures are warning incidents only. Detaching, closing, focusing, renaming, or losing a pane never cancels or changes a run. Pane text, scrollback, labels, IDs, and Herdr metadata are not correctness evidence.

## 2026-06-26: Runtime economy is a release invariant

Status: accepted.

Decision: Hidden agent turns, duplicate expensive verification, unconditional repair, invisible retries, unbounded tails, noisy telemetry writes, and avoidable polling are release defects. Preserve complete raw/structured evidence while bounding diagnostics, memory, updates, persistence, and cancellation latency. Persist economics by dirty revision and use measured 1/3/10-worker evidence before adding pools, brokers, telemetry services, or other infrastructure.

## 2026-06-26: Prefer YAGNI and deep seams

Status: accepted.

Decision: Prefer surgical fixes and interfaces smaller than the behavior they hide. `workflow::Manager` remains a cohesive temporal orchestrator unless measured locality pressure justifies a deep extraction. Do not split by file size or add generic harnesses, event registries, extra gates, planner layers, connection pools, or compatibility abstractions without repeated evidence.

## 2026-06-27: Exit states and evidence ownership stay explicit

Status: accepted.

Decision: Existing lifecycle exit states, incidents, primary terminal reasons, evidence attestation, and plan revisions are explicit read-only summaries of daemon state. Reviewer/QA concepts may attest existing evidence but must not introduce hidden gates, worker self-approval, or a second workflow owner.

## 2026-07-04: Pi is the sole real worker harness

Status: accepted.

Decision: Real workers run through Pi's documented surfaces. `fake` is permanently a deterministic test seam, not a portability promise. Daemon state and CLI JSON remain harness-neutral because that is useful and cheap. Silent model fallback and Pi-side acceptance gates are rejected; reports attest the effective provider, model, profile, and reasoning that actually ran.

## 2026-07-05: Agent profiles are operator-wide

Status: accepted.

Decision: `~/.khazad-doom/agents.toml` is the sole profile source, with explicit run overrides where supported. Repo-local profile fallback is intentionally absent. Worktree setup, profile resolution, and launch preflight are daemon-owned reliability boundaries.

## 2026-07-06: Decisions and replans are typed durable protocols

Status: accepted.

Decision: Operator questions, replan proposals, and grants persist before UI delivery and resolve through first-commit-wins transactional compare-and-set outcomes. Applied, idempotent, conflicting, stale, and missing outcomes remain explicit. Accepted changes use one idempotent apply path with provenance; restart preserves pending decisions and never infers operator intent.

## 2026-07-07: Herdr is the optional-default cockpit

Status: accepted.

Decision: Herdr provides worker, dashboard, gate/repair, and attention surfaces while the daemon remains authoritative. Native Herdr-hosted Pi TUI workers are default when placement is available; direct/JSON-wrapper execution remains an explicit compatibility fallback. Native workers submit only daemon-owned result artifacts. Cockpit placement resolves live anchors by semantic role rather than trusting stale pane IDs.

## 2026-07-09: Operator escalation stays authority-bounded

Status: accepted.

Decision: Worker `ask_operator` state belongs to the daemon even when the dialog appears in the worker Pi pane. A daemon-owned 60-second fallback may choose an exact listed recommendation only with nonempty rationale and bounded, reversible authority. Ambiguous, stale, interrupted, policy/API/security-changing, out-of-area, or otherwise ineligible `must_ask_if` cases still block.

## 2026-07-09: Autonomous frontier reuses replan authority

Status: accepted.

Decision: `off`, `shadow`, `promote`, and `run` are visibility/acceptance levels over the existing typed replan channel. Only deterministic Tier-1 `add_followup_slice` proposals inside the active mission envelope and remaining area/depth/generation/budget limits may auto-accept; generated slices run serially. Workers cannot invent candidates outside a proposal, and all ambiguous, exhausted, rejected, deferred, ineligible `must_ask_if`, or authority-expanding cases stop for the operator.

## 2026-07-11: Identity, admission, merge, and publication are durable protocols

Status: accepted.

Decision: Launch identity is append-only and distinct from attempt/retry/repair ordinals. Run admission and prepared launch intent commit before side effects. Destructive cleanup requires positive resource ownership. Merge authority binds exact run, scope, launch, source, expected head, parentage, ancestry, operation trailer, and predicted tree. Terminalization and authoritative JSON replacement are recoverable; publication uses an explicit pinned manifest and advances only the intended integration ref after a passing gate.

## 2026-07-11: Status and attention derive from one coherent snapshot

Status: accepted.

Decision: Authoritative status reads, evidence lookup, terminal summaries, handoffs, operator actions, and attention policy share one transaction-rooted snapshot and daemon semantic projection. Missing or corrupt indexed evidence is unavailable, never opportunistically reread live. Attention delivery, focus, rename, and dedupe are visibility-only and occur after authoritative state commits.

## 2026-07-11: Active contracts are strict; persisted compatibility is tolerant

Status: accepted.

Decision: Active worker and repair wires deny undeclared daemon-authority fields. Canonical event producers bind typed payloads to typed kinds; selected-slice order, provenance, and follow-up drafts have explicit normalized storage. Rust and Node consume one reviewed versioned fixture bundle with deterministic checking. Persisted legacy runs, queues, events, and unknown future kinds remain readable; dead active-wire compatibility and duplicated JavaScript daemon semantics are not retained.
