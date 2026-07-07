# Pi-Native Migration — Master Traceability Matrix

Date: 2026-07-06
Owner: sivanirosh
Scope: commit Khazad-Doom to Pi as the sole real worker harness and unlock Pi-native capabilities, while keeping daemon-owned durable state and the fake test runner.

Phase 1 truth audit: `docs/design/roadmap-truth-audit-2026-07-06.md`. The audited statuses below reconcile implementation reality with required tests and slice/run evidence; they are not slice acceptance or closure.

Roadmap Markdown is checked planning output, not workflow truth. `scripts/roadmap-truth-check` fails if a matrix row claims `done`, `closed`, `accepted`, `complete`, or `completed` while the referenced slice is not closed by JSON slice metadata plus named daemon report evidence.

Every implementation task for this migration must reference a Slice ID from this matrix. When a slice moves to `ready`, convert its workpackage into a JSON Issue Slice under `.workflow/slices/` so Khazad-Doom can dogfood its own migration.

## Product decisions

- **D1 — Pi-first commitment.** Pi is the only real worker harness. `FakeRunner` stays as the deterministic test double, justified by testing, not portability. Daemon state remains harness-neutral JSON (free neutrality); worker execution is Pi-native (paid generality removed).
- **D2 — Truthful environmental failure.** Deterministic environment/launch failures block immediately with operator guidance; they never burn retries or masquerade as implementation failures.
- **D3 — Escalation over termination.** A worker hitting a `must_ask_if` condition escalates to the operator mid-run and continues after an answer, instead of dying `blocked`.
- **D4 — Versioned coupling only.** Khazad couples to Pi's documented, versioned surfaces (CLI flags, JSON event stream, exit codes). Never to stderr wording, internals, or unversioned behavior. Unknown fields/events are tolerated.
- **D5 — Single verification owner.** The daemon owns verification, gates, economics, and attestation. No duplicate Pi-side acceptance gates, no silent model failover.
- **D6 — Feedback stays daemon-owned and explicit.** Operators must be able to discover progress and needs-attention states through `status`, `watch`, and `monitor`; daemon state remains the single source of truth and the CLI stays the harness-neutral surface. Any Pi feedback adapter is explicit attach and read-only over the daemon feed.

## Status state machine

`planned` → `ready` → `in_progress` → `done`
Any state → `blocked` (blocker named explicitly).
Decision not to implement → `explicitly_deferred` (rationale + revisit condition in the workpackage).
No hidden states: no "mostly done", no "wired later".

Phase 1 audit interpretation: `in_progress` includes rows where implementation has already landed by hand but done-level evidence, declared tests, or slice/run closure is missing.

## Matrix

