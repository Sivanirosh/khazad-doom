---
name: khazad-doom
description: "Drive the Khazad-Doom workflow daemon: initialize repo workflow contracts, run JSON Issue Slices through isolated Pi workers, use fake only as a deterministic test seam, inspect status/artifacts, create handoffs, and report blockers."
argument-hint: "[init|slices|run|resume|status|monitor|watch|cockpit|cancel|handoff|inspect] [options]"
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
khazad-doom run --agent fake --all
khazad-doom resume --run <run-id>
khazad-doom status --run <run-id>
khazad-doom status --run <run-id> --follow
khazad-doom monitor --repo . --latest
khazad-doom monitor --run <run-id>
khazad-doom watch --run <run-id>
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
- `.workflow/khazad.json` carries repo defaults, durable `cockpit` policy (`auto`, `herdr`, or `direct`), daemon-owned `worktree_setup` bootstrap commands, and verification profiles.
- GitHub issues/PRDs carry rich human context, but the JSON slice wins on conflict.
- Treat each open slice as bounded intent plus minimum evidence, not a frozen mini-spec: learning is allowed inside the fence; moving the fence requires approval. TDD-discovered cases directly implied by the slice goal or acceptance may be handled inline and reported; discoveries that alter intent or exceed declared `areas` require `ask-user` or a follow-up slice.
- When authoring slices, include expected test/helper/doc paths in `areas`; narrow `areas` are intentional hard stops, not semantic hints.
- Worker output is JSON-only.
- Worker `acceptance_status` is an evidence claim, not approval. Workers must not approve their own evidence; daemon checks/gates and later human review attest or reject it separately.
- Worker commits are required before merge.
- Runs are clean-by-default: starting from a dirty source repo requires explicit `--allow-dirty`, and the daemon writes a preflight snapshot with base branch/SHA and dirty status.
- Verification/tooling failures such as missing commands, invalid verify cwd, shell spawn failures, and non-executable commands are daemon/operator environment failures, not worker auto-fix requests. Operator environment gate failures block instead of spending an integration-repair worker.
- Declared slice `areas` are path guardrails: worker changes outside those areas block the slice as scope violations; do not add semantic scope-policing machinery.
- Multiple open slices run in dependency order; independent open slices can run in parallel, then merge serially.
- Pi is the sole real worker harness. `--agent fake` is deterministic and only for local tests/dogfooding; do not present it as portability or a second production harness. Fake-runner artifacts/status/reports are labelled as deterministic test-double evidence, not real Pi worker implementation evidence.
- Interrupted daemon runs are marked `interrupted` on next startup; lost workers are not silently resumed. Pending worker questions from the lost attempt become stale/interrupted evidence; resume the run and answer the fresh pending question for the active attempt.
- Merge conflicts are structured blocked artifacts; do not paper over them.
- Runtime artifacts under `.workflow/runs/` are gitignored and include preflight snapshots, observed Pi contract/profile summaries, raw outputs, terminal run summaries, and bounded failed/cancelled attempt diagnostics.
- Handoff prints by default; push/PR creation require explicit flags or config, and `--dry-run` suppresses configured actions.
- Final reports and handoff JSON expose explicit `exit_states`, `evidence_attestation`, and `plan_revisions`; treat them as read-only summaries over existing lifecycle state, not extra gates. Pending replan proposals block handoff readiness until an operator records an accepted/rejected/deferred/superseded disposition.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, live progress snapshots, status projection, Herdr cockpit launch/fallback decisions, monitor output, handoff JSON, and artifact inspection.
- Runs are daemon-owned durable sessions. A Pi tool call must start/control/observe a run, never define its lifetime.

## Pi chat UX rule: detach after run start

After `khazad-doom run ...` successfully returns a `run_id`, the assistant MUST:

1. Report the `run_id`.
2. Report the emitted `monitor_command` / `run_monitor_command`.
3. Stop.

The assistant MUST NOT run `sleep`, repeated `status`, `watch`, or polling loops after a successful run start.

Allowed exceptions:
- The user explicitly asks to check status.
- The run command failed or returned an immediate blocker.
- The user asks for handoff/inspect/resume/cancel.
- A single non-looping `status --run` is needed to diagnose ambiguous daemon startup output.

Preferred response after run start:

“Khazad-Doom is running in the background. Monitor with:
`khazad-doom monitor --run <run-id>`
I’ll stop polling unless you ask me to inspect or resume it.”

