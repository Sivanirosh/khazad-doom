# Pi contract inventory

Khazad-Doom treats Pi as the only real worker harness, but keeps daemon state authoritative. This inventory documents the Pi process surface Khazad-Doom currently relies on.

## Launch surface

Khazad-Doom launches Pi directly from the worker worktree:

```text
pi <profile args> <operator args> --mode json --no-session
```

The effective worker profile resolver supplies profile args from `.workflow/agents.toml` (required `implementer` profile) and appends per-run overrides from CLI/env. The built-in profile emits:

```text
--provider <provider> --model <model> --thinking <reasoning>
```

`--mode json --no-session` are Khazad-owned contract flags added by `src/pi_contract.rs`. `preflight.json` records the binary, launch flags, supported contract version, and event vocabulary for each run.

## Stdout event vocabulary

`src/pi_contract.rs` is the only module that interprets Pi stdout JSON. Current known event types observed from Pi 0.80.3:

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

Known assistant payload shapes:

- `thinking_start`
- `thinking_delta`
- `thinking_end`
- `toolcall_start`
- `toolcall_delta`
- `toolcall_end`
- `text_start`
- `text_delta`
- `text_end`
- `text_complete`

Unknown event types and extra fields are tolerated. A missing version marker is tolerated. A future contract version produces at most one warning per worker parse, then the run continues.

## Usage and transcript

The contract parser assembles:

- assistant text for worker JSON extraction,
- bounded stdout/stderr/assistant transcript tails for failure classification,
- token/cost usage from recognized usage payloads,
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
