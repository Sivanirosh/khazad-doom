# PI-04 — Operator escalation channel for `must_ask_if`

Matrix row: [00-matrix.md](00-matrix.md) → PI-04. Status: `in_progress` after the 2026-07-06 truth audit: code and extension tests exist despite unresolved workpackage questions; black-box ask/answer/timeout/restart proof is missing.
Depends on: PI-01 (blocked semantics, incident vocabulary), PI-02 (contract module).

## Scope

Convert the most frustrating terminal state — a worker dying `blocked` on a `must_ask_if` condition — into an interactive pause:

- **Worker side:** a Khazad-shipped Pi extension, loaded additively into worker sessions, registers an `ask_operator` tool. Worker launches keep the operator's normal Pi extensions and skills; Khazad-Doom adds only the per-attempt worker extension. The worker prompt (`src/workflow/prompts.rs`) instructs: on a `must_ask_if` condition, call `ask_operator` with the question and the options considered; only return `blocked` JSON if the channel is unavailable or the question times out/cancels.
- **Transport:** the daemon's existing Unix socket (`src/ipc.rs`). The daemon passes `KHAZAD_DAEMON_SOCKET`, `KHAZAD_RUN_ID`, `KHAZAD_SLICE_ID`, and a per-run `KHAZAD_WORKER_TOKEN` into the worker environment. IPC methods: `workerAsk` (posts question, then blocks for a CLI/headless answer), `workerAskOpen` (posts question for same-pane Pi UI), `workerQuestionTimeout` (closes cancelled/expired in-pane prompts), `listQuestions`, `answerQuestion`.
- **Daemon side:** questions persist in `state::Store` (new table: id, run_id, slice_id, question, options, asked_at, answered_at, answer, state). Progress snapshot gains phase `awaiting_operator` (string phase — **no** `SliceStatus` enum change; slice stays `Running`). `run_incident`-style event on ask and on answer.
- **Operator side:** in native Pi TUI mode, the worker pane itself is the operator answer surface: `ask_operator` records the pending daemon question, shows the normal Pi select/input dialog in that worker session, and submits the selected answer through `answerQuestion`. `khazad-doom status`/monitor still show the pending question and exact answer command; CLI `khazad-doom answer <run-id> <question-id> "text"` and `/khazad-answer` remain explicit fallback/debug paths. The Pi monitor bridge is read-only for worker questions.
- **Timeout policy:** configurable `ask_operator` timeout (default generous, e.g. 30–60 min). On timeout the tool returns "no answer; proceed per your blocked contract" and the worker returns `blocked` exactly as today — the feature degrades to current behavior, never hangs a run forever.
- **Economics:** awaiting-operator wall time is accounted separately from agent time in progress/reports (the pause must not pollute phase-duration economics).
- **Attempt-timeout interplay:** `worker_attempt_timeout_seconds` must exclude time spent in `awaiting_operator`, or a long think by the operator kills the attempt. The supervision loop (`WorkerAttemptContext`) needs a pause-aware clock.

## Out of scope

- Nested fan-out (workers spawning subagents) — deferred at matrix level.
- Remote/mobile answering beyond a thin authenticated bridge over daemon `status` + `answerQuestion`.
- Multi-question concurrency per slice beyond a simple queue (one open question per slice at a time is acceptable v1).
- Changing `must_ask_if` slice-schema semantics (the fence stays; this changes only what happens at the fence).

## Data model changes

- New state table `worker_questions` (schema above) with indexes on run_id/state.
- No enum changes to `RunStatus`/`SliceStatus` (doctrine: schema/state changes evidence-driven; progress phase string suffices until reporting proves otherwise).
- Handoff/report JSON: additive `questions` section (asked/answered/timed-out counts and transcript).

## API changes

- IPC: `workerAsk { run_id, slice_id, token, question, options[], timeout_seconds }` → `{ answer }` | timeout; `workerAskOpen { ... }` → `{ question_id, timeout_seconds }`; `workerQuestionTimeout { run_id, question_id, token }` → timeout; `listQuestions { repo_path | run_id }`; `answerQuestion { run_id, question_id, answer }`. Documented in invariants doc with the token rule.
- CLI: `khazad-doom answer ...`, `khazad-doom questions [--run ...]`.
- Worker extension ships in this repo (`extensions/`), versioned with the daemon; handoff already tells workers their contract — prompt text gains the escalation instructions.

## UI states (CLI/monitor output)