- Do not use blocking `--wait` as the primary Pi UX for real `pi` runs. Start the run without `--wait`, capture the JSON (`run_id`, `repo_path`, `monitor_command`, `run_monitor_command`), report the monitor command, and detach unless an allowed exception above applies.
- Use `khazad-doom watch --run <run-id>` or short `status --run` checks only as plain fallbacks when the monitor dashboard is not suitable and an allowed exception above applies.
- Cockpit mode defaults to `auto`: when `herdr` is usable, Khazad-Doom may create/focus a `Khazad-Doom <run-id>` workspace with read-only `Run Status / Event Feed` and `Integration Gate / Repair` panes plus deterministic worker panes named from run id, slice id, and attempt; the gate/repair pane paints active daemon-owned gate/repair command activity from status feed and bounded shell progress, falling back to the feed/status summary when idle; worker panes show a read-only activity painter over daemon-owned wrapper stdout artifacts; `--cockpit direct` or config `"cockpit": "direct"` suppresses this.
- Herdr cockpit failures, activity-painter exits, and pre-launch worker pane/wrapper handoff failures are non-fatal fallback/visibility incidents or pane warnings; they must not change slice selection, worker authorization, verification, merge, handoff, or terminal run status by themselves. Worker panes run a Khazad-owned wrapper and KD reads only wrapper stdout/stderr/exit/result artifacts through the Pi contract parser, never pane text, scrollback, or Herdr agent-status metadata. The gate/repair painter reads only daemon status feed/shell progress data and never contributes correctness evidence. Do not answer, approve, or paste worker result JSON into a worker pane; use daemon commands such as `answer` or `cancel`.
- The Planner Pi pane is deferred until RPL planner authority exists; do not launch a planner agent for cockpit setup.
- `khazad-doom monitor` is attach-only: Ctrl-C exits the terminal dashboard, but must not stop or suspend the daemon-owned run.
- `khazad-doom monitor`, `watch`, Herdr feed/status panes, and the optional Pi `/khazad-attach <run-id>` / `/khazad-explain <run-id|--latest>` bridge paint the daemon `feed` projection from `status` JSON. The Herdr gate/repair pane may additionally display bounded `status` progress tails for the active gate or repair command. Renderers may choose layout/color but should not invent workflow wording; terminal reasons and operator commands come from `primary_terminal_reason`, `feed.terminal_reason`, and `feed.operator_commands`.
- `khazad-doom cockpit open --run <run-id>` and `khazad-doom cockpit open --latest --repo .` explicitly open/focus Herdr for an existing daemon run. If Herdr is unavailable, the command returns JSON with fallback/remediation and the `status`/`watch`/`monitor` operator commands instead of making Herdr required.
- Do not require Herdr or a Pi UI extension for core monitoring; keep `khazad-doom monitor --repo . --latest` as the terminal path over daemon state and `watch`/`status` as fallbacks. If the operator wants in-Pi feedback, suggest explicit `/khazad-attach <run-id>`, `/khazad-explain <run-id|--latest>`, `/khazad-open <run-id|--latest>`, and `/khazad-detach`.
- Verification/gate timeouts are per-command hang protection, not global workflow timeouts.
- Worker attempt supervision separates daemon/process liveness from worker output activity. In `status`, `watch`, or `monitor`, treat `Supervisor: alive` as "Khazad-Doom still observes the child process," not proof of semantic progress.
- Missing worker output is advisory by default. If monitor says `Warning: worker is quiet`, explain that it may be normal and offer wait, inspect, or `khazad-doom cancel --run <id> --reason ...`; do not claim the worker is hung unless an explicit timeout/policy made it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker attempt timeout is an explicit repo/operator policy, separate from run lifetime.

If status/monitor shows an `Attention` line or pending worker question, ask the user for the answer and then run `khazad-doom answer <run-id> <question-id> "..."` (or `/khazad-answer <run-id> <question-id> "..."` in Pi) after normal command confirmation. Do not answer by typing into Herdr worker panes. If the daemon says the run is interrupted or the question is not attached to the active worker attempt, resume first and answer the fresh question shown by status/monitor. If a run blocks with an `ask-user` finding after timeout/unavailable ask_operator fallback, relay the blocker with exact details and ask for a decision before resuming.

If status/monitor shows `Awaiting replan decision`, use the exact `khazad-doom replan accept|reject|defer` command shown by the daemon feed. Replan v1 never auto-applies queue/slice/verification/policy mutations; accepted decisions record `applied=false` until a later authorized slice adds application semantics. Do not treat roadmap Markdown as the source of truth; use `scripts/roadmap-truth-check` to compare roadmap completion claims against slice JSON and daemon report evidence.
