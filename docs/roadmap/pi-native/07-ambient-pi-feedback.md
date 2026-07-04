# PI-06 — Ambient Pi feedback: auto-tracking widget and notifications

Matrix row: [00-matrix.md](00-matrix.md) → PI-06. Status: `planned` (open questions block `ready`).
Depends on: PI-05 (paints the projection). Attention integration depends on PI-04; lifecycle
notifications (completed/failed/blocked) may ship before PI-04.

## Problem being removed

Feedback today is pull-only: the operator must open `/khazad-monitor` or a terminal monitor to
learn anything, including that a run needs them. The pi-subagents model demonstrates the right
default for Pi-native work: a compact widget appears when work is active, completions notify,
and needs-attention states interrupt politely. Without this, PI-04's escalation questions land
in a window nobody has open and time out unseen.

## Scope

- **Auto-tracking:** the `khazad-monitor` extension tracks active runs for the current repo
  without any user command. Two attach paths: (a) discovery polling — the extension
  periodically checks for active runs in the session's repo and shows the widget when one
  exists (also catches runs started outside this session); (b) immediate attach — the
  `khazad-doom` skill, after starting a run, invokes the extension's attach hook with the run
  id so the widget appears without waiting for the next poll.
- **Compact footer widget:** run id (short), phase, current slice, attempt, cost/agent-calls,
  data freshness. Renders lines from the PI-05 projection (`summary_line` + selected blocks);
  the widget adds layout only, never wording. Pi's expand key opens the detail view (the
  existing overlay content, now projection-painted).
- **Lifecycle notifications:** completed, failed, blocked, interrupted — one notification per
  terminal transition, carrying the projection `summary_line` and the relevant next command
  (`handoff`, `resume`, or the PI-01 fix commands for blocked runs).
- **Attention integration (with PI-04):** a pending question raises a needs-attention
  notification with the question preview and the exact `khazad-doom answer …` command. The
  skill wording instructs Pi: when the user answers in chat, run the answer command (subject to
  Pi's normal command confirmation). Widget shows a persistent attention line while any
  question is pending.
- **Auto-dismiss:** widget hides after a terminal run's notification is delivered (configurable
  linger), and when no active runs remain.
- **Degradation:** headless / no-UI Pi contexts no-op cleanly (the extension already guards on
  `ctx.hasUI`); daemon-unreachable shows the existing stale/error line with the
  `KHAZAD_DOOM_BIN` hint.
- **Opt-out:** config keys for ambient mode, notification levels, and poll interval;
  `/khazad-monitor` continues to work unchanged as the explicit detail view.

## Out of scope

- Full-screen overlay visual polish (explicitly deferred at matrix level).
- Rich multi-run UI (v1 policy below); fleet views.
- Any daemon-side change beyond what PI-04/PI-05 already provide (this slice is extension +
  skill + docs; if a daemon gap is found, it goes back to the matrix as its own row).
- Answering questions through a TUI form (chat-driven `answer` command only).
- Streaming transport (polling stays; deferred at matrix level).

## Data model changes

None. Reads the PI-05 projection and (for attention) PI-04's `listQuestions` via the CLI.

## API changes

None to the daemon. Extension config surface documented (ambient on/off, intervals,
notification levels). Skill (`skills/khazad-doom`) gains the attach hook call after `run`
and the answer-in-chat instruction for pending questions.

## UI states

- **Active run:** widget visible with live phase/slice/cost; freshness indicator when polling
  lags.
- **No active runs (empty):** widget hidden; nothing rendered.
- **Multiple active runs:** v1 policy — widget shows the most recent plus an `(+N more)`
  count; expand lists all. No per-run tabs.
- **Awaiting operator:** attention-styled widget line + one needs-attention notification per
  question (no re-notification on every poll).
- **Terminal transition:** one notification; widget lingers briefly, then dismisses.
- **Daemon unreachable:** stale/error line with fix hint; polling continues with backoff.
- **Headless/no-UI:** complete no-op; no errors in Pi logs beyond debug level.
- **Reattach (new session, run still active):** widget reappears from discovery polling; no
  replay of past lifecycle notifications (see dedup rule).

## Migration / backward compatibility

`/khazad-monitor` behavior is unchanged for users who prefer pull-only; ambient mode is on by
default with a documented opt-out. No daemon compatibility concerns (read-only consumer).
Sessions running an older extension simply lack ambient behavior.

