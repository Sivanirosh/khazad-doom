# PI-04 — Operator escalation channel for `must_ask_if`

Matrix row: [00-matrix.md](00-matrix.md) → PI-04. Status: `in_progress`. The base ask/answer/timeout/unavailable/restart proof exists; ASK-FALLBACK-01 adds daemon-owned 60-second bounded recommendation fallback while PI-04 remains open pending its own slice closure and production observation.
Depends on: PI-01 (blocked semantics, incident vocabulary), PI-02 (contract module).

## Scope

Convert the most frustrating terminal state — a worker dying `blocked` on a `must_ask_if` condition — into an interactive pause:

- **Worker side:** a Khazad-shipped Pi extension, loaded additively into worker sessions, registers an `ask_operator` tool. Worker launches keep the operator's normal Pi extensions and skills; Khazad-Doom adds only the per-attempt worker extension. The worker prompt (`src/workflow/prompts.rs`) requires the question/options, original recommendation/rationale, and explicit bounded-authority plus reversible attestations. Unavailable, malformed, ineligible, unsafe, or cancelled-before-deadline questions retain the blocked contract.
- **Transport:** the daemon's existing Unix socket (`src/ipc.rs`). The daemon passes `KHAZAD_DAEMON_SOCKET`, `KHAZAD_RUN_ID`, `KHAZAD_SLICE_ID`, and a per-run `KHAZAD_WORKER_TOKEN` into the worker environment. IPC methods: `workerAsk` (posts question, then blocks for a CLI/headless answer), `workerAskOpen` (posts question for same-pane Pi UI), `workerQuestionTimeout` (closes cancelled/expired in-pane prompts), `listQuestions`, `answerQuestion`.
- **Daemon side:** questions persist in `state::Store`, including recommendation/rationale, bounded/reversible inputs, absolute deadline, final answer, and typed answer source. Progress snapshot gains phase `awaiting_operator` (string phase — **no** `SliceStatus` enum change; slice stays `Running`). Ask/answer events preserve the audit evidence; answer/source/event/progress commit transactionally.
- **Operator side:** in native Pi TUI mode, the worker pane itself is the operator answer surface: `ask_operator` records the pending daemon question, shows the normal Pi select/input dialog in that worker session, and submits the selected answer through `answerQuestion`. `khazad-doom status`/monitor still show the pending question and exact answer command; CLI `khazad-doom answer <run-id> <question-id> "text"` and `/khazad-answer` remain explicit fallback/debug paths. The Pi monitor bridge is read-only for worker questions.
- **Timeout policy:** the built-in and repository timeout is 60 seconds (`0` waits indefinitely). At the absolute daemon-owned deadline, only an exact match to one non-empty declared option with non-empty rationale and true bounded-authority/reversible attestations is auto-answered with `answer_source=llm_recommendation_timeout`. All missing/invalid/hard-authorization cases remain timed out and blocked.
- **Economics:** awaiting-operator wall time is accounted separately from agent time in progress/reports (the pause must not pollute phase-duration economics).
- **Attempt-timeout interplay:** `worker_attempt_timeout_seconds` must exclude time spent in `awaiting_operator`, or a long think by the operator kills the attempt. The supervision loop (`WorkerAttemptContext`) needs a pause-aware clock.

## Out of scope

- Nested fan-out (workers spawning subagents) — deferred at matrix level.
- Remote/mobile answering beyond a thin authenticated bridge over daemon `status` + `answerQuestion`.
- Multi-question concurrency per slice beyond a simple queue (one open question per slice at a time is acceptable v1).
- Changing `must_ask_if` slice-schema semantics (the fence stays; this changes only what happens at the fence).

## Data model changes

- `worker_questions` stores identity, attempt, question/options, recommendation/rationale, both eligibility inputs, derived eligibility, relative timeout, absolute deadline, state, final answer, and answer source, with an index on run_id/state. Additive defaulted columns preserve old rows.
- No enum changes to `RunStatus`/`SliceStatus` (doctrine: schema/state changes evidence-driven; progress phase string suffices until reporting proves otherwise).
- Handoff/report JSON: additive `questions` section (asked/answered/timed-out counts and transcript).

## API changes

- IPC: `workerAsk`/`workerAskOpen` accept additive `recommended_answer`, `rationale`, `bounded_within_current_slice_or_mission_authority`, and `reversible` fields. Results include durable deadline, eligibility, answer, and answer source. `workerQuestionTimeout` and `answerQuestion` are idempotent on a committed race winner and return durable state; `listQuestions` exposes the same transcript.
- CLI: `khazad-doom answer ...`, `khazad-doom questions [--run ...]`.
- Worker extension ships in this repo (`extensions/`), versioned with the daemon; handoff already tells workers their contract — prompt text gains the escalation instructions.

