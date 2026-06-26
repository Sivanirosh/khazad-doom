---
name: khazad-doom
description: "Drive the Khazad-Doom workflow daemon: initialize repo workflow contracts, run JSON Issue Slices through isolated Pi/fake workers, inspect status/artifacts, create handoffs, and report blockers."
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
khazad-doom cancel --run <run-id>
khazad-doom daemon status
```

## Protocol

- JSON Issue Slices in `.workflow/slices/*.json` are the machine source of truth.
- `.workflow/khazad.json` carries repo defaults and verification profiles.
- GitHub issues/PRDs carry rich human context, but the JSON slice wins on conflict.
- Worker output is JSON-only.
- Worker commits are required before merge.
- Multiple slices run in dependency order; independent slices can run in parallel, then merge serially.
- `--agent fake` is deterministic and only for local tests/dogfooding.
- Interrupted daemon runs are marked `interrupted` on next startup; lost workers are not silently resumed.
- Merge conflicts are structured blocked artifacts; do not paper over them.
- Runtime artifacts under `.workflow/runs/` are gitignored.
- Handoff prints by default; push/PR creation require explicit flags or config, and `--dry-run` suppresses configured actions.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, live progress snapshots, status, monitor output, handoff JSON, and artifact inspection.
- Runs are daemon-owned durable sessions. A Pi tool call must start/control/observe a run, never define its lifetime.
- Do not use blocking `--wait` as the primary Pi UX for real `pi` runs. Start the run without `--wait`, capture the JSON (`run_id`, `repo_path`, `monitor_command`, `run_monitor_command`), and recommend or use `khazad-doom monitor --repo . --latest` / the emitted `monitor_command` for user-visible progress.
- Use `khazad-doom watch --run <run-id>` or short `status --run` checks only as plain fallbacks when the monitor dashboard is not suitable.
- Khazad-Doom does not auto-open external windows by default; a Pi extension is an optional adapter over daemon state, not core workflow state.
- Verification/gate timeouts are per-command hang protection, not global workflow timeouts.

If a run blocks with an `ask-user` finding, relay the blocker to the user with exact details and ask for a decision before resuming.
