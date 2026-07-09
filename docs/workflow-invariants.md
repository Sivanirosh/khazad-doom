# Workflow invariants

These invariants define the daemon-owned workflow behavior that v0.1.0 release-polish refactors must preserve. They are intentionally phrased as testable contracts; changing one is a public workflow behavior change, not a cleanup.

## Product doctrine

- **D1 — Pi-first commitment.** Pi is the only real worker harness. `FakeRunner` stays as the deterministic test double, justified by testing, not portability. Daemon state remains harness-neutral JSON; worker execution is Pi-native.
- **D2 — Truthful environmental failure.** Deterministic environment/launch failures block immediately with operator guidance. They must not burn retries or masquerade as implementation failures.
- **D3 — Escalation over termination.** A worker that hits a `must_ask_if` condition should call the shipped worker-only `ask_operator` Pi tool, pause in `awaiting_operator`, open the normal Pi prompt in that same worker session, and continue after the answer is recorded through daemon `answerQuestion`. If the worker tool is unavailable or times out/cancels, the worker falls back to the existing `ask-user` blocked output.
- **D4 — Versioned coupling only.** Khazad-Doom couples to Pi's documented, versioned surfaces such as CLI flags, JSON event streams, and exit codes; it must not depend on Pi internals. `src/pi_contract.rs` is the only module that may parse Pi stdout/stderr or recognize Pi event/error strings; the current contract inventory is `docs/design/pi-contract-inventory.md`. A Pi behavior change may degrade observability, but daemon-owned state remains authoritative for correctness. Unknown fields/events from Pi are tolerated and surfaced as bounded warnings.
- **D5 — Single verification owner.** The daemon owns verification, gates, economics, and attestation. Workers produce evidence claims; daemon checks/gates or human review attest them.
- **D6 — Feedback stays daemon-owned and explicit.** Operators must be able to discover progress and needs-attention states through `status`, `watch`, `monitor`, and `attend`; those surfaces show the same daemon-side feed projection, attention items, deadlines, and exact commands. Herdr may be the optional-default live cockpit for visible workspaces/panes when available, but it is a painter/launcher over daemon-owned state, not a workflow owner. Any Pi feedback adapter is a thin bridge over daemon feed and Herdr focus/open actions; it must not emulate a full live cockpit or replace core monitoring.
- **D7 — Versioned cockpit coupling only.** Khazad-Doom may couple to Herdr only through documented Herdr CLI/session/workspace/tab/pane/agent surfaces wrapped by the `Cockpit` seam. Herdr pane layout, scrollback, and agent-status metadata are observability signals, not daemon truth; Herdr failures may degrade cockpit visibility but must not degrade workflow correctness. Native Herdr-hosted Pi TUI workers are the default when cockpit placement is available; correctness comes only from daemon-owned `submit_worker_result` / `kd_tui_result_artifact` results, or from legacy wrapper stdout/stderr/exit/result artifacts when the wrapper path is explicitly selected or used as fallback. Painters are display-only. The Dashboard pane paints daemon monitor/feed data; bounded operator answers come through the worker pane's Pi `ask_operator` dialog or explicit daemon IPC clients such as `khazad-doom answer`, not an alternate truth store. Pane renames on terminal completion are hygiene only.

Standing rejections:

- Multi-harness worker support is removed from the vision. Revisit only when a concrete second real harness has a user.
- Pi acceptance gates (`attested`, `checked`, `verified`, `reviewed`) are rejected unless daemon verification is retired.
- `fallbackModels` or other silent worker-model failover is rejected unless provider-outage incidents recur and attestation records the actual model per attempt.
- Auto-login and credential mutation are rejected as outside Khazad-Doom's trust boundary; remediation is explicit operator action.

## Roadmap, plan, and finding truth

