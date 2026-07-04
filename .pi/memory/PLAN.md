# Plan

## Near-term product work

- Preserve the core contract: JSON Issue Slices, isolated worktrees, JSON-only worker outputs, per-slice commits, lightweight checks before merge, integration gate before handoff, durable checkpoints, and explicit handoff commands.
- Make the Pi-native migration explicit through PI-00..PI-06: doctrine first, truthful launch blockers, a typed Pi contract, effective profile fidelity, one status projection, operator escalation, and ambient Pi feedback.
- Treat workflow runtime economics as release-relevant: avoid duplicate verification, gate first, repair only on actual gate failure or explicit policy, and expose phase durations/agent calls/command counts in reports/status.
- Prevent historical dependency reruns: keep closed dependencies satisfied and expose selected/included/skipped dependencies before workers launch.
- Dogfood the bounded-intent/TDD-discovery rule through docs and worker prompt wording before adding schema fields or daemon phases; treat schema/state changes as evidence-driven follow-ups only if repeated runs prove the lightweight rule insufficient.
- Refactor seam-first: preserve `workflow::Manager` as the cohesive temporal orchestrator unless a new interface hides more behavior than it exposes. Prefer deep seams such as gate/shell execution, worker execution context, status projection, and recorded-agent economics.
- Keep the CLI status/monitor surface harness-neutral while worker execution is Pi-native: `khazad-doom status/watch/monitor` remain core over daemon JSON; Pi extensions are optional renderers and ambient feedback adapters over the same projection.
- Keep SAW/SAFe learnings narrow: make existing exit states explicit and separate worker evidence claims from daemon/human attestation; do not add 11-agent team machinery or optional gates.

## Memory hygiene

- Keep Khazad-Doom decisions and taste in this repo-local `.pi/memory` directory or in tracked repo docs such as `docs/workflow-invariants.md`.
- Do not promote Khazad-Doom-specific memory to `/home/sivanirosh/.pi/memory`.