**Notification dedup rule (hard requirement):** notifications are keyed by
`(run_id, transition)` and recorded in extension session state; a session never notifies the
same transition twice, and a *newly attached* session does not notify transitions that
predate its attachment. Two sessions watching the same repo may each notify once — that is
accepted v1 behavior (each operator surface learns once); cross-session dedup is an open
question, not a silent assumption.

## Permissions

Extension is read-only over daemon state via CLI exec. The answer flow executes
`khazad-doom answer` through Pi's normal tool/command permission prompts — the extension never
bypasses them. Token enforcement stays in the daemon (PI-04); the extension holds no secrets.

## Test plan

Unit (JS, mocked Pi API + fixture projections):
- Attach paths: discovery poll shows widget; skill attach hook shows it immediately.
- Dedup: same transition never notifies twice; pre-attachment transitions suppressed.
- State machine: active → awaiting → answered → terminal renders the right widget lines and
  exactly the right notifications.
- Headless guard: no UI calls when `hasUI` is false.
- Unknown projection roles/blocks: painted as plain text (shared with PI-05 tolerance tests).

Integration:
- Against a real daemon with the fake runner: start run → widget data updates → terminal
  notification carries the projection summary and next command.
- With PI-04's scripted fake worker: question → needs-attention payload contains the answer
  command verbatim.

### Workflow acceptance test

```text
1. User asks Pi (khazad-doom skill) to run a slice; the run starts and the footer widget
   appears without the user opening any monitor view.
2. The widget updates through worker phases; the user keeps chatting with Pi meanwhile.
3. The worker hits a must_ask_if fence (PI-04); a needs-attention notification shows the
   question and the answer command; the user answers in chat; Pi runs `khazad-doom answer`
   after its normal confirmation; the run resumes and later completes with one completion
   notification; the widget dismisses.
4. Edge condition: the user closes the Pi session mid-run and opens a new one in the same
   repo. The widget reappears for the still-active run (daemon durability), and the new
   session does NOT replay old lifecycle notifications.
5. Second edge: the daemon becomes unreachable mid-run; the widget shows the stale/error
   line with the fix hint and recovers automatically when the daemon returns.
6. Invariants: the extension issued no state-mutating command except the user-confirmed
   answer; every string shown in widget and notifications came from the PI-05 projection
   or the PI-04 question record (no extension-invented wording); notification count per
   (run, transition) per session is exactly 0 or 1.
```

## Acceptance criteria

1. A run started from a Pi session surfaces a widget with no user action; discovery polling
   also picks up externally started runs in the repo.
2. Terminal transitions produce exactly one notification per session, carrying the projection
   summary and the correct next command.
3. Pending PI-04 questions raise needs-attention notifications with the copy-pasteable (and
   chat-executable) answer command.
4. Reattach after session restart shows the widget without notification replay.
5. Headless contexts and daemon outages degrade cleanly.
6. All rendered wording originates from the projection/question records (grep + tests).
7. Ambient mode is configurable; `/khazad-monitor` unchanged for pull-only users.

## Open questions (block `ready`)

1. **Pi widget API surface:** confirm the installed Pi extension API supports a persistent
   footer widget with an expand-key hook (the current overlay uses a different surface);
   identify the exact APIs pi-subagents uses for its async widget and whether they are public.
2. **Skill → extension attach hook:** what is the supported mechanism for a skill-triggered
   extension call (custom command, event, or file drop)? Fallback: discovery polling only,
   with a shorter first-poll interval after the skill starts a run.
3. **Cross-session dedup:** is once-per-session notification acceptable long-term, or should
   the daemon record delivered notifications? (Recommend once-per-session v1; daemon-side
   delivery state only if operator feedback shows double-notification pain.)
4. **Notification channel for unattended operators:** is Pi's in-session notification enough,
   or is an OS-level notification hook wanted for long waits? (Recommend defer; measure how
   often PI-04 questions time out first.)

## Definition of Done

- [ ] Data model changes — explicitly not needed.
- [ ] API changes — none to daemon (verified); extension config + skill hook documented.
- [ ] All named UI states implemented: active, empty, multi-run, awaiting, terminal,
      unreachable, headless, reattach.
- [ ] Permissions: answer flow goes through Pi's confirmation; extension read-only otherwise.
- [ ] Migration: opt-out documented; pull-only path unchanged.
- [ ] Unit tests pass (mocked Pi API + fixture projections).
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: README operator flow, skill wording, extension config reference.
- [ ] Invariants checked: dedup rule, projection-only wording, no unconfirmed state mutation.