## UI states (CLI/monitor output)

- **Pending question:** status/monitor show slice, elapsed wait, question preview, and the copy-pasteable answer command.
- **No pending questions:** `khazad-doom questions` says so explicitly (empty state).
- **Answered:** event visible in run event log with `operator` or `llm_recommendation_timeout` source; worker resumes; progress phase returns to `worker_running`.
- **Timeout:** eligible bounded recommendation continues the worker; missing/invalid/ineligible/hard recommendations end blocked with the unanswered question preserved.
- **Error/race states:** unknown IDs and token mismatches reject; interrupted/stale/terminal runs never auto-answer; already-committed answer races return the durable winner instead of an error or second event.
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
- Question lifecycle state machine (pending → answered / timed-out; first committed answer wins and later race participants receive the durable outcome).
- Token validation; cross-run token rejected.
- Pause-aware attempt clock excludes awaiting time.

Integration:
- Scripted fake worker asks → CLI answers → worker output reflects the answer → run completes.
- Worker extension same-pane Pi prompt path opens the question with `workerAskOpen`, calls `answerQuestion`, and returns the durable selected/race-winning answer; prompt cancellation calls `workerQuestionTimeout` and preserves an already-applied fallback. The headless path returns the same source metadata.
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
5. Second edge: operator never answers a question in another run; at the deadline an eligible bounded recommendation continues with typed source, while a missing/invalid/hard recommendation ends blocked and preserves the unanswered question verbatim.
6. Invariants: attempts consumed == 1 for the answered slice (escalation burned no retry);
   awaiting-operator wall time is reported separately from agent time; no question row is
   ever orphaned (every row ends answered, timed-out, or attached to an interrupted run).
```

## Acceptance criteria

1. Worker can escalate mid-run and continue after an operator answer, without consuming an attempt.
2. Pending questions are durable, visible in status/monitor, answerable in the worker pane by default, and answerable via CLI fallback.
3. Timeout atomically applies only an eligible exact-option bounded/reversible recommendation; every other timeout degrades to today's `blocked` with the question preserved in the incident/handoff.
4. Token scoping enforced at the daemon; cross-run asks rejected.
5. Attempt timeout and economics exclude awaiting time.
6. Channel-unavailable path is behavior-identical to today (asserted by test).
7. Fake-runner script mode keeps all of the above deterministic in CI.

## ASK-FALLBACK-01 behavioral-TDD evidence

The real-Pi worker session for run `kd-20260709-231950-60c0f658`, launched from `55ddf0b`, is preserved at:

```text
~/.pi/agent/sessions/--home-sivanirosh-.khazad-doom-worktrees-9afe9527241f-kd-20260709-231950-60c0f658-ASK-FALLBACK-01--/2026-07-09T23-19-52-463Z_019f492e-338f-71e8-8e78-7cda72853907.jsonl
```

Its JSONL messages 76–79 record the first RED commands before the first production implementation edits at messages 93–95. Representative verbatim failures are copied here so the sequence remains inspectable without replaying the deleted worktree:

```text
$ node --test extensions/khazad-worker/index.test.mjs tests/khazad-worker-extension.test.mjs
ℹ tests 19
ℹ pass 15
ℹ fail 4

TypeError: Cannot read properties of undefined (reading 'type')
AssertionError: input did not match /2026-07-10T00:01:00+00:00/
AssertionError: 'B' !== 'A'
AssertionError: '' !== 'yes'
```

Those failures were the new headless recommendation contract, absolute-deadline same-pane expiry, answer-race winner, and same-pane cancellation/fallback tests. The Rust StateStore/daemon-RPC tests were also compiled before the recommendation/source implementation existed:

```text
$ cargo test worker_question --quiet
error[E0422]: cannot find struct `WorkerQuestionRecommendation` in this scope
error[E0609]: no field `answer_source` on type `domain::WorkerQuestion`
error[E0599]: no method named `insert_worker_question_with_recommendation`
error[E0599]: no method named `answer_worker_question_cas`
error: could not compile `khazad-doom` (bin "khazad-doom" test) due to 16 previous errors
```

Messages 130–141 retain the intermediate RED phase after those types existed: daemon assertions still failed with `left: Null` / `right: false`, first two tests and then one test, while the Node contract moved from one remaining failure to green. Messages 144–146 show `cargo test worker_question --quiet` at 10/10 and `cargo test ask_operator --quiet` green; messages 231–234 show the expanded 13-test Rust worker-question suite and all 19 then-current Node tests green. The recovered implementation was subsequently rerun through the expanded Rust/Node suites, strict Clippy, formatting, and workflow-validation gates.

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
