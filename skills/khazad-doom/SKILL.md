---
name: khazad-doom
description: "Drive the Khazad-Doom workflow daemon: initialize repo workflow contracts, run JSON Issue Slices through isolated Pi workers, use fake only as a deterministic test seam, inspect status/artifacts, create handoffs, and report blockers."
argument-hint: "[init|slices|run|resume|status|monitor|watch|attend|cockpit|cancel|handoff|inspect] [options]"
---

# Khazad-Doom

Khazad-Doom is a production daemon-oriented agentic workflow framework.

Core rule:

> You shall not slop.

## Use

Use the `khazad-doom` CLI. Do not reimplement daemon behavior manually in chat.

Common commands:

```sh
khazad-doom init
khazad-doom slices validate
khazad-doom slices list
khazad-doom slices schema --write
khazad-doom run --slice <slice-id>
khazad-doom run --all --parallel <n>
khazad-doom run --allow-dirty --slice <slice-id>
khazad-doom run --cockpit direct --all
khazad-doom run --origin-notification-target <target> --slice <slice-id>
khazad-doom run --envelope <mission.json> --autonomy off --slice <slice-id>
khazad-doom run --agent fake --all
khazad-doom resume --run <run-id>
khazad-doom status --run <run-id>
khazad-doom status --run <run-id> --follow
khazad-doom monitor --repo . --latest
khazad-doom monitor --run <run-id>
khazad-doom watch --run <run-id>
khazad-doom attend --run <run-id>
khazad-doom cockpit open --run <run-id>
khazad-doom cockpit open --latest --repo .
khazad-doom handoff --run <run-id>
khazad-doom handoff --run <run-id> --dry-run
scripts/roadmap-truth-check
khazad-doom inspect --run <run-id>
khazad-doom inspect --repo . --latest
khazad-doom cancel --run <run-id>
khazad-doom replan list <run-id>
khazad-doom replan propose <run-id> --change kind:target:summary
khazad-doom replan accept <run-id> <proposal-id> --reason "..."
khazad-doom replan reject <run-id> <proposal-id> --reason "..."
khazad-doom replan defer <run-id> <proposal-id> --until "condition" --reason "..."
khazad-doom daemon status
```

## Protocol

- JSON Issue Slices in `.workflow/slices/*.json` are the machine source of truth.
- Slices have an issue-style lifecycle: new slices are open by default; successful daemon runs close completed slice JSON with `status: "closed"`, `closed_by_run`, and `closed_at`.
- Do not rerun closed historical slices. Closed dependencies are treated as satisfied; explicitly requesting a closed slice should be rejected in favor of creating a follow-up slice.
- `docs/workflow-invariants.md` records daemon workflow invariants that behavior-preserving refactors must keep stable.
- `.workflow/khazad.json` carries repo defaults, durable `cockpit` policy (`auto`, `herdr`, or `direct`), daemon-owned `worktree_setup` bootstrap commands, `worker_question_timeout_seconds` (`0` = wait indefinitely), and verification profiles.
- GitHub issues/PRDs carry rich human context, but the JSON slice wins on conflict.
- Treat each open slice as bounded intent plus minimum evidence, not a frozen mini-spec: learning is allowed inside the fence; moving the fence requires approval. TDD-discovered cases directly implied by the slice goal or acceptance may be handled inline and reported; discoveries that alter intent or exceed declared `areas` require `ask-user` or a follow-up slice.
- When authoring slices, include expected test/helper/doc paths in `areas`; narrow `areas` are intentional hard stops, not semantic hints.
- Worker output is JSON-only. Invalid/missing/schema-invalid JSON is durable evidence and may use bounded envelope re-emission for the existing worker head without burning implementation attempts.
- Worker `acceptance_status` is an evidence claim, not approval. Workers must not approve their own evidence; daemon checks/gates and later human review attest or reject it separately.
- Worker commits are required before merge.
- Runs are clean-by-default: starting from a dirty source repo requires explicit `--allow-dirty`, and the daemon writes a preflight snapshot with base branch/SHA and dirty status.
- Optional `--envelope <mission.json>` records a durable mission envelope plus zeroed frontier budget at run start. Envelope `allowed_areas` use the same literal-prefix area contract as slice `areas`, are persisted across restart/resume, and appear in status/watch/monitor/report/handoff. In AF-04 `off` performs no frontier classification; `shadow` classifies pending typed `add_followup_slice` replan proposals at existing checkpoints and records tier/reason codes/envelope hash/budget snapshot/classified-at plus `frontier_classified` events without decisions or queue/slice mutation. `promote`/`run` are still recorded-not-active before AF-06 and behave as shadow observation only; they do not auto-propose, auto-apply, auto-generate slices, or grant authority.
- Verification/tooling failures such as missing commands, invalid verify cwd, shell spawn failures, and non-executable commands are daemon/operator environment failures, not worker auto-fix requests. Operator environment gate failures block instead of spending an integration-repair worker.
- Declared slice `areas` are path guardrails: worker changes outside those areas block the slice as scope violations; do not add semantic scope-policing machinery. Areas are repo-relative literal path prefixes, not globs; use directory prefixes like `src/normia/` or exact files like `README.md`.
- Mechanical daemon-owned slice verify failures may get at most one targeted in-scope slice-repair attempt after normal attempts would otherwise fail; scope violations are never auto-repaired or auto-authorized.
- Multiple open slices run in dependency order; independent open slices can run in parallel, then merge serially. Ready siblings from a failed parallel layer stay preserved-but-unmerged evidence.
- Pi is the sole real worker harness. `--agent fake` is deterministic and only for local tests/dogfooding; do not present it as portability or a second production harness. Fake-runner artifacts/status/reports are labelled as deterministic test-double evidence, not real Pi worker implementation evidence.
- Interrupted daemon runs are marked `interrupted` on next startup; lost workers are not silently resumed. Pending worker questions from the lost attempt become stale/interrupted evidence; resume the run and answer the fresh pending question for the active attempt.
- Merge conflicts are structured blocked artifacts; do not paper over them.
- Runtime artifacts under `.workflow/runs/` are gitignored and include preflight snapshots, optional `origin.json` terminal-feedback targets, notification dedupe records, observed Pi contract/profile summaries, raw outputs, terminal run summaries, and bounded failed/cancelled attempt diagnostics.
- Handoff prints by default; push/PR creation require explicit flags or config, and `--dry-run` suppresses configured actions.
- Final reports and handoff JSON expose explicit `exit_states`, `evidence_attestation`, and `plan_revisions`; treat them as read-only summaries over existing lifecycle state, not extra gates. Pending replan proposals block handoff readiness until an operator records an accepted/rejected/deferred/superseded disposition.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, live progress snapshots, status projection, Herdr cockpit launch/fallback decisions, monitor output, handoff JSON, and artifact inspection.
- Runs are daemon-owned durable sessions. A Pi tool call must start/control/observe a run, never define its lifetime.

