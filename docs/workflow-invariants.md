# Workflow invariants

These invariants define the daemon-owned workflow behavior that v0.1.0 release-polish refactors must preserve. They are intentionally phrased as testable contracts; changing one is a public workflow behavior change, not a cleanup.

## Run lifetime and ownership

- A run is a durable daemon-owned session keyed by `run_id`. CLI commands, Pi tool calls, and optional Pi UI adapters start, control, or observe that session; they do not define its lifetime.
- No hidden global workflow timeout exists by default. A run continues until it reaches a terminal state, is cancelled, or is marked interrupted/recovered by daemon startup logic.
- Time limits are explicit policy knobs for specific work: verification/gate command timeouts and, when configured, per-worker-attempt timeouts.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, progress snapshots, status/monitor output, handoff JSON, and artifact inspection.

## Slice lifecycle and integration

- JSON Issue Slices are the authoritative work contract. Dependencies run before dependents, cycles are invalid, and requested slices include required dependencies.
- Each worker attempt runs one slice in a daemon-managed isolated worktree. Parallel workers must not share a checkout.
- A completed worker must return valid JSON, commit intended changes, and leave its worktree clean before the daemon may integrate the slice.
- Independent slices may execute concurrently, but integration into the run branch is serial.
- After each successful integration merge, Khazad-Doom records a checkpoint before advancing, so `resume` can continue from recorded state instead of replaying completed merges.
- Merge conflicts, `ask-user` findings, invalid worker output, dirty worktrees, and verification failures become structured blocked/failed artifacts rather than silent best-effort integration.
- If integrated work needs repair, repair occurs before the integration gate is treated as passed; repair does not bypass or weaken the gate.

## Worker attempt supervision

- Attempt history is append-only evidence. Retries add attempts and preserve previous output/failure context.
- Worker execution is at-least-once, not exactly-once. A timed-out, cancelled, or retried attempt may have produced files or commits in its isolated worktree.
- Process liveness and output activity are distinct. `Supervisor: alive` means the daemon still observes the child process, not that semantic progress is guaranteed.
- Quiet-worker warnings are advisory. Missing output alone is not terminal unless an explicit timeout/policy makes it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker-attempt timeout is an explicit repo/operator policy and applies to an attempt, not to the whole run lifetime.

## Cancellation, interruption, and resume

- `cancel` requests cancellation through daemon state and worker process signalling; it is an explicit operator action, not a side effect of closing a monitor, status follower, Pi overlay, or CLI session.
- If the daemon starts and discovers active runs from a previous process, it marks them `interrupted`, records recovery events, and cleans daemon-managed worktrees where possible.
- `resume` is explicit. It uses durable checkpoint/run state for remaining work and never claims to resurrect a lost worker process.

## Verification and gates

- Slice `verify` commands and configured verification profile commands are gates for completion/integration; they must run with their declared repo-relative context and environment.
- Verification and gate timeouts are per-command hang protection. They are not global workflow timeouts and must not be reused to cap total run lifetime.
- Gate failures are reported with command evidence and must be repaired or surfaced as blocked/failed before handoff.

## Progress, status, and monitor state

- The daemon/state store is the source of truth for run status, slice states, events, and live progress snapshots.
- `status`, `watch`, `monitor`, and optional Pi adapters render the same daemon state. They must not own workflow state or infer cancellation from UI/session shutdown.
- Progress output may distinguish supervisor liveness, worker process state, last output event, last semantic progress, configured timeouts, and advisory quiet-worker warnings.

## Artifacts, handoffs, and remotes

- Runtime handoffs, raw worker outputs, checkpoints, and inspection artifacts live under `.workflow/runs/` and remain transient/gitignored unless explicitly promoted elsewhere.
- Worker handoff JSON is generated before the worker starts and records the exact slice contract, worktree path, branch, run id, and output path the worker must use.
- `inspect` and blocked/failed artifacts expose bounded diagnostics without requiring maintainers to scrape daemon internals.
- `khazad-doom handoff` prints branch, summary, and suggested push/PR commands by default. It must not mutate remotes unless `--push`, `--create-pr`, or explicit repository configuration requests that behavior; `--dry-run` suppresses configured actions.

## Release and tag safety

- Release creation is an explicit maintainer action, currently by pushing a `v*` tag for CI. Daemon runs and default handoffs do not create or push release tags.
- Release/package refactors must preserve the workflow gate: validated slices, committed worker changes, serial integration, passing verification, and explicit handoff before any remote or tag mutation.
