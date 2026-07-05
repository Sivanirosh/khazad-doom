---
name: khazad-doom
description: "Drive the Khazad-Doom workflow daemon: initialize repo workflow contracts, run JSON Issue Slices through isolated Pi workers, use fake only as a deterministic test seam, inspect status/artifacts, create handoffs, and report blockers."
argument-hint: "[init|slices|run|resume|status|monitor|watch|cancel|handoff|inspect] [options]"
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
khazad-doom run --agent fake --all
khazad-doom resume --run <run-id>
khazad-doom status --run <run-id>
khazad-doom status --run <run-id> --follow
khazad-doom monitor --repo . --latest
khazad-doom monitor --run <run-id>
khazad-doom watch --run <run-id>
khazad-doom handoff --run <run-id>
khazad-doom handoff --run <run-id> --dry-run
khazad-doom inspect --run <run-id>
khazad-doom inspect --repo . --latest
khazad-doom cancel --run <run-id>
khazad-doom daemon status
```

## Protocol

- JSON Issue Slices in `.workflow/slices/*.json` are the machine source of truth.
- Slices have an issue-style lifecycle: new slices are open by default; successful daemon runs close completed slice JSON with `status: "closed"`, `closed_by_run`, and `closed_at`.
- Do not rerun closed historical slices. Closed dependencies are treated as satisfied; explicitly requesting a closed slice should be rejected in favor of creating a follow-up slice.
- `docs/workflow-invariants.md` records daemon workflow invariants that behavior-preserving refactors must keep stable.
- `.workflow/khazad.json` carries repo defaults and verification profiles.
- GitHub issues/PRDs carry rich human context, but the JSON slice wins on conflict.
- Treat each open slice as bounded intent plus minimum evidence, not a frozen mini-spec: learning is allowed inside the fence; moving the fence requires approval. TDD-discovered cases directly implied by the slice goal or acceptance may be handled inline and reported; discoveries that alter intent or exceed declared `areas` require `ask-user` or a follow-up slice.
- When authoring slices, include expected test/helper/doc paths in `areas`; narrow `areas` are intentional hard stops, not semantic hints.
- Worker output is JSON-only.
- Worker `acceptance_status` is an evidence claim, not approval. Workers must not approve their own evidence; daemon checks/gates and later human review attest or reject it separately.
- Worker commits are required before merge.
- Runs are clean-by-default: starting from a dirty source repo requires explicit `--allow-dirty`, and the daemon writes a preflight snapshot with base branch/SHA and dirty status.
- Verification/tooling failures such as missing commands, invalid verify cwd, shell spawn failures, and non-executable commands are daemon/operator environment failures, not worker auto-fix requests.
- Declared slice `areas` are path guardrails: worker changes outside those areas block the slice as scope violations; do not add semantic scope-policing machinery.
- Multiple open slices run in dependency order; independent open slices can run in parallel, then merge serially.
- Pi is the sole real worker harness. `--agent fake` is deterministic and only for local tests/dogfooding; do not present it as portability or a second production harness.
- Interrupted daemon runs are marked `interrupted` on next startup; lost workers are not silently resumed.
- Merge conflicts are structured blocked artifacts; do not paper over them.
- Runtime artifacts under `.workflow/runs/` are gitignored and include preflight snapshots, observed Pi contract/profile summaries, raw outputs, terminal run summaries, and bounded failed/cancelled attempt diagnostics.
- Handoff prints by default; push/PR creation require explicit flags or config, and `--dry-run` suppresses configured actions.
- Final reports and handoff JSON expose explicit `exit_states` and `evidence_attestation`; treat them as read-only summaries over existing lifecycle state, not extra gates.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, live progress snapshots, status projection, monitor output, handoff JSON, and artifact inspection.
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
- Khazad-Doom does not auto-open external windows by default; core observability is the daemon-owned CLI surface.
- `khazad-doom monitor` is attach-only: Ctrl-C exits the terminal dashboard, but must not stop or suspend the daemon-owned run.
- `khazad-doom monitor`, `watch`, and the optional Pi `/khazad-attach <run-id>` widget paint the daemon `feed` projection from `status` JSON. Renderers may choose layout/color but should not invent workflow wording.
- Do not require a Pi UI extension for core monitoring; keep `khazad-doom monitor --repo . --latest` as the terminal path over daemon state and `watch`/`status` as fallbacks. If the operator wants in-Pi feedback, suggest explicit `/khazad-attach <run-id>` and `/khazad-detach`.
- Verification/gate timeouts are per-command hang protection, not global workflow timeouts.
- Worker attempt supervision separates daemon/process liveness from worker output activity. In `status`, `watch`, or `monitor`, treat `Supervisor: alive` as "Khazad-Doom still observes the child process," not proof of semantic progress.
- Missing worker output is advisory by default. If monitor says `Warning: worker is quiet`, explain that it may be normal and offer wait, inspect, or `khazad-doom cancel --run <id> --reason ...`; do not claim the worker is hung unless an explicit timeout/policy made it terminal.
- `worker_attempt_timeout_seconds: 0` means no fatal worker-attempt timeout. Any nonzero worker attempt timeout is an explicit repo/operator policy, separate from run lifetime.

If status/monitor shows an `Attention` line or pending worker question, ask the user for the answer and then run `khazad-doom answer <run-id> <question-id> "..."` after normal command confirmation. If a run still blocks with an `ask-user` finding, relay the blocker with exact details and ask for a decision before resuming.