## Pi chat UX rule: non-blocking run-start handoff

After `khazad-doom run ...` successfully returns a `run_id`, the assistant MUST acknowledge the daemon-owned background run without turning the chat into a polling loop.

Required run-start response shape:

1. One short sentence with the `run_id` and whether the run is backgrounded.
2. One primary next action, preferably the emitted `run_monitor_command` / `monitor_command`.
3. Optional compact controls only when useful: `attend`, `inspect`, `handoff`, `resume`, or `cancel`.

Do NOT use the old verbose boilerplate: no phase-title announcement, no “Per protocol” framing, no mention of polling cessation, and no invented future milestone work such as generating later issue batches unless the user explicitly asked for it.

Preferred response after run start:

```text
Started KD run `<run-id>` in the background.
Monitor: `khazad-doom monitor --run <run-id>`
Need action later: ask me to inspect, attend, resume, cancel, or handoff.
```

The assistant MUST NOT run `sleep`, repeated `status`, `watch`, `monitor`, or polling loops after a successful run start.

Allowed exceptions:
- The user explicitly asks to check status, monitor, watch, or stay with the run.
- The run command failed or returned an immediate blocker.
- The user asks for handoff/inspect/resume/cancel/attend.
- A single non-looping `status --run` is needed to diagnose ambiguous daemon startup output.

