# Pi-Native Migration — Master Traceability Matrix

Date: 2026-07-04
Owner: sivanirosh
Scope: commit Khazad-Doom to Pi as the sole real worker harness and unlock Pi-native capabilities, while keeping daemon-owned durable state and the fake test runner.

Every implementation task for this migration must reference a Slice ID from this matrix. When a slice moves to `ready`, convert its workpackage into a JSON Issue Slice under `.workflow/slices/` so Khazad-Doom can dogfood its own migration.

## Product decisions

- **D1 — Pi-first commitment.** Pi is the only real worker harness. `FakeRunner` stays as the deterministic test double, justified by testing, not portability. Daemon state remains harness-neutral JSON (free neutrality); worker execution is Pi-native (paid generality removed).
- **D2 — Truthful environmental failure.** Deterministic environment/launch failures block immediately with operator guidance; they never burn retries or masquerade as implementation failures.
- **D3 — Escalation over termination.** A worker hitting a `must_ask_if` condition escalates to the operator mid-run and continues after an answer, instead of dying `blocked`.
- **D4 — Versioned coupling only.** Khazad couples to Pi's documented, versioned surfaces (CLI flags, JSON event stream, exit codes). Never to stderr wording, internals, or unversioned behavior. Unknown fields/events are tolerated.
- **D5 — Single verification owner.** The daemon owns verification, gates, economics, and attestation. No duplicate Pi-side acceptance gates, no silent model failover.
- **D6 — Feedback stays daemon-owned and explicit.** Operators must be able to discover progress and needs-attention states through `status`, `watch`, and `monitor`; daemon state remains the single source of truth and the CLI stays the harness-neutral surface. No Pi monitor UI extension ships in this package.

## Status state machine

`planned` → `ready` → `in_progress` → `done`
Any state → `blocked` (blocker named explicitly).
Decision not to implement → `explicitly_deferred` (rationale + revisit condition in the workpackage).
No hidden states: no "mostly done", no "wired later".

## Matrix

| Product Decision | Required Feature | Slice ID | Files / Modules Likely Touched | Success Criteria | Required Tests | Status | Explicit Deferrals |
|---|---|---|---|---|---|---|---|
| D1 | Pi-first doctrine recorded; multi-harness aspiration removed | PI-00 | `.pi/memory/VISION.md`, `.pi/memory/PLAN.md`, `docs/workflow-invariants.md`, `README.md` | No project doc claims harness-agnostic worker execution; invariants state D1–D5; `Runner` trait documented as test seam | Doc review; `grep` finds no "harness-agnostic" worker claims | `ready` | Multi-harness worker support removed from vision (revisit only on concrete demand) |
| D2 | Classify non-retryable Pi launch failures | PI-01 | `src/agent.rs`, `src/workflow/manager.rs`, `src/workflow/gate.rs`, `tests/daemon_integration.rs`, `docs/workflow-invariants.md` | Missing-auth launch → run/slice `blocked`, 1 attempt, incident with provider/model/profile + fix commands; unmatched errors keep current retry behavior | Unit: classifier signatures, retryable fallback; Integration: fake `pi` auth-failure → blocked, 1 attempt, later layers not dispatched; fake path stays green | `ready` | Generic `FailureClassifier` module deferred to evidence; see PI-01 |
| D4 | Typed, versioned Pi event/CLI contract module | PI-02 | `src/agent.rs` (or new `src/agent/pi_contract.rs`), `docs/workflow-invariants.md`, tests | All Pi wire-format knowledge lives in one module; unknown events/fields ignored; contract inventory documented; actual model/provider captured when Pi reports it | Unit: parser tolerance (unknown fields, new event types, version markers); regression: recorded event-stream fixtures from current Pi | `planned` | Consuming pi-subagents lifecycle artifacts deferred (Khazad drives `pi -p` directly) |
| D1, D4 | Pi profile fidelity — one effective worker profile | PI-03 | `src/agent.rs`, `src/artifact.rs`, `src/domain.rs`, `src/workflow/manager.rs`, `.workflow/agents.toml`, docs | Config precedence (CLI > env > `khazad.json` > `agents.toml` > default) computed and tested in one module; Pi args generated in one place; identical profile summary in run events, handoffs, reports, monitor | Unit: precedence table, arg generation; Integration: run_started/handoff/report show same profile | `planned` | `fallbackModels` failover rejected (D5); revisit only if provider-outage incidents recur AND attestation records actual model |
| D3 | Operator escalation channel for `must_ask_if` | PI-04 | `src/ipc.rs`, `src/daemon.rs`, `src/state.rs`, `src/workflow/manager.rs`, `src/workflow/prompts.rs`, `src/cli.rs`, new worker-side Pi extension under `extensions/`, `skills/khazad-doom`, tests | Worker raises question mid-run; daemon persists it; `status`/monitor surface it with the answer command; operator answers via CLI; worker continues; timeout falls back to today's `blocked` | Unit: question lifecycle, token auth, timeout; Integration: scripted worker asks → answer → run completes; daemon-restart edge; workflow test below | `planned` | Nested worker fan-out (worker spawning subagents) deferred; interactive TUI answering deferred (CLI answer first) |
| D6 | Render-ready status projection — one interpretation layer for CLI renderers and future read-only adapters | PI-05 | new `src/workflow/projection.rs`, `src/ipc.rs`, `src/cli.rs`, `src/domain.rs`, tests | All feed interpretation lives in one daemon-side module; CLI monitor/watch/status are thin painters of the same versioned projection; behavior-preserving for current output | Unit: projection fixture snapshots; parity: CLI output equivalence on recorded runs; grep check: no event-type strings in painters | `planned` | Push/streaming transport deferred (polling stays); projection for offline `inspect` artifacts deferred; Pi monitor UI removed |