- **Pending question:** status/monitor show slice, elapsed wait, question preview, and the copy-pasteable answer command.
- **No pending questions:** `khazad-doom questions` says so explicitly (empty state).
- **Answered:** event visible in run event log; worker resumes; progress phase returns to `worker_running`.
- **Timeout:** slice ends `blocked` with an incident that includes the unanswered question (so the handoff explains *what* needed deciding).
- **Error states:** answering an unknown/already-answered question → clear error; token mismatch → rejected with incident; answering a question on an interrupted run → "run interrupted, resume first" error (never silently applied or lost).
- **Channel unavailable** (old Pi, extension failed to load): worker behaves exactly as today; run report notes escalation was unavailable.

## Migration / backward compatibility

- Workers without the extension or on an older Pi keep current `blocked` behavior — escalation is a capability, not a requirement.
- Daemon restart with a pending question: question row persists; the worker process dies with the daemon (current lifecycle), run becomes `interrupted` as today; on `resume`, previously answered questions are injected into the retry prompt's failure context so the answer is not lost, and unanswered ones are re-askable. Never silently dropped.
- `fake` runner gains a deterministic ask/answer script mode so integration tests don't need a live Pi.

## Permissions

- `workerAsk` requires the per-run token; a worker can only post questions for its own run/slice. Token is generated per run, passed via env, stored hashed in state.
- Socket remains user-local (existing daemon trust boundary); `answerQuestion` is operator-CLI only — no remote surface.
- Enforced at the IPC layer (daemon), not in the extension.

## Test plan

Unit:
- Question lifecycle state machine (pending → answered / timed-out; double-answer rejected).
- Token validation; cross-run token rejected.
- Pause-aware attempt clock excludes awaiting time.

Integration:
- Scripted fake worker asks → CLI answers → worker output reflects the answer → run completes.
- Worker extension same-pane Pi prompt path opens the question with `workerAskOpen`, calls `answerQuestion`, and returns the selected answer; prompt cancellation calls `workerQuestionTimeout` and returns the blocked-contract signal.
- Timeout → slice `blocked` with question-bearing incident.
- Daemon restart with pending question → question survives; answer-after-interrupt rejected with guidance; resume path re-exposes the question.
- Channel-unavailable worker → byte-identical to pre-slice behavior.

### Workflow acceptance test

```text
1. Operator starts a run whose slice has a must_ask_if rule the worker will hit.
2. Worker calls ask_operator; progress phase becomes awaiting_operator; status shows the
   question and the answer command; economics clock for agent time is paused.
3. Operator answers in the same worker Pi pane (or via explicit `khazad-doom answer` fallback); worker continues and completes the slice;
   run reaches ready_to_merge.
4. Edge condition: a second run's worker attempts workerAsk with the first run's token;
   daemon rejects it and records an incident; the first run is unaffected.
5. Second edge: operator never answers a question in another run; after the timeout the
   slice ends blocked and the handoff contains the unanswered question verbatim.
6. Invariants: attempts consumed == 1 for the answered slice (escalation burned no retry);
   awaiting-operator wall time is reported separately from agent time; no question row is
   ever orphaned (every row ends answered, timed-out, or attached to an interrupted run).
```

## Acceptance criteria

1. Worker can escalate mid-run and continue after an operator answer, without consuming an attempt.
2. Pending questions are durable, visible in status/monitor, answerable in the worker pane by default, and answerable via CLI fallback.
3. Timeout degrades to today's `blocked` with the question preserved in the incident/handoff.
4. Token scoping enforced at the daemon; cross-run asks rejected.
5. Attempt timeout and economics exclude awaiting time.
6. Channel-unavailable path is behavior-identical to today (asserted by test).
7. Fake-runner script mode keeps all of the above deterministic in CI.

## Open questions (block `ready`)

1. One question per slice at a time acceptable for v1? (Recommended yes; multi-question queue deferred.)

## Definition of Done

- [ ] Data model changes applied and migrated (`worker_questions` table).
- [ ] IPC/CLI contracts implemented and documented, with error contracts (unknown id, double answer, token mismatch, interrupted run).
- [ ] All named output states implemented: pending, empty, answered, timeout, error states, channel-unavailable.
- [ ] Permissions enforced at the IPC layer (token scoping), not only in the extension.
- [ ] Migration/backward-compat verified: no-extension parity test, restart survival, resume re-exposure.
- [ ] Unit tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: invariants (escalation contract, token rule), README (operator flow), skill/prompt wording.
- [ ] Invariants checked: no orphaned questions, attempts not consumed by escalation, economics separation, `must_ask_if` fence semantics unchanged.
