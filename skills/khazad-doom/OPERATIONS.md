# Operate daemon and runs

Read this branch before checking daemon health or observing, attending, answering, replanning, focusing a cockpit, cancelling, inspecting, or handing off a run.

## 1. Read daemon truth

Choose the narrowest surface that answers the request:

```sh
khazad-doom daemon status
khazad-doom status --run <run-id>
khazad-doom monitor --run <run-id>
khazad-doom watch --run <run-id>
khazad-doom attend --run <run-id>
khazad-doom questions --run <run-id>
khazad-doom inspect --run <run-id>
khazad-doom cockpit open --run <run-id>
```

- `daemon status` checks daemon process health; it does not describe a run.
- `status` returns one authoritative run snapshot.
- `monitor` is an attach-only terminal dashboard; exiting it leaves the daemon run untouched.
- `watch` is the plain-text fallback.
- `attend` presents daemon-owned pending decisions and exact commands.
- `questions` lists recorded worker questions and outcomes.
- `inspect` exposes recorded artifacts and bounded daemon diagnostics.
- `cockpit open` opens or focuses Herdr for an existing run without changing workflow state; unavailable Herdr returns fallback commands.

When the operator explicitly requests the latest repo run instead of naming an ID, use the matching selector:

```sh
khazad-doom status --repo . --latest --include-terminal
khazad-doom monitor --repo . --latest
khazad-doom attend --repo . --latest
khazad-doom inspect --repo . --latest
khazad-doom cockpit open --latest --repo .
```

`monitor --latest` remains attached and waits for future active runs. Use `status`, `inspect`, or `cockpit open` for a one-shot latest-terminal lookup.

Render and interpret `feed.summary_line`, `feed.attention`, `feed.terminal_reason`, and `feed.operator_commands`. Preserve full attention text and exact commands. Pane text, scrollback, labels, and Herdr agent status are observability only.

**Observation is complete when:** the requested snapshot or attachment is available and no lifecycle mutation was inferred from UI state.

## 2. Interpret runtime evidence conservatively

`Supervisor: alive` means KD still observes the child process; it is not proof of semantic progress. A quiet-worker warning is advisory unless an explicit attempt timeout or policy made it terminal. Offer the operator three truthful choices: wait, inspect evidence, or cancel with a reason.

Verification timeouts protect individual commands, not the whole run. `worker_attempt_timeout_seconds: 0` disables fatal attempt timeout. `worker_question_timeout_seconds: 0` waits indefinitely; any nonzero value is the daemon-owned absolute question deadline shown in the feed.

Visibility incidents, painter exits, and terminal-feedback failures may coexist with a correct run. Report them as incidents rather than changing the workflow conclusion.

**Interpretation is complete when:** every claim is supported by daemon state or artifacts and advisory evidence is still labeled advisory.

## 3. Resolve operator attention

### Worker questions

Prefer the visible Pi `ask_operator` dialog in the active worker pane. When it is unavailable, ask the operator for the answer and use the exact feed action, typically:

```sh
khazad-doom answer <run-id> <question-id> "<answer>"
```

A stale or interrupted attempt cannot receive a new answer. Resume first and answer the fresh question attached to the active launch.

Recommendation timeout is eligible only when the recommendation exactly matches one declared option and the worker truthfully attests that it stays inside current slice or mission authority and is reversible. Credentials, secrets, permissions, scope expansion, destructive or irreversible actions, release/push/handoff authority, and work outside the envelope remain operator-only decisions.

### Replan proposals

List first when the proposal ID is unknown, then use the exact action emitted by the daemon:

```sh
khazad-doom replan list <run-id>
khazad-doom replan accept <run-id> <proposal-id> --reason "<reason>"
khazad-doom replan reject <run-id> <proposal-id> --reason "<reason>"
khazad-doom replan defer <run-id> <proposal-id> --until "<condition>" --reason "<reason>"
khazad-doom replan supersede <run-id> <proposal-id> <replacement-id> --reason "<reason>"
```

Create a proposal for an operator-requested durable change; use `khazad-doom replan propose --help` for its evidence and `--change kind:target:summary` fields. Accepted changes flow through the idempotent replan engine with provenance. Keep slice files, queue order, verification, areas, and policy unchanged while a proposal awaits disposition.

**Attention is complete when:** daemon state records the authorized answer or disposition, or the exact unanswered decision and its next command have been returned to the operator.

## 4. Cancel explicitly

Cancellation is an operator mutation:

```sh
khazad-doom cancel --run <run-id> --reason "<reason>"
```

Closing Pi, Herdr, a monitor, or the invoking shell is not cancellation. Relay the resulting daemon state after an explicit cancel request; do not infer completion from process or pane disappearance.

**Cancellation is complete when:** the daemon records the request or returns an exact reason it could not.

## 5. Inspect and hand off recorded evidence

Use `inspect` for artifact paths and bounded log tails. Runtime artifacts under `.workflow/runs/<run-id>/` are evidence, not an alternate control interface.

Generate handoff from daemon state:

```sh
khazad-doom handoff --run <run-id>
khazad-doom handoff --run <run-id> --dry-run
```

Handoff prints by default. Push and PR creation require explicit operator flags or durable configuration; `--dry-run` suppresses configured actions. Pending replan proposals block readiness until disposition. Final reports expose exit states, attestation, and plan revisions as read-only summaries rather than new gates.

**Handoff is complete when:** readiness and blockers come from daemon evidence, every pending proposal has a disposition, and any push or PR side effect was explicitly authorized.