| Product Decision | Required Feature | Slice ID | Files / Modules Likely Touched | Success Criteria | Required Tests | Status | Explicit Deferrals |
|---|---|---|---|---|---|---|---|
| D1 | Pi-first doctrine recorded; multi-harness aspiration removed | PI-00 | `.pi/memory/VISION.md`, `.pi/memory/PLAN.md`, `docs/workflow-invariants.md`, `README.md` | No project doc claims harness-agnostic worker execution; invariants state D1–D5; `Runner` trait documented as test seam | Doc review; `grep` finds no "harness-agnostic" worker claims | `in_progress` | Multi-harness worker support removed from vision (revisit only on concrete demand). Audit gap: doc criteria pass, but `.workflow/slices/PI-00.json` remains open and no dogfooded closure exists. |
| D2 | Classify non-retryable Pi launch failures | PI-01 | `src/agent.rs`, `src/workflow/manager.rs`, `src/workflow/gate.rs`, `tests/daemon_integration.rs`, `docs/workflow-invariants.md` | Missing-auth launch → run/slice `blocked`, 1 attempt, incident with provider/model/profile + fix commands; unmatched errors keep current retry behavior | Unit: classifier signatures, retryable fallback; Integration: fake `pi` auth-failure → blocked, 1 attempt, later layers not dispatched; fake path stays green | `in_progress` | Generic `FailureClassifier` module deferred to evidence; current source and installed-binary smoke pass, but `.workflow/slices/PI-01.json` remains open and historical production evidence only showed the failure before this audit. |
| D4 | Typed, versioned Pi event/CLI contract module | PI-02 | `src/pi_contract.rs`, `src/agent.rs`, `src/workflow/manager.rs`, `docs/workflow-invariants.md`, tests | All Pi wire-format knowledge lives in one module; unknown events/fields ignored; contract inventory documented; preflight records binary, launch flags, supported contract version, event vocabulary, and worker evidence kind | Unit: parser tolerance (unknown fields, new event types, version markers); integration: `pi_contract_preflight_records_profile_launch_and_contract_black_box`; regression: recorded event-stream fixtures from current Pi | `in_progress` | Consuming pi-subagents lifecycle artifacts deferred (Khazad drives `pi -p` directly). PI-PROOF-01 closes the preflight-recording proof gap with scripted Pi evidence; status remains `in_progress` until a PI-02 JSON slice exists/closes and current real-Pi event-stream fixtures are recorded. Revisit when Pi publishes a newer contract version or emits provider/model metadata in the stream. |
| D1, D4 | Pi profile fidelity — one effective worker profile | PI-03 | `src/agent_profile.rs`, `src/agent.rs`, `src/artifact.rs`, `src/domain.rs`, `src/workflow/manager.rs`, `src/workflow/projection.rs`, `~/.khazad-doom/agents.toml`, docs | Config precedence (CLI > env > `khazad.json` agent choice for worker kind > operator-global agent profile > default) computed and tested in one module; Pi args generated in one place; identical profile summary in run events, preflight, handoffs, reports, status/monitor feed, and economics; fake evidence is unmistakably labelled as deterministic-test-double evidence | Unit: precedence table, arg generation; integration: `profile_and_fake_evidence_are_consistent_everywhere_black_box`, `pi_contract_preflight_records_profile_launch_and_contract_black_box` | `in_progress` | `fallbackModels` failover rejected (D5); revisit only if provider-outage incidents recur AND attestation records actual model. PI-PROOF-01 closes the dedicated profile-surface proof gap, including fake-runner attestation; status remains `in_progress` until a PI-03 JSON slice exists/closes and production dogfood evidence confirms the same surfaces. |
| D3 | Operator escalation channel for `must_ask_if` | PI-04 | `src/ipc.rs`, `src/daemon.rs`, `src/state.rs`, `src/workflow/manager.rs`, `src/workflow/prompts.rs`, `src/cli.rs`, `extensions/khazad-worker`, `skills/khazad-doom`, tests | Worker raises question mid-run; daemon persists it with attempt identity; `status`/monitor surface it with the answer command; operator answers via CLI; worker continues; timeout/unavailable ask paths return blocked JSON with ask-user findings; daemon restart interrupts stale pending questions and requires resume/fresh answer | Unit: question lifecycle, token auth, timeout; integration: `ask_operator_answer_timeout_unavailable_and_restart_black_box`; workflow test below | `in_progress` | Nested worker fan-out (worker spawning subagents) deferred; interactive TUI answering deferred (CLI answer first). PI-PROOF-01 closes the black-box ask/answer/timeout/unavailable/restart proof gap; status remains `in_progress` until a PI-04 JSON slice exists/closes and production observation confirms the same operator path. Revisit TUI answering only after CLI answer has repeated dogfood use. |
| D6 | Render-ready status projection — one interpretation layer for CLI renderers and future read-only adapters | PI-05 | new `src/workflow/projection.rs`, `src/ipc.rs`, `src/cli.rs`, `src/domain.rs`, tests | All feed interpretation lives in one daemon-side module; CLI monitor/watch/status are thin painters of the same versioned projection; behavior-preserving for current output | Unit: projection fixture snapshots; parity: CLI output equivalence on recorded runs; grep check: no event-type strings in painters | `in_progress` | Push/streaming transport deferred (polling stays); projection for offline `inspect` artifacts deferred; Pi monitor UI removed. Audit gap: projection module exists, but CLI still duplicates interpretation and parity/grep tests are missing. |

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

PI-01 and PI-00 can land in either order or together. PI-04 must not start until its open questions are resolved and its status is `ready`. An escalation channel must always render pending questions in daemon-owned `status`, `watch`, and `monitor` output; the Pi widget is optional read-only feedback, not the required escalation surface.

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
| Rich Pi monitor overlay and auto-discovery | Removed; replaced by Herdr cockpit scope | The old optional Pi UI overlay duplicated core daemon monitoring, introduced Pi session-lifecycle crash risk, and is not the right live multi-agent cockpit. Core monitoring remains `status`/`watch`/`monitor`; FEED-01 makes daemon projection authoritative; HERDR-01..03 add an optional-default Herdr cockpit while the Pi adapter becomes a thin start/explain/answer/summarize/open bridge. | Revisit a richer Pi UI only if Herdr is unavailable long-term and Pi exposes a lifecycle-safe persistent multi-pane API without duplicated interpretation |
| Push/streaming status transport | Deferred | Polling over the existing socket/CLI is sufficient at current scale; streaming adds daemon lifecycle complexity | Polling interval becomes a measured UX or load problem |
