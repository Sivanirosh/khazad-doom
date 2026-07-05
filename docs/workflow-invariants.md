# Workflow invariants

These invariants define the daemon-owned workflow behavior that v0.1.0 release-polish refactors must preserve. They are intentionally phrased as testable contracts; changing one is a public workflow behavior change, not a cleanup.

## Product doctrine

- **D1 — Pi-first commitment.** Pi is the only real worker harness. `FakeRunner` stays as the deterministic test double, justified by testing, not portability. Daemon state remains harness-neutral JSON; worker execution is Pi-native.
- **D2 — Truthful environmental failure.** Deterministic environment/launch failures block immediately with operator guidance. They must not burn retries or masquerade as implementation failures.
- **D3 — Escalation over termination.** A worker that hits a `must_ask_if` condition should call the shipped `ask_operator` Pi tool, pause in `awaiting_operator`, and continue after an answer. If the tool is unavailable or times out, the worker falls back to the existing `ask-user` blocked output.
- **D4 — Versioned coupling only.** Khazad-Doom couples to Pi's documented, versioned surfaces such as CLI flags, JSON event streams, and exit codes; it must not depend on Pi internals. `src/pi_contract.rs` is the only module that may parse Pi stdout/stderr or recognize Pi event/error strings; the current contract inventory is `docs/design/pi-contract-inventory.md`. A Pi behavior change may degrade observability, but daemon-owned state remains authoritative for correctness. Unknown fields/events from Pi are tolerated and surfaced as bounded warnings.
- **D5 — Single verification owner.** The daemon owns verification, gates, economics, and attestation. Workers produce evidence claims; daemon checks/gates or human review attest them.
- **D6 — Feedback stays daemon-owned and explicit.** Operators must be able to discover progress and needs-attention states through `status`, `watch`, and `monitor`; those surfaces show the same daemon-side feed projection and answer commands. Any Pi feedback adapter is an explicit, read-only painter over that daemon feed and must not replace core monitoring.

Standing rejections:

- Multi-harness worker support is removed from the vision. Revisit only when a concrete second real harness has a user.
- Pi acceptance gates (`attested`, `checked`, `verified`, `reviewed`) are rejected unless daemon verification is retired.
- `fallbackModels` or other silent worker-model failover is rejected unless provider-outage incidents recur and attestation records the actual model per attempt.
- Auto-login and credential mutation are rejected as outside Khazad-Doom's trust boundary; remediation is explicit operator action.

## Run lifetime and ownership

- A run is a durable daemon-owned session keyed by `run_id`. CLI commands and Pi worker tools start, control, or observe that session; they do not define its lifetime.
- No hidden global workflow timeout exists by default. A run continues until it reaches a terminal state, is cancelled, or is marked interrupted/recovered by daemon startup logic.
- Time limits are explicit policy knobs for specific work: verification/gate command timeouts and, when configured, per-worker-attempt timeouts.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, progress snapshots, status/monitor output, handoff JSON, and artifact inspection.

## Slice lifecycle and integration