## Dependency order

```text
PI-00 (doctrine)          — no deps; do first, it is the decision record
PI-01 (classify failures) — no deps; already fully specified
PI-02 (event contract)    — after PI-01; upgrades PI-01's signal source if Pi emits typed errors
PI-03 (profile fidelity)  — after PI-00; independent of PI-02
PI-05 (status projection) — after PI-00; independent of PI-01..03; land before PI-04 so
                            pending questions have a surface to render on
PI-04 (escalation)        — after PI-01 (blocked semantics) and PI-02 (contract module); largest slice
```

PI-01 and PI-00 can land in either order or together. PI-04 must not start until its open questions are resolved and its status is `ready`. An escalation channel must always render pending questions in daemon-owned `status`, `watch`, and `monitor` output; a Pi ambient surface is not part of the current package.

## Cross-slice workflow acceptance test

Proves the slices connect into one coherent operator path. Run after PI-04 lands.

```text
1. Operator runs `khazad-doom run --slice S` with Pi unauthenticated for the configured provider.
2. Run ends `blocked` after exactly one launch attempt; status output names provider/model/profile
   and prints the fix commands (PI-01, PI-03). No worker attempt artifacts imply implementation work.
3. Operator authenticates Pi, reruns the same command.
4. Worker starts; mid-run it hits a must_ask_if condition and calls ask_operator (PI-04).
5. Run stays active with progress phase awaiting_operator; `khazad-doom status`, `watch`, and
   `monitor` show the question and the exact answer command without requiring any Pi UI adapter.
6. Edge condition: the daemon is restarted while the question is pending. After restart, the
   question is still listed as pending against the interrupted run; answering it is either applied
   on resume or rejected with a clear "run interrupted, resume first" error — never silently lost.
7. Operator answers; worker resumes and completes; run reaches handoff.
8. Invariants: attempts consumed == 1 for the auth-blocked run and == 1 for the successful run;
   the profile summary in run_started, handoff JSON, and final report is identical (PI-03);
   every event in the run log parses through the typed contract module (PI-02);
   economics report separates awaiting-operator wall time from agent time;
   CLI status/watch/monitor render identical wording for the same run state,
   because all paint the same projection (PI-05).
```

## Explicit deferrals and rejections (migration level)

| Item | Decision | Rationale | Revisit condition |
|---|---|---|---|
| Multi-harness worker support | Removed | N=1 real harness; agnosticism was shaping design while delivering nothing | A concrete second harness with a user |
| Pi acceptance gates (`attested/checked/verified/reviewed`) | Rejected | D5: daemon is sole verification owner; duplicate gates violate runtime economics | Never, unless daemon verification is retired |
| `fallbackModels` silent failover for workers | Rejected | Handoff attestation must not lie about the model that did the work | Provider-outage incidents recur AND attestation records the actual model per attempt |
| Consuming pi-subagents for delegation | Deferred | Khazad drives `pi -p` directly; pi-subagents is session-scoped orchestration | Khazad needs nested fan-out inside a worker |
| Auto-login / credential mutation | Rejected | Out of trust boundary; operator action by design | Never |
| Pi monitor UI and ambient widget | Removed | The optional UI adapter duplicated core daemon monitoring, introduced Pi session-lifecycle crash risk, and is not required for worker execution or operator escalation. Core monitoring remains `status`/`watch`/`monitor`. | Revisit only with concrete demand and a lifecycle-safe Pi UI API contract |
| Push/streaming status transport | Deferred | Polling over the existing socket/CLI is sufficient at current scale; streaming adds daemon lifecycle complexity | Polling interval becomes a measured UX or load problem |
