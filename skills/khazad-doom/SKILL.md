---
name: khazad-doom
description: Drive the Khazad-Doom workflow daemon: initialize repo workflow contracts, run JSON Issue Slices through isolated Pi/fake workers, inspect status/artifacts, create handoffs, and report blockers.
argument-hint: "[init|slices|run|status|cancel|handoff|inspect] [options]"
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
khazad-doom run --slice <slice-id> --wait
khazad-doom run --all --wait
khazad-doom run --agent fake --all --wait
khazad-doom status --run <run-id>
khazad-doom handoff --run <run-id>
khazad-doom inspect --run <run-id>
khazad-doom cancel --run <run-id>
khazad-doom daemon status
```

## Protocol

- JSON Issue Slices in `.workflow/slices/*.json` are the machine source of truth.
- GitHub issues/PRDs carry rich human context, but the JSON slice wins on conflict.
- Worker output is JSON-only.
- Worker commits are required before merge.
- Multiple slices run serially in dependency order; requested slice dependencies are included automatically.
- `--agent fake` is deterministic and only for local tests/dogfooding.
- Interrupted daemon runs are marked `interrupted` on next startup; lost workers are not silently resumed.
- Runtime artifacts under `.workflow/runs/` are gitignored.
- The daemon owns worker prompts, state, worktrees, scheduling, repair, integration gates, cleanup, status, handoff JSON, and artifact inspection.

If a run blocks with an `ask-user` finding, relay the blocker to the user with exact details and ask for a decision before resuming.