- Slice JSON plus daemon/run state are the live source of truth for workflow status. Roadmap documents, matrices, and workpackages may summarize or reference that state, but they must not become a competing acceptance or closure ledger. Any roadmap status that cannot be derived from slice/run evidence must be labeled as a planning/audit note rather than accepted workflow truth. `scripts/roadmap-truth-check` enforces that roadmap rows cannot claim done/closed/accepted status without closed slice JSON and named daemon report evidence.
- Implementation work that lands outside Khazad-Doom during normal operation must receive an explicit disposition: Khazad-run, legitimate exemption, freeze or operational exception, or bypass/failure evidence. A silent hand-made product commit is itself workflow-governance evidence until disposed.
- Manual integration disposition: commits from the `kd-20260707-112420-7f061d72` turbulence were manually integrated because that run was superseded by attention/read-model authority defects and was intentionally not resumed. This is a bypass/failure evidence disposition, not evidence that the superseded run completed or was resumed.
- Plan and queue revisions are durable workflow facts, not silent edits. Any accepted change to remaining slice set, order, dependencies, areas, acceptance, verification, or close state records what changed, why, who/what proposed it, who authorized it, and which evidence caused it. Accepted proposal records may grant bounded `authorized_paths`/`action_class` authority to the source slice; worker prompts and daemon path guards honor those grants, not free-form prose. Silent `.workflow/slices/` edits during an active run are not a valid replan mechanism.
- Replan v1 is proposal-only: pending proposals and accepted/rejected/deferred/superseded decisions are daemon state, status/feed render exact decision commands, accepted decisions record whether anything was applied, and the auto-approvable mutation tier is empty.
- Findings that ask for operator intent or propose changes to scope, verification, queue, or workflow policy must reach exactly one terminal disposition: answered, folded into a slice or plan revision, explicitly deferred with revisit condition, or rejected with rationale. Handoffs and reports must not leave such findings as ambient unowned text.

## Run lifetime and ownership

- A run is a durable daemon-owned session keyed by `run_id`. CLI commands and Pi worker tools start, control, or observe that session; they do not define its lifetime.
- No hidden global workflow timeout exists by default. A run continues until it reaches a terminal state, is cancelled, or is marked interrupted/recovered by daemon startup logic.
- Time limits are explicit policy knobs for specific work: verification/gate command timeouts and, when configured, per-worker-attempt timeouts.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, progress snapshots, status/monitor output, handoff JSON, and artifact inspection.

## Slice lifecycle and integration

- JSON Issue Slices are the authoritative work contract. Open slices represent runnable work; closed slices represent accepted historical work.
- JSON Issue Slices are bounded intent contracts, not frozen implementation plans. Acceptance criteria are minimum evidence, not an exhaustive case inventory. Learning is allowed inside the JSON fence; moving the fence requires approval. During an open slice, TDD/code-inspection discoveries directly implied by the slice goal or acceptance and inside declared `areas` may be implemented with the smallest clear change and reported in worker output. Discoveries that require new product intent, public API semantics, dependencies, verification policy, or paths outside `areas` must become `ask-user` blockers or follow-up slices.
- Slice `areas` are repo-relative literal path prefixes, not globs. Directory prefixes should use a trailing slash (for example `src/normia/`); exact files are written as file paths (for example `README.md`). Validation rejects glob characters, parent traversal, absolute paths, leading/trailing whitespace, and leading `./` before workers launch.
- Requested open slices include open dependencies before dependents. Closed dependencies are treated as satisfied and must not launch historical workers again. Explicitly requesting a closed slice is rejected; create a follow-up slice for new work.
- Cycles are invalid across the slice graph.
- Each worker attempt runs one open slice in a daemon-managed isolated worktree. Parallel workers must not share a checkout.
- A completed worker must return valid JSON, commit intended changes, and leave its worktree clean before the daemon may integrate the slice.
- Worker `acceptance_status` entries are evidence claims, not approvals. A worker must not approve its own evidence; daemon-owned checks/gates or later human review attest or reject it separately.
- Independent slices may execute concurrently, but integration into the run branch is serial.
- A parallel dependency layer is integration-atomic: Khazad-Doom joins every spawned worker in the active batch/layer and records deterministic per-slice outcomes before deciding whether the layer may merge. No successful worker from a layer is merged after a sibling fails or blocks.
- After each successful integration merge, Khazad-Doom records a checkpoint before advancing, so `resume` can continue from recorded state instead of replaying completed merges.
- Merge conflicts, `ask-user` findings, invalid worker output, dirty worktrees, scope violations, and verification failures become structured blocked/failed artifacts rather than silent best-effort integration.
- The integration gate runs before integration repair. With `integration_repair: "auto"`, repair is only launched after failed gate evidence that is not an operator environment failure; with `"never"`, failed gate evidence is surfaced without repair; with `"always"`, repair may run even after a passing gate for explicit policy reasons. Operator environment failures block with evidence instead of launching repair.
- Repair never bypasses or weakens the gate: whenever repair runs, the daemon reruns the integration gate and only treats the run as successful after the post-repair gate passes.
- Repair workers are not privileged policy mutators. Integration repair may work only inside the already-authorized slice set and gate evidence: changing workflow policy, worker profiles, verification commands, slice contracts, dependencies, or paths outside the authorized areas requires an operator-approved plan revision or follow-up slice.
- After a run passes the integration gate, the daemon publishes completion atomically from the operator's perspective: it closes completed slice JSON in the integration branch with `status: "closed"`, `closed_by_run`, and `closed_at`, writes daemon-owned implementation/final report artifacts, commits those publication changes, and only then advertises the final handoff SHA. Handoff JSON plus transient implementation-summary/final-report outputs derive `final_sha` from the integration branch tip after that publication commit, so branch-level handoff commands name a commit carrying implementation changes, close records, and report artifacts together. Missing slice metadata records a warning incident and preserves handoff readiness; slice close read/write failures record error incidents and block handoff readiness. Re-running publication after the final report artifacts already exist is idempotent and must not create duplicate close/report commits.