- JSON Issue Slices are the authoritative work contract. Open slices represent runnable work; closed slices represent accepted historical work.
- JSON Issue Slices are bounded intent contracts, not frozen implementation plans. Acceptance criteria are minimum evidence, not an exhaustive case inventory. Learning is allowed inside the JSON fence; moving the fence requires approval. During an open slice, TDD/code-inspection discoveries directly implied by the slice goal or acceptance and inside declared `areas` may be implemented with the smallest clear change and reported in worker output. Discoveries that require new product intent, public API semantics, dependencies, verification policy, or paths outside `areas` must become `ask-user` blockers or follow-up slices.
- Requested open slices include open dependencies before dependents. Closed dependencies are treated as satisfied and must not launch historical workers again. Explicitly requesting a closed slice is rejected; create a follow-up slice for new work.
- Cycles are invalid across the slice graph.
- Each worker attempt runs one open slice in a daemon-managed isolated worktree. Parallel workers must not share a checkout.
- A completed worker must return valid JSON, commit intended changes, and leave its worktree clean before the daemon may integrate the slice.
- Worker `acceptance_status` entries are evidence claims, not approvals. A worker must not approve its own evidence; daemon-owned checks/gates or later human review attest or reject it separately.
- Independent slices may execute concurrently, but integration into the run branch is serial.
- A parallel dependency layer is integration-atomic: Khazad-Doom joins every spawned worker in the active batch/layer and records deterministic per-slice outcomes before deciding whether the layer may merge. No successful worker from a layer is merged after a sibling fails or blocks.
- After each successful integration merge, Khazad-Doom records a checkpoint before advancing, so `resume` can continue from recorded state instead of replaying completed merges.
- Merge conflicts, `ask-user` findings, invalid worker output, dirty worktrees, scope violations, and verification failures become structured blocked/failed artifacts rather than silent best-effort integration.
- The integration gate runs before integration repair. With `integration_repair: "auto"`, repair is only launched after failed gate evidence; with `"never"`, failed gate evidence is surfaced without repair; with `"always"`, repair may run even after a passing gate for explicit policy reasons.
- Repair never bypasses or weakens the gate: whenever repair runs, the daemon reruns the integration gate and only treats the run as successful after the post-repair gate passes.
- After a run passes the integration gate, the daemon closes completed slice JSON in the integration branch with `status: "closed"`, `closed_by_run`, and `closed_at` before writing final reports.

## Worker attempt supervision

- Attempt history is append-only evidence. Retries add attempts and preserve previous output/failure context.
- Deterministic operator-class worker launch failures, such as Pi provider authentication failures detected by a narrow no-assistant-output plus known stderr signature, block after the first attempt and must not consume the remaining worker retries. Unknown or ambiguous launch failures preserve the existing retry behavior.
- Operator-class launch incidents include `failure_kind`, `retryable`, `operator_action_required`, agent provider/model/profile metadata, and `fix_commands` so status, monitor, reports, and handoffs can surface the same remediation without scraping daemon stderr.
- The effective worker profile is resolved once by the profile module from CLI/env, `.workflow/khazad.json` agent choice, operator-global `~/.khazad-doom/agents.toml`, and built-in defaults. Repo-local `.workflow/agents.toml` is not a runtime input. Pi launch args, `RunnerMetadata`, `profile_summary`, `launch_summary`, source attribution, and auth fix guidance derive from that result; worker surfaces must not assemble provider/model text independently.
- Worker execution is at-least-once, not exactly-once. A timed-out, cancelled, or retried attempt may have produced files or commits in its isolated worktree.
- Worker operator questions are durable daemon state. `workerAsk` requires the per-run `KHAZAD_WORKER_TOKEN`; token validation happens in daemon IPC, not only in the Pi extension. `answerQuestion` is operator-side and rejects interrupted/cancelled runs rather than silently storing an answer the worker cannot see.
- While a worker is paused in `awaiting_operator`, the slice remains `Running`; no `SliceStatus` enum value is added for questions. Fatal worker-attempt timeout accounting excludes the paused interval so a slow operator answer does not consume a retry.
- Parallel worker cancellation is graceful-first. If a run cancellation or sibling layer failure happens while a parallel batch is active, Khazad-Doom propagates cancellation to active workers and still joins every worker thread before the layer returns.
- Process liveness and output activity are distinct. `Supervisor: alive` means the daemon still observes the child process, not that semantic progress is guaranteed.
- Quiet-worker warnings are advisory. Missing output alone is not terminal unless an explicit timeout/policy makes it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker-attempt timeout is an explicit repo/operator policy and applies to an attempt, not to the whole run lifetime.

## Cancellation, interruption, and resume

- `cancel` requests cancellation through daemon state and worker process signalling; it is an explicit operator action, not a side effect of closing a monitor, status follower, or CLI session.
- If the daemon starts and discovers active runs from a previous process, it marks them `interrupted`, records recovery events, and cleans daemon-managed worktrees where possible.
- `resume` is explicit. It uses durable checkpoint/run state for remaining work and never claims to resurrect a lost worker process.

