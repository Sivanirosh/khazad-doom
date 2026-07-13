---
name: khazad-doom
description: "Drive Khazad-Doom. Use for repository initialization or JSON Issue Slices; starting or resuming runs; status, monitoring, attention, cancellation, inspection, cockpit focus, daemon health, or handoff. Fake workers are deterministic test seams."
---

# Khazad-Doom

Khazad-Doom turns JSON Issue Slices into bounded, daemon-owned runs. Cross the workflow seam through the `khazad-doom` CLI; daemon state and artifacts are authoritative, while Pi and Herdr are execution and observability adapters.

## Route before acting

Load the reference that matches each requested branch:

- **Initialize, author, validate, list, or select slices:** read [`SLICES.md`](SLICES.md) before changing `.workflow/` or choosing work.
- **Start or resume a run:** read [`RUNS.md`](RUNS.md) before constructing the command. It owns cockpit selection, mission envelopes, and the non-blocking run-start handoff.
- **Observe or operate the daemon or an existing run:** read [`OPERATIONS.md`](OPERATIONS.md) before daemon health checks, status, monitoring, attention, answers, replans, cockpit focus, cancellation, inspection, or handoff.

For requests spanning branches, load every applicable reference before the first mutation. Use `khazad-doom <command> --help` as the syntax source of truth when a required option is not shown in the loaded reference.

**Routing is complete when:** every requested action maps to a branch, every applicable reference has been read, and the next action is an exact CLI command or an explicit operator decision.

## Invariants carried across every branch

- A run is a durable daemon session. CLI calls start, observe, or control it; the caller process does not own its lifetime.
- Slice JSON bounds intent, paths, dependencies, acceptance evidence, verification, and stop conditions. Learning stays inside that fence; moving it requires a durable operator decision or follow-up slice.
- Pi is the only real worker harness. `fake` is a deterministic test seam, not a production alternative.
- Workers claim evidence; daemon checks, gates, and later human review attest it. Herdr panes, terminal text, and worker claims never approve work.
- Read status through the daemon `feed` projection and use its exact operator commands. Visibility failures remain separate from workflow correctness.
