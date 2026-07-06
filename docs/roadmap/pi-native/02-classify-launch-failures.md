# PI-01 — Classify non-retryable Pi launch failures

Matrix row: [00-matrix.md](00-matrix.md) → PI-01. Status: `in_progress` after the 2026-07-06 truth audit: source, tests, and current installed-binary smoke pass, but slice acceptance/closure has not been dogfooded.

Supersedes the preflight-first proposal in the worker-run complexity audit; the audit's review addendum is the normative design.

## Scope

Make deterministic environmental launch failures truthful and cheap:

- Give `RunnerError` (`src/agent.rs:148`) a classified `kind`. First kind: `agent_auth_required`. Add it to the existing `failure_kind` taxonomy and `failure_kind_needs_operator` (`src/workflow/gate.rs:605`).
- Classification signal is **structural + textual**: the Pi process exited without producing any agent output (`RunnerTranscript.assistant_tail` empty) AND stderr matches a narrow auth signature (e.g. `No API key found for`). Patterns live in one documented place in `src/pi_contract.rs`.
- In the worker attempt loop (`src/workflow/manager.rs:1647`): an operator-class launch error becomes `BlockedError` immediately — no attempts 2 and 3. The existing error mapping already turns `BlockedError` into slice/run `blocked` and stops later layers (`manager.rs:1468`); rely on it, do not add new abort machinery.
- Record a `run_incident` carrying `failure_kind`, provider/model/profile from `RunnerMetadata`, and concrete fix guidance: `pi /login` and pointer to `~/.khazad-doom/agents.toml`.
- Apply the same classification to the integration **repair** agent call path (the gate itself is shell commands and already classified).

## Out of scope

- A generic `FailureClassifier` module (deferred; extend existing taxonomy instead).
- A pre-launch readiness probe (rejected: only Pi can authoritatively answer whether Pi can run; a probe either re-implements Pi auth resolution or spends an agent call). Revisit only if Pi ships a no-op auth-check flag — then run it once per run, not per slice.
- New `RunStatus`/`SliceStatus` enum values (existing `Blocked` is sufficient).
- Auth remediation of any kind.

## Data model changes

None to SQLite schema or slice JSON schema. New `failure_kind` string value `agent_auth_required` appears in incident/event JSON payloads (additive; consumers ignore unknown kinds).

## API changes

None to IPC methods. `status`/`inspect` responses gain nothing structural; incident payloads carry the new kind and `fix_commands` array. Document the payload shape in `docs/workflow-invariants.md`.

## UI states (CLI/monitor output)

- **Blocked (new path):** terminal summary states the worker never started, shows provider/model/profile, attempts consumed = 1, and prints fix commands verbatim.
- **Success:** unchanged.
- **Unmatched launch error:** identical to today (3 attempts, `failed`) — the fallback state, asserted by test.
- **Monitor:** incident feed shows the readiness incident with the same wording as `status` (no divergent phrasing).

## Migration / backward compatibility

Behavior change: runs that previously ended `failed` after three identical auth failures now end `blocked` after one. Document in invariants and changelog. No stored-state migration.

**Misclassification safety rule (hard requirement):** a false positive (marking a retryable failure non-retryable) is worse than a false negative (status-quo three retries). Patterns stay narrow; any launch error that matches no signature keeps today's retry behavior byte-for-byte.

## Permissions

Not applicable (no new external surface).

## Test plan

Unit:
- Classifier recognizes auth stderr + empty `assistant_tail` → `agent_auth_required`, `retryable = false`.
- Auth-looking stderr with non-empty `assistant_tail` (mid-work mention) → not classified, retryable.
- Unknown launch error → unclassified, retryable.
- `failure_kind_needs_operator("agent_auth_required")` is true.

Integration (`tests/daemon_integration.rs`):
- Fake `pi` binary exits immediately with the missing-auth stderr → run `blocked`, slice attempts == 1, incident contains provider/model/profile and `pi /login`.
- Multi-layer slice set: layer 1 blocks on auth → later layers never dispatch.
- Repair-phase agent call with same fake binary → same classification, no repair retry burn.
- `--agent fake` end-to-end run stays green (deterministic smoke path preserved).

### Workflow acceptance test

```text
1. Operator runs `khazad-doom run --slice S` with a stub pi that prints
   "No API key found for openai" to stderr and exits nonzero, producing no agent events.
2. Run ends blocked; status output shows: worker never started, attempts consumed 1,
   configured provider / model gpt-5.5 / profile implementer, fix commands.
3. Operator replaces the stub with one that fails with an unrecognized error string.
4. Edge condition: unrecognized launch failure → system falls back to exactly the
   pre-slice behavior: 3 attempts, run failed, no blocked classification.
5. Invariant: in both outcomes, no worker-attempt artifact implies implementation work
   happened (no commit SHAs, no diff evidence), and the incident/event log is sufficient
   to explain the outcome without reading daemon stderr.
```

## Acceptance criteria

1. Pi missing-auth launch failure → `agent_auth_required`, operator-action-required.
2. Slice/run become `blocked`, not `failed`.
3. Attempts 2 and 3 are not consumed for non-retryable operator-class launch failures.
4. Incident/terminal summary include provider/model/profile and concrete fix guidance.
5. Regression test with fake `pi` binary asserts blocked status, one attempt, actionable incident text.
6. Classification path is shared with the repair agent call.
7. Unmatched errors retain current retry behavior (fallback asserted by test).

## Open questions

None blocking. (PI-02 may later replace the stderr signature with a typed Pi error event; the classifier interface should take the transcript struct, not raw strings, to make that swap local.)

## Definition of Done

- [ ] Data model changes — explicitly not needed (additive JSON payload only).
- [ ] IPC/CLI contracts documented for the incident payload.
- [ ] All named output states implemented: blocked-with-guidance, fallback-unchanged, monitor parity.
- [ ] Permissions — not applicable.
- [ ] Backward-compat behavior change documented; misclassification safety rule enforced by test.
- [ ] Unit tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: `docs/workflow-invariants.md` (taxonomy + "operator-class failures never consume retries" invariant).
- [ ] Invariants checked: no orphaned run state; blocked runs carry exactly one attempt's artifacts.
