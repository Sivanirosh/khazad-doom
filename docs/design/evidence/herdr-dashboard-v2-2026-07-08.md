# Herdr dashboard v2 evidence — 2026-07-08

Scope: compact monitor/dashboard projection for the narrow right pane introduced by layout v2.

## Before

- The shared feed rendered separate `Todos`, singular `Worker`, `Progress`, `Activity`, `Tail`, optional `Terminal`, and optional `Replan` blocks.
- Long worker profile summaries such as `implementer: provider=... model=... reasoning=... mode=...` could wrap in the right panel.
- Worker state appeared in more than one block, making the dashboard feel duplicated when a slice worker was active.

## After

Dashboard v2 projects daemon status/feed JSON into these compact sections:

1. `Run` — status, phase, repo, compact worker profile, and run message.
2. `Workers` — active worker/slice summary plus bounded selected-slice status lines.
3. `Attention` — terminal reasons, pending questions, and pending replan decisions; attention text is not truncated.
4. `Commands` — only when daemon-owned operator commands are actionable.
5. `Checks` — verification profile, current phase/check command, gate/repair state, and semantic progress.
6. `Economics` — compact agent, command, duplicate, cache, and repair counters.
7. `Incidents` — only when daemon status includes incidents.

The renderer still paints `StatusFeed` blocks only. It may choose compact line width/color, but terminal reasons, attention, and operator commands come from daemon status/feed fields.

## Invariant

`khazad-doom monitor`, Herdr dashboard panes, and related dashboard renderers are attach-only observability over daemon state. Attaching, detaching, repainting, or truncating non-attention display lines must not alter run state, worker state, verification evidence, commands, handoff readiness, or source-of-truth artifacts.