## Worker attempt supervision

- Attempt history is append-only evidence. Retries add attempts and preserve previous output/failure context.
- Deterministic operator-class worker launch failures, such as Pi provider authentication failures detected by a narrow no-assistant-output plus known stderr signature, block after the first attempt and must not consume the remaining worker retries. Unknown or ambiguous launch failures preserve the existing retry behavior.
- Operator-class launch incidents include `failure_kind`, `retryable`, `operator_action_required`, agent provider/model/profile metadata, and `fix_commands` so status, monitor, reports, and handoffs can surface the same remediation without scraping daemon stderr.
- The effective worker profile is resolved once by the profile module from CLI/env, `.workflow/khazad.json` agent choice, operator-global `~/.khazad-doom/agents.toml`, and built-in defaults. Repo-local `.workflow/agents.toml` is not a runtime input. Pi launch args, `RunnerMetadata`, `profile_summary`, `launch_summary`, source attribution, and auth fix guidance derive from that result; worker surfaces must not assemble provider/model text independently.
- Worker execution is at-least-once, not exactly-once. A timed-out, cancelled, or retried attempt may have produced files or commits in its isolated worktree.
- Worker operator questions are durable daemon state. `workerAsk`/`workerAskOpen` require the per-run `KHAZAD_WORKER_TOKEN`; token validation happens in daemon IPC, not only in the Pi extension. In native TUI mode, `ask_operator` opens the daemon question and prompts in the same worker Pi session, then records the answer through `answerQuestion`. `answerQuestion` is operator-side and rejects interrupted/cancelled runs rather than silently storing an answer the worker cannot see. The Pi monitor bridge is read-only for questions; CLI/dashboard answer commands remain explicit fallback/debug paths.
- While a worker is paused in `awaiting_operator`, the slice remains `Running`; no `SliceStatus` enum value is added for questions. Fatal worker-attempt timeout accounting excludes the paused interval so a slow operator answer does not consume a retry.
- Parallel worker cancellation is graceful-first. If a run cancellation or sibling layer failure happens while a parallel batch is active, Khazad-Doom propagates cancellation to active workers and still joins every worker thread before the layer returns.
- Process liveness and output activity are distinct. `Supervisor: alive` means the daemon still observes the child process, not that semantic progress is guaranteed.
- Quiet-worker warnings are advisory. Missing output alone is not terminal unless an explicit timeout/policy makes it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker-attempt timeout is an explicit repo/operator policy and applies to an attempt, not to the whole run lifetime.
- `worker_question_timeout_seconds: 0` means pending operator questions wait indefinitely. Any nonzero value is the daemon-owned question timeout shown in status/feed/monitor/attend and recorded with the worker question.

## Cancellation, interruption, and resume

- `cancel` requests cancellation through daemon state and worker process signalling; it is an explicit operator action, not a side effect of closing a monitor, status follower, or CLI session.
- If the daemon starts and discovers active runs from a previous process, it marks them `interrupted`, records recovery events, and cleans daemon-managed worktrees where possible.
- `resume` is explicit. It uses durable checkpoint/run state for remaining work and never claims to resurrect a lost worker process.

## Verification and gates

