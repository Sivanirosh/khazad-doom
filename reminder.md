# Reminder: Khazad-Doom automatic escalation/notifications

We should add daemon-native escalation so operators do not need to poll `status` / `monitor` manually to discover failures or ask-user blockers.

## Problem

Current Pi UX detaches after `khazad-doom run` starts. If a run later fails, blocks, or needs user input, the operator only notices by manually checking `monitor`, `watch`, or `status`.

Recent example: a run failed at the integration gate because `vitest` was missing in the integration worktree. The daemon had the right structured finding (`failure_kind: tool_missing`, `action: operator-fix`), but no proactive escalation reached the operator.

## Desired behavior

Khazad-Doom should proactively notify/escalate when a run enters an attention state:

- `failed`
- `blocked`
- `cancelled` if unexpected
- `ask-user` finding
- integration gate failed
- worker quiet/no-output warning when policy says it needs attention
- optionally `completed`

## Suggested notification payload

```json
{
  "type": "needs_attention",
  "run_id": "kd-...",
  "status": "failed",
  "severity": "error",
  "summary": "integration gate failed: vitest not found",
  "action": "operator-fix",
  "monitor_command": "khazad-doom monitor --run kd-...",
  "inspect_command": "khazad-doom inspect --run kd-..."
}
```

## Suggested config

In `.workflow/khazad.json`:

```json
{
  "notifications": {
    "enabled": true,
    "on": ["failed", "blocked", "ask-user", "worker_quiet"],
    "debounce_seconds": 60,
    "channels": [
      { "type": "desktop" },
      { "type": "command", "command": "khazad-alert" }
    ]
  }
}
```

## Implementation direction

Add a daemon-side `Notifier` module/seam:

- invoked when events/incidents are persisted or when run state transitions to terminal/attention states
- dedupes by `run_id + event_type + status/finding`
- never blocks daemon execution
- notification failures become warnings, not run failures
- test with fake sink

Initial channels:

1. Linux desktop notification via `notify-send` if available.
2. Configured command hook receiving JSON on stdin.
3. Later: Pi extension/TUI notification, webhook, Slack/Discord/email.

## Acceptance sketch

- A failed integration gate creates a single proactive notification with run id, summary, and inspect/monitor commands.
- An `ask-user` blocker creates a proactive notification with exact question/finding details.
- Repeated status updates do not spam notifications.
- Notification sink failure does not fail the run.