## Verification and gates

- Slice `verify` commands and configured verification profile commands are gates for completion/integration; they must run with their declared repo-relative context and environment. Missing tools, invalid verify cwd, shell spawn failures, and non-executable commands are classified as daemon/operator environment failures rather than worker auto-fix failures.
- Gate command plans preserve profile insertion order, merge exact duplicate commands within a gate, and may fail fast after the first gate failure when `gate_fail_fast` is enabled.
- The workflow manager owns lifecycle ordering, retries, repair decisions, checkpointing, and state transitions; the workflow gate/shell seam owns only command resolution/execution details and returns typed check/gate results.
- Verification and gate timeouts are per-command hang protection. They are not global workflow timeouts and must not be reused to cap total run lifetime.
- Gate failures are reported with command evidence and must be repaired or surfaced as blocked/failed before handoff.
- Final reports and handoff JSON expose explicit `exit_states` and `evidence_attestation` as read-only summaries of existing lifecycle state. They must not introduce hidden gates, extra worker turns, or a second source of truth.
- Status/watch/monitor snapshots and final reports include runtime economics: agent calls, daemon-owned command executions, cache hit/miss counts, repair policy/attempts, phase durations, duplicate-command telemetry, and SLA violations.
- Completed runs may still have incidents. Resume events, prior run errors, cleanup issues, integration repairs, and non-fatal lifecycle warnings must remain visible as run incidents instead of being hidden by a final `completed` status.
- Every terminal run writes `.workflow/runs/<run>/outputs/run-summary.json` before daemon worktree cleanup and before the terminal state is advertised. Failed/cancelled/blocked summaries retain primary failure or cancel reason plus bounded worktree/attempt diagnostics where available; they are not committed reports.
- Run start is clean-by-default: the source repo dirty state and base branch/SHA are captured in `preflight.json`, and dirty starts require explicit `--allow-dirty`.

## Progress, status, and monitor state

- The daemon/state store is the source of truth for run status, slice states, events, and live progress snapshots.
- `status`, `watch`, and `monitor` render the same daemon state. They must not own workflow state or infer cancellation from UI/session shutdown.
- Status interpretation is centralized daemon-side in the status feed projection. Renderers are painters: they may choose layout/color, but not invent different wording or re-interpret daemon event payloads independently. The CLI monitor/watch paths and Pi `/khazad-attach` adapter prefer `RunDetails.feed` when present.
- `monitor --latest` must not make terminal runs disappear. When no active run exists, it keeps the latest terminal run summary visible, including incidents and handoff readiness.
- Progress output may distinguish supervisor liveness, worker process state, last output event, last semantic progress, configured timeouts, and advisory quiet-worker warnings.
- When a parallel worker layer is active, status/watch/monitor output exposes the layer explicitly and lists the active slice IDs in deterministic order.
- The Pi feedback adapter is explicit attach only, read-only over the daemon projection, cleans up all session-bound resources on Pi session replacement/reload, and is never required for core monitoring.

## Artifacts, handoffs, and remotes

- Runtime handoffs, raw worker outputs, checkpoints, and inspection artifacts live under `.workflow/runs/` and remain transient/gitignored unless explicitly promoted elsewhere.
- Worker handoff JSON is generated before the worker starts and records the exact slice contract, worktree path, branch, run id, and output path the worker must use.
- `inspect` and blocked/failed artifacts expose bounded diagnostics without requiring maintainers to scrape daemon internals.
- `khazad-doom handoff` prints branch, summary, and suggested push/PR commands by default. It must not mutate remotes unless `--push`, `--create-pr`, or explicit repository configuration requests that behavior; `--dry-run` suppresses configured actions.

## Release and tag safety

- Release creation is an explicit maintainer action, currently by pushing a `v*` tag for CI. Daemon runs and default handoffs do not create or push release tags.
- Release/package refactors must preserve the workflow gate: validated slices, committed worker changes, serial integration, passing verification, and explicit handoff before any remote or tag mutation.