- Slice `verify` commands and configured verification profile commands are gates for completion/integration; they must run with their declared repo-relative context and environment. Optional `worktree_setup` commands are daemon-owned bootstrap steps for each worker/integration worktree, run without verification-cache reuse, and must leave the git worktree clean except ignored files. Missing tools, invalid verify cwd, shell spawn failures, and non-executable commands are classified as daemon/operator environment failures rather than worker auto-fix failures.
- Gate command plans preserve profile insertion order, merge exact duplicate commands within a gate, and may fail fast after the first gate failure when `gate_fail_fast` is enabled.
- The workflow manager owns lifecycle ordering, retries, repair decisions, checkpointing, and state transitions; the workflow gate/shell seam owns only command resolution/execution details and returns typed check/gate results.
- Verification and gate timeouts are per-command hang protection. They are not global workflow timeouts and must not be reused to cap total run lifetime.
- Gate failures are reported with command evidence and must be repaired or surfaced as blocked/failed before handoff.
- Terminal `blocked` and `failed` states must include structured primary-reason data, not only prose: reason kind, resolution owner, retryability/operator-action flags where applicable, evidence links, and remediation or disposition links. `status` JSON exposes this as `primary_terminal_reason` and the versioned daemon `feed` mirrors it as `feed.terminal_reason` plus exact `feed.operator_commands`; monitor/watch/Pi renderers paint those blocks instead of reclassifying raw text.
- Final reports and handoff JSON expose explicit `exit_states`, `evidence_attestation`, and `plan_revisions` as read-only summaries of existing lifecycle state. They must not introduce hidden gates, extra worker turns, or a second source of truth. Pending replan proposals block handoff readiness until an operator records an approved disposition; accepted/rejected/deferred/superseded proposals remain visible with evidence and rationale.
- Status/watch/monitor snapshots and final reports include runtime economics: agent calls, daemon-owned command executions, cache hit/miss counts, repair policy/attempts, phase durations, duplicate-command telemetry, and SLA violations.
- Completed runs may still have incidents. Resume events, prior run errors, cleanup issues, integration repairs, and non-fatal lifecycle warnings must remain visible as run incidents instead of being hidden by a final `completed` status.
- Every terminal run writes `.workflow/runs/<run>/outputs/run-summary.json` before daemon worktree cleanup and before the terminal state is advertised. Failed/cancelled/blocked summaries retain primary failure or cancel reason plus bounded worktree/attempt diagnostics where available; they are not committed reports.
- If run start records an optional `.workflow/runs/<run>/origin.json` target, terminal feedback is emitted only after that terminal summary exists, only for completed/blocked/failed/cancelled transitions, and only as inert declarative evidence through the Cockpit Herdr send seam. Dedupe records are per terminal transition so a resumed run can separately report blocked and completed while retry paths do not duplicate one transition. Pending worker questions and replan decisions may also send/focus visibility-only attention messages through `herdr agent send` / `herdr agent focus`; failures are recorded as incidents only. Runs without an origin target do not notify. Missing Herdr, malformed/stale recorded targets, or send/focus failures are warning-level visibility evidence and must not affect status, verification, merge, handoff readiness, or final SHA. Interrupted is excluded in v1 because daemon-restart recovery can stale the origin target.
- Run start is clean-by-default: the source repo dirty state and base branch/SHA are captured in `preflight.json`, and dirty starts require explicit `--allow-dirty`.

## Progress, status, and monitor state