- Do not use blocking `--wait` as the primary Pi UX for real `pi` runs. Start the run without `--wait`, capture the JSON (`run_id`, `repo_path`, `monitor_command`, `run_monitor_command`), report the monitor command, and return control to the user unless an allowed exception above applies.
- Use `khazad-doom watch --run <run-id>`, `khazad-doom monitor --run <run-id>`, or short `status --run` checks only as explicit user-requested actions or plain fallbacks when the monitor dashboard is not suitable and an allowed exception above applies.
- Cockpit mode defaults to `auto`: when `herdr` is usable, Khazad-Doom may create/focus a `Khazad-Doom <run-id>` workspace with a Dashboard pane, a left worker region, and deterministic native Pi TUI worker agents named from run id, slice id, and attempt. Worker `ask_operator` questions should prompt in that same worker Pi pane and record answers through daemon `answerQuestion`; replans still go through daemon commands (`attend`, `replan`). Native worker truth is `submit_worker_result` / `kd_tui_result_artifact`; `--json-wrapper-worker`, `KHAZAD_JSON_WRAPPER_WORKER=1`, `KHAZAD_DISABLE_PI_TUI_WORKER=1`, `--cockpit direct`, or config `"cockpit": "direct"` uses/suppresses the legacy wrapper/cockpit path.
- `--origin-notification-target <target>` or `KHAZAD_ORIGIN_NOTIFICATION_TARGET` records an opaque `.workflow/runs/<run>/origin.json` target. After the daemon terminal summary exists, completed/blocked/failed/cancelled transitions send inert JSON evidence back through the Cockpit Herdr `agent send` seam with durable per-transition dedupe; interrupted is excluded. Pending worker questions and replan decisions also send/focus attention messages when an origin target exists, with durable question/proposal-keyed dedupe and declarative payloads containing only reason text plus exact `answer`/`replan` and `status`/`monitor` commands. Runs without an origin target do not notify. Missing Herdr, malformed/stale recorded targets, or send/focus failures are non-fatal visibility evidence and do not change status, verification, merge, handoff readiness, or final SHA.
- Herdr cockpit failures, activity-painter exits, terminal-feedback failures, and pre-launch native-TUI/wrapper handoff failures are non-fatal fallback/visibility incidents or pane warnings; they must not change slice selection, worker authorization, verification, merge, handoff, or terminal run status by themselves. KD reads only daemon-owned result artifacts (`kd_tui_result_artifact` for native TUI, wrapper stdout/stderr/exit/result artifacts for legacy wrapper), never pane text, scrollback, or Herdr agent-status metadata. Painters are semantic display only and never contribute correctness evidence. Do not approve or paste worker result JSON into a worker pane; bounded operator answers are acceptable only through the worker pane's Pi `ask_operator` dialog or explicit daemon commands such as `answer`/`cancel`.
- The Planner Pi pane is deferred until RPL planner authority exists; do not launch a planner agent for cockpit setup.
- `khazad-doom monitor` is attach-only: Ctrl-C exits the terminal dashboard, but must not stop or suspend the daemon-owned run.
- `khazad-doom monitor`, `watch`, `attend`, Herdr dashboard/status panes, and the optional Pi `/khazad-attach <run-id>` / `/khazad-explain <run-id|--latest>` bridge paint the daemon `feed` projection from `status` JSON. Renderers may choose layout/color but should not invent workflow wording; terminal reasons, attention, and operator commands come from `primary_terminal_reason`, `feed.terminal_reason`, `feed.attention`, and `feed.operator_commands`. Attention lines must remain full/untruncated.
- `khazad-doom cockpit open --run <run-id>` and `khazad-doom cockpit open --latest --repo .` explicitly open/focus Herdr for an existing daemon run. If Herdr is unavailable, the command returns JSON with fallback/remediation and the `status`/`watch`/`monitor` operator commands instead of making Herdr required.
- Do not require Herdr or a Pi UI extension for core monitoring; keep `khazad-doom monitor --repo . --latest` as the terminal path over daemon state and `watch`/`status` as fallbacks. If the operator wants in-Pi feedback, suggest explicit `/khazad-attach <run-id>`, `/khazad-explain <run-id|--latest>`, `/khazad-open <run-id|--latest>`, and `/khazad-detach`.
- Verification/gate timeouts are per-command hang protection, not global workflow timeouts.
- Worker attempt supervision separates daemon/process liveness from worker output activity. In `status`, `watch`, or `monitor`, treat `Supervisor: alive` as "Khazad-Doom still observes the child process," not proof of semantic progress.
- Missing worker output is advisory by default. If monitor says `Warning: worker is quiet`, explain that it may be normal and offer wait, inspect, or `khazad-doom cancel --run <id> --reason ...`; do not claim the worker is hung unless an explicit timeout/policy made it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker attempt timeout is an explicit repo/operator policy, separate from run lifetime.
- `worker_question_timeout_seconds: 0` means pending operator questions wait indefinitely. The built-in and this repository use 60 seconds; status/feed/monitor/attend and the same-pane dialog show the daemon-owned absolute deadline and any eligible recommended fallback.
- A worker calling `ask_operator` must include its original recommendation and rationale. Timeout fallback is eligible only when the recommendation exactly matches one non-empty declared option and the worker truthfully attests both bounded-within-current-slice-or-mission-authority and reversibility. Never attest eligibility for scope expansion, destructive/irreversible actions, credentials/secrets, permissions, release/push/handoff authorization, or work outside the current slice/mission envelope; those cases must block without an operator answer.
- The daemon atomically resolves operator-answer versus recommendation-deadline races. `answer_source=operator` and `answer_source=llm_recommendation_timeout` are durable audit evidence; worker panes and renderers must return/paint the durable winner rather than inventing a local outcome.

If status/monitor/attend shows an `Attention` line or pending worker question, prefer answering the visible Pi `ask_operator` dialog in the worker pane when it is available. If no worker-pane prompt is available, ask the user for the answer and then run `khazad-doom answer <run-id> <question-id> "..."` or use `khazad-doom attend --run <run-id>` (or `/khazad-answer <run-id> <question-id> "..."` in Pi) after normal command confirmation. The Pi monitor bridge attached with `/khazad-attach <run-id>` is read-only for worker questions. Do not answer by arbitrary terminal typing into Herdr worker panes. If the daemon says the run is interrupted or the question is not attached to the active worker attempt, resume first and answer the fresh question shown by status/monitor/attend. If a run blocks with an `ask-user` finding after timeout/unavailable/ineligible ask_operator fallback, relay the blocker with exact details and ask for a decision before resuming.

If status/monitor/attend shows a pending replan decision, use the exact `khazad-doom replan accept|reject|defer` command shown by the daemon feed or `khazad-doom attend --run <run-id>`. Accepted replan records may grant `authorized_paths`/`action_class`; daemon path guards and worker prompts may honor those grants for the source slice, but other queue/slice/verification/policy mutations remain explicit and auditable. Do not treat roadmap Markdown as the source of truth; use `scripts/roadmap-truth-check` to compare roadmap completion claims against slice JSON and daemon report evidence.
