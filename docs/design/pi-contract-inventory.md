# Pi contract inventory

Khazad-Doom treats Pi as the only real worker harness, but keeps daemon state authoritative. This inventory documents the Pi process surface Khazad-Doom currently relies on.

## Launch surface

Khazad-Doom launches Pi directly from the worker worktree:

```text
pi <profile args> <operator args> --mode json --no-session
```

The effective worker profile resolver supplies profile args from the operator-wide `~/.khazad-doom/agents.toml` (required `implementer` profile) and appends per-run overrides from CLI/env. Repo-local `.workflow/agents.toml` is optional compatibility/fallback metadata and is overridden by the operator-wide file. The built-in operator profile emits:

```text
--provider <provider> --model <model> --thinking <reasoning>
```

`--mode json --no-session` are Khazad-owned contract flags added by `src/pi_contract.rs`. `preflight.json` records the binary, launch flags, supported contract version, and event vocabulary for each run.

## Stdout event vocabulary

`src/pi_contract.rs` is the only module that interprets Pi stdout JSON. Source-backed contract facts for local Pi 0.80.3:

- `dist/modes/print-mode.js.map` (`src/modes/print-mode.ts`) shows `--mode json` writes one `session` header, then writes every `session.subscribe(...)` event as JSONL.
- `dist/core/agent-session.js.map` (`src/core/agent-session.ts`) defines the session event union and session-owned event additions.
- `@earendil-works/pi-agent-core/dist/types.d.ts` defines the core `AgentEvent` union.
- `@earendil-works/pi-agent-core/dist/agent-loop.js.map` (`src/agent-loop.ts`) shows only partial assistant stream events are forwarded as `assistantMessageEvent` inside `message_update`.
- `@earendil-works/pi-ai/dist/types.d.ts` defines the assistant stream event and usage shapes.

Known top-level JSON event types:

- `session`
- `agent_start`
- `turn_start`
- `message_start`
- `message_update`
- `message_end`
- `tool_execution_start`
- `tool_execution_update`
- `tool_execution_end`
- `turn_end`
- `agent_end`
- `queue_update`
- `compaction_start`
- `compaction_end`
- `session_info_changed`
- `thinking_level_changed`
- `auto_retry_start`
- `auto_retry_end`

Known `assistantMessageEvent.type` payloads forwarded by Pi's agent loop:

- `text_start`
- `text_delta`
- `text_end`
- `thinking_start`
- `thinking_delta`
- `thinking_end`
- `toolcall_start`
- `toolcall_delta`
- `toolcall_end`

Provider stream internals `start`, `done`, and `error` are consumed by Pi's agent loop and are not forwarded as `assistantMessageEvent` values. `text_complete` is not source-defined in local Pi 0.80.3 and is not treated as part of Khazad-Doom's Pi contract.

Unknown event types and extra fields are tolerated. A missing version marker is tolerated. A future contract version produces at most one warning per worker parse, then the run continues.

## Usage and transcript

The contract parser assembles:

- assistant text for worker JSON extraction,
- bounded stdout/stderr/assistant transcript tails for failure classification,
- token usage from Pi's source-defined `usage.input` / `usage.output` payloads,
- bounded contract warnings for daemon incidents.

Malformed final worker JSON remains a worker-output error; the parser does not make daemon workflow decisions beyond producing typed parse data.

## Launch/auth failures

Pi auth readiness is classified after the first real launch failure, not through a separate probe. The classifier requires no assistant output plus a known Pi auth stderr signature such as:

```text
No API key found for <provider>.
Use /login to log into a provider via OAuth or API key.
```

Classified auth failures are non-retryable, operator-action-required launch failures with `fix_commands` (currently `pi /login`). Unknown or ambiguous launch failures retain retry behavior.

## Boundary rule

No file outside `src/pi_contract.rs` should parse Pi event type strings, Pi stdout/stderr byte formats, or Pi launch-failure signatures. Other modules may consume typed `PiParser`, `PiContractObservation`, `PiContractWarning`, `RunnerTranscript`, and `RunnerLaunchFailure` results.