- The daemon/state store is the source of truth for run status, slice states, events, and live progress snapshots.
- `status`, `watch`, and `monitor` render the same daemon state. They must not own workflow state or infer cancellation from UI/session shutdown.
- Status interpretation is centralized daemon-side in the status feed projection. Renderers are painters: they may choose layout/color, but not invent different wording or re-interpret daemon event payloads independently. The CLI monitor/watch/attend paths and explicit Pi `/khazad-attach` / `/khazad-explain` adapter actions paint `RunDetails.feed`; when feed data is unavailable, they report feed unavailability rather than rebuilding workflow wording from raw events. The `feed.attention` list is first-class and must render full, untruncated pending-question/proposal text, options, exact commands, and deadlines.
- `monitor --latest` must not make terminal runs disappear. When no active run exists, it keeps the latest terminal run summary visible, including incidents and handoff readiness.
- Progress output may distinguish supervisor liveness, worker process state, last output event, human last-semantic-progress summary, configured timeouts, and advisory quiet-worker warnings.
- When a parallel worker layer is active, status/watch/monitor output exposes the layer explicitly and lists the active slice IDs in deterministic order.
- The Herdr cockpit adapter, when enabled, may open/focus run workspaces with a Dashboard pane and deterministically named native Pi TUI worker agents tied to run id, slice id, and attempt. The Dashboard paints daemon monitor/feed data; operator attention uses the worker pane's `ask_operator` Pi dialog or daemon commands such as `khazad-doom attend`; successful native worker panes remain available through terminal-summary hygiene so they can be renamed with outcome markers. Herdr absence, cockpit startup failure, native TUI startup failure, activity-painter exit, unexpected pane rename failure, or wrapper handoff failure before Pi launches falls back to direct execution or degrades visibility only by default and records/displays non-fatal evidence; it must not by itself change run/slice status, worker authorization, verification, merge, or handoff readiness. The Planner Pi pane remains explicitly deferred until RPL planner authority exists.
- Cockpit layout pane identities are not durable authority. Every layout operation resolves a live anchor by role at use time: worker-region placeholder, stable worker slot label, unlabeled fresh root, or Dashboard as a last-resort split base. Closed pane ids must not be treated as empty available panes, and a workspace whose worker region was emptied by timeout/cancel/retry cleanup must remain placeable for the next worker attempt. This invariant is backed by COCKPIT-ANCHOR-01 evidence (`docs/design/evidence/cockpit-anchor-stale-pane-2026-07-08.md`).
- Herdr worker panes/agents are not a freeform authority channel. Native TUI workers must submit through the daemon-owned `submit_worker_result` artifact contract; legacy wrapper workers write stdout/stderr/exit/result artifacts under `.workflow/runs/<run>/outputs/` and feed the same daemon worker-attempt supervision. The daemon never reads worker JSON from pane text, scrollback, or Herdr agent-status metadata. Normal operator control means observe, focus, answer bounded `ask_operator` dialogs, and request daemon-owned actions such as cancel or answer; arbitrary terminal typing into the worker pane is not accepted workflow input, and any manual takeover must be explicit evidence, not silent accepted worker output.
- The Pi feedback adapter is a thin bridge over daemon commands/data and Herdr open/focus actions: start/shape via the CLI, explain from `RunDetails.feed`, summarize handoff JSON, answer blockers through daemon `answerQuestion`, and delegate Herdr focus/open to `khazad-doom cockpit open`. It cleans up all session-bound resources on Pi session replacement/reload, never infers run lifecycle from Pi sessions, never renders a full live multi-agent cockpit, and is never required for core monitoring or headless operation.

## Artifacts, handoffs, and remotes

- Runtime handoffs, raw worker outputs, checkpoints, and inspection artifacts live under `.workflow/runs/` and remain transient/gitignored unless explicitly promoted elsewhere.
- Worker handoff JSON is generated before the worker starts and records the exact slice contract, worktree path, branch, run id, and output path the worker must use.
- `inspect` and blocked/failed artifacts expose bounded diagnostics without requiring maintainers to scrape daemon internals.
- `khazad-doom handoff` prints branch, summary, and suggested push/PR commands by default. It must not mutate remotes unless `--push`, `--create-pr`, or explicit repository configuration requests that behavior; `--dry-run` suppresses configured actions.

## Phase 5 scope amendment record

- **Herdr is the optional-default live cockpit, not a workflow owner.**
  - Accepted invariant text: Herdr may open/focus visible run workspaces and read-only feed/phase panes when available, but daemon state remains authoritative; direct execution remains fallback; Pi is a thin bridge/explainer rather than a rich live dashboard, and planner panes wait for RPL planner authority.
  - Ledger entries: F-013; Phase 1 PI-05 status/monitor drift; rich Pi monitor overlay/feed-widget churn; PUB-01B-era operator scope decision.
  - Enforcement mechanism: FEED-01 projection authority, HERDR-01 cockpit config/fallback, HERDR-02 KD-owned wrapper/result capture, HERDR-03 Pi bridge only.
  - Violation-detecting tests: real-Herdr gated workspace/pane smoke; explicit `cockpit open` real/fallback tests; worker wrapper artifact-capture e2e; package extension tests that ensure Pi adapter feed rendering comes from the daemon projection instead of raw event interpretation.
  - Status: accepted for Phase 5 slices.

## Phase 2 invariant amendment record

This section is the Phase 2 doctrine diff from `REVISION_PLAN.md`; it records why the new or sharpened invariants above exist. It is not a parallel doctrine document.

### Accepted amendments

- **Live roadmap status has one source of truth.**
  - Proposed invariant text: slice JSON plus daemon/run state are the live source of truth; roadmap docs may summarize but must not become a competing acceptance or closure ledger.
  - Ledger entries: F-001, F-004, F-014; Phase 1 roadmap truth audit.
  - Enforcement mechanism: generated or linted matrix status from `.workflow/slices/*.json` plus run/close metadata, or a slice-close check that validates the matrix row before handoff.
  - Violation-detecting test: a fixture where a matrix/workpackage status disagrees with slice JSON or named run evidence must fail the roadmap-status lint/check.
  - Status: accepted.
- **Work outside Khazad-Doom needs explicit disposition.**
  - Proposed invariant text: implementation work that bypasses Khazad-Doom is classified as Khazad-run, legitimate exemption, freeze/operational exception, or bypass/failure evidence.
  - Ledger entries: F-001, F-014.
  - Enforcement mechanism: release/revision checklist or commit-audit lint comparing product commits to run evidence and recorded exceptions.
  - Violation-detecting test: a product commit after the last Khazad-run closure with no recorded disposition is reported by the audit/lint.
  - Status: accepted.
- **Plan and queue revisions are durable facts.**
  - Proposed invariant text: accepted changes to slice set, order, dependencies, areas, acceptance, verification, or close state record change, rationale, proposer, authorizer, and evidence; silent active-run slice edits are invalid.
  - Ledger entries: F-004, F-008, F-009.
  - Enforcement mechanism: Phase 3 replan checkpoint model with daemon-recorded revision events/artifacts and status/handoff rendering.
  - Violation-detecting test: a queue mutation without a revision record is rejected or surfaced as an invariant failure; accepted/rejected revisions appear in status and handoff fixtures.
  - Status: accepted; concrete mechanism deferred to the Phase 3 RFC.
- **Actionable findings need terminal disposition.**
  - Proposed invariant text: findings that ask for intent or propose scope/verification/queue/policy changes end as answered, folded into a slice/revision, explicitly deferred, or rejected.
  - Ledger entries: F-003, F-006, F-008.
  - Enforcement mechanism: worker-output/repair-output schemas require `finding_dispositions`; successful outputs with actionable findings and no terminal disposition or pending daemon-created proposal fail validation before handoff readiness.
  - Violation-detecting test: a worker or repair output with an actionable finding and no disposition fails validation or marks the run/handoff as unresolved.
  - Status: accepted; RPL-02 enforcement implemented for worker and integration-repair outputs.
- **Repair authority is bounded by existing authorization.**
  - Proposed invariant text: repair workers may not mutate workflow policy, profiles, verification, slice contracts, dependencies, or paths outside authorized areas without an operator-approved revision/follow-up.
  - Ledger entries: F-003.
  - Enforcement mechanism: repair prompt contract, repair change-scope checks against authorized slice areas and protected workflow paths, daemon-created replan proposal records for exceptions, and reset of unapplied repair revisions.
  - Violation-detecting test: an integration repair fixture that changes `.workflow/khazad.json`, profiles, slice JSON, or out-of-area files is blocked unless paired with an approved revision.
  - Status: accepted; RPL-02 records proposal evidence instead of applying out-of-authority repair mutations.
- **Invalid worker outputs are durable attempt evidence.**
  - Proposed invariant text: invalid worker JSON, schema mismatches, and missing output are preserved before retry with attempt number, slice id, bounded raw payload or parse error, and transcript/output tails when available.
  - Ledger entries: F-006; post-Herdr invalid-output evidence gap.
  - Enforcement mechanism: daemon writes `*.worker.attempt-N.invalid-output.json`, records `invalid_worker_output` events, and updates slice attempts before retry/fail.
  - Violation-detecting test: invalid JSON and schema retry fixture preserves artifacts/events and counts all attempts/economics.
  - Status: accepted; RPL-02 implemented.
- **Bounded worker-attempt recovery separates evidence-envelope repair from implementation authority.**
  - Proposed invariant text: invalid/missing/schema-invalid worker JSON may consume a small envelope re-emission budget against the existing worker head without burning implementation attempts; daemon-owned mechanical slice verify failures may get at most one targeted in-scope slice-repair attempt after normal attempts would otherwise become terminal; scope violations remain hard failures or RPL-02B proposal/grant cases, and ready siblings from a failed parallel layer remain preserved-but-unmerged evidence. A false positive that auto-repairs beyond authority is worse than a false negative that blocks.
  - Ledger entries: F-015; dogfood run `kd-20260707-153202-9f41ac7c` terminal worker evidence-envelope failure; CPLX-03 ready sibling discarded under preserved layer atomicity; durable invalid-worker-output artifacts/events for CPLX-04.
  - Enforcement mechanism: typed `worker_attempt_failure` events with attempt, slice id, evidence path, retry disposition, and repair disposition; default two envelope retries; one targeted slice-repair attempt only for `command_failed` slice checks; existing path guards for scope violations and RPL-02B accepted grants; parallel-layer failure outcomes include preserved-unmerged branch/commit evidence.
  - Violation-detecting test: fake runner sequence invalid envelope output -> scope violation -> mechanical verify failure -> targeted repair succeeds, plus a final-envelope-failure fixture that preserves invalid-output artifacts/events through exhaustion.
  - Status: accepted; REPAIR-01 implemented.
- **Terminal blocked/failed states carry structured reason data.**
  - Proposed invariant text: `blocked`/`failed` are not enough; terminal artifacts include primary reason kind, resolution owner, retryability/operator-action flags where applicable, evidence links, and remediation/disposition links.
  - Ledger entries: F-004, F-006, F-009.
  - Enforcement mechanism: terminal summary/report/handoff schema plus status projection rendering from the structured reason.
  - Violation-detecting test: blocked/failed run fixtures missing `primary_reason.kind` or equivalent structured data fail schema/projection tests.
  - Status: accepted.

### Explicit deferrals tested against evidence

- **Runtime mission object.** Status: explicitly_deferred. Current evidence shows slice/queue/status truth gaps, not a need for a new runtime mission abstraction. Reconsider only if the Phase 3 RFC proves that recorded slice revisions cannot express the operator's durable intent.
- **Daemon-internal autonomous replan engine.** Status: explicitly_deferred. F-008 proves operators need trustworthy queue visibility and recorded revision points; it does not prove the daemon should generate plans. Reconsider only after repeated recorded findings show a mechanical replan pattern that humans approve unchanged.
- **Automated planner authority to mutate queues.** Status: explicitly_deferred. Queue mutation changes intent and remains operator-authorized unless Phase 3 defines a narrow auto-approvable tier with evidence and rollback semantics. Reconsider only with production evidence that manual approval is the bottleneck and accepted changes are mechanically safe.
- **Auto-blocking complexity telemetry.** Status: explicitly_deferred. Complexity signals may be advisory report context in a future slice, but no ledger entry proves a metric that should block work automatically. Reconsider only if repeated failures correlate with a daemon-computable threshold and false-positive cost is understood.
- **Advisory complexity telemetry (pre-registered survive hypothesis).** Status: explicitly_deferred. Tested against the ledger: no entry shows a failure that daemon-computed diff/dependency/module deltas would have caught, or that episodic manual audits missed; F-011 only obligates preserving proven throughput. Advisory deltas remain a candidate redesign slice for report/economics surfaces, never an invariant. Reconsider when the Phase 4 architecture review or a repeated ledger pattern shows complexity regressions that manual audits fail to catch.

## Frontier autonomy proposed invariant amendment records

These records are proposed by AF-00. They are not implemented runtime behavior until the corresponding AF slices land and the operator accepts them through normal review.

- **Mission envelopes bound delegated autonomy; they are not workflow truth.**
  - Proposed invariant text: a `MissionEnvelope` is a daemon-owned per-run authorization record with `goal`, `allowed_areas`, `non_goals`, `verify_profile`, `max_auto_promotions`, `max_depth`, `max_generated_slices`, `autonomy_level`, and `must_ask_if`. It bounds only envelope-delegated decision recording for not-yet-existing follow-up slices; slice JSON plus daemon/run state remain the live source of workflow truth, and an absent envelope means `autonomy_level=off`.
  - Ledger entries: AF-00; auto-frontier AD3/AD4; Phase 2 runtime mission object deferral reconsider condition ("recorded slice revisions cannot express the operator's durable intent").
  - Enforcement mechanism: AF-02 durable envelope serde/defaults/validation, area-contract validation for `allowed_areas`, nonnegative budget validation, shared status/watch/monitor/report/handoff rendering from daemon run state, and downgrade-to-off behavior for old runs with no envelope.
  - Violation-detecting tests: invalid area strings, negative budgets, unknown `autonomy_level`, and unknown `verify_profile` are rejected before worker launch; restart/resume preserves the envelope and budget counters; status/watch/monitor/report/handoff snapshots render the same envelope; old-run fixtures without an envelope behave as `off`.
  - Status: proposed; acceptance of AF-00 is the doctrine gate, with implementation deferred to AF-02 and later slices.
- **Envelope-delegated auto-acceptance uses the existing replan channel.**
  - Proposed invariant text: every generated follow-up candidate is an `add_followup_slice` replan proposal. There is no separate frontier authority channel. At `promote` or `run`, the daemon may record an accept decision only for Tier-1 proposals inside the active MissionEnvelope, within budget, after the RFC evidence bars are met; the decision records `authorizer: "envelope:<run_id>"`, `source: "frontier_policy"`, tier, and stable reason codes. The same idempotent apply engine used for operator accepts applies the decision. Rejected or deferred proposals are never auto-reapplied.
  - Ledger entries: AF-00; auto-frontier AD1/AD2/AD3/AD6; Phase 2 automated planner authority deferral reconsider condition ("production evidence that manual approval is the bottleneck and accepted changes are mechanically safe"); Phase 2 daemon-internal autonomous replan engine deferral reconsider condition ("repeated recorded findings show a mechanical replan pattern that humans approve unchanged").
  - Enforcement mechanism: AF-03 pure `promotion_policy::classify_followup_proposal` classifier with stable reason codes, explicit graph/budget/envelope inputs only, and no filesystem/git/state/IPC/clock/worker access; AF-04 calls it only to record shadow measurements on existing replan proposals before production enablement; AF-05 idempotent follow-up-slice apply path; AF-06 decision writer that records envelope authorizer data and calls only the AF-05 apply engine.
  - Violation-detecting tests: classifier table tests classify inside-envelope proposals as Tier 1 and outside-area, dependency-changing, non-goal, rejected/deferred-duplicate, and `must_ask_if` cases as Tier 3; shadow-mode tests record would-have decisions without queue or `.workflow/slices/` mutation and preserve event history across restart; fake-runner e2e proves Tier-1 auto-accept uses the replan decision/apply records; a previously rejected/deferred proposal cannot be auto-accepted.
  - Status: proposed; auto-acceptance remains unavailable until AF-04 evidence satisfies the RFC's promote/run numeric bars and AF-05/AF-06 implement the shared apply path.
- **Generated follow-up slices carry provenance and reports derive the graph.**
  - Proposed invariant text: any slice JSON generated from a follow-up proposal must include `provenance` with `parent_slice_id`, `origin_proposal_id`, `generation`, `created_by`, and `created_at` before a worker can run it. Promotion decisions record authorizer, source, tier, reason codes, budget counters, queue snapshot hashes, and apply checkpoints. Reports and handoffs derive the promotion graph from proposal, decision, queue, and slice records; they must not become a second truth store.
  - Ledger entries: AF-00; auto-frontier AD1/AD4; Phase 2 live roadmap truth and plan/queue revision amendments; RPL-01/RPL-03 replan attestation doctrine.
  - Enforcement mechanism: AF-01 records typed follow-up draft payloads through replan and makes slice JSON/schema accept optional provenance, AF-05 writes generated slice JSON with provenance and commits it before dispatch, AF-07 report/handoff generation derives graphs from recorded proposal/decision/slice state, and roadmap truth lint treats generated slices like handwritten slices.
  - Violation-detecting tests: applying a follow-up without required provenance fails; generated-slice closure without parent/proposal/generation evidence fails report or handoff snapshot validation; restart during apply resumes idempotently without duplicating slice files; roadmap truth fixtures pass for generated closed slices and fail when generated status lacks named run/proposal evidence.
  - Status: proposed; implementation deferred to AF-01, AF-05, and AF-07.

## Release and tag safety

- Release creation is an explicit maintainer action, currently by pushing a `v*` tag for CI. Daemon runs and default handoffs do not create or push release tags.
- Release/package refactors must preserve the workflow gate: validated slices, committed worker changes, serial integration, passing verification, and explicit handoff before any remote or tag mutation.
