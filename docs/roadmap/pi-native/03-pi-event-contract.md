# PI-02 — Typed, versioned Pi event/CLI contract

Matrix row: [00-matrix.md](00-matrix.md) → PI-02. Status: `in_progress` after the 2026-07-06 truth audit: source/unit evidence exists, but preflight contract recording and fixture/workflow acceptance are not proven.

## Scope

Turn the implicit Pi wire-format knowledge scattered through `PiRunner`/`PiParser` into one deep module that owns everything Khazad knows about Pi's process surface:

- **Discovery first (timeboxed, half a day):** inventory the installed Pi version's contract — CLI flags Khazad uses (`-p`, model/provider args), stdout JSON event vocabulary (`agent_end`, tool events, usage), exit-code semantics, and whether events carry a version marker or typed error events. Record the inventory as a contract table in the workpackage before writing code.
- Extract a `pi_contract` module (new file or clearly bounded section of `src/agent.rs`) that is the *only* code allowed to interpret Pi bytes: event parsing, usage extraction, transcript assembly, launch-failure signatures (from PI-01), and actual model/provider extraction when Pi reports it.
- Forward-compatibility rules as code: unknown event types and unknown fields are ignored, never fatal; a missing version marker is tolerated; a higher version logs one incident-level warning per run, not per event.
- Record the observed contract (flags, events, version) into the run's `preflight.json` so postmortems show which Pi surface a run spoke to.
- If discovery finds typed launch-failure events: swap PI-01's stderr signature for the typed signal inside this module (classifier interface unchanged).

## Out of scope

- Consuming pi-subagents' lifecycle artifact files (Khazad drives `pi -p` directly; deferred at matrix level).
- Changing what events the daemon *records* (`state.record_event` vocabulary unchanged).
- Supporting multiple Pi major versions simultaneously (tolerate-and-warn only).

## Data model changes

None to schema. `preflight.json` gains a `pi_contract` object (binary path, resolved version string if obtainable, event-vocabulary hash or list). Additive.

## API changes

Internal only: `PiRunner` consumes the contract module; `Runner` trait signature unchanged. Document the module boundary rule in `docs/workflow-invariants.md`: *no code outside `pi_contract` may parse Pi stdout/stderr.*

## UI states (CLI/monitor output)

- **Success:** unchanged output; `inspect` shows the recorded contract info.
- **Unknown event/field encountered:** run proceeds; one warning incident max.
- **Version ahead of supported:** run proceeds; warning names the observed vs supported version.
- **Malformed final JSON (existing failure):** unchanged behavior, now produced by the contract module with the same message.

## Migration / backward compatibility

Pure refactor plus additive artifact field. Recorded event-stream fixtures captured from the current Pi version become the regression corpus; the refactored parser must produce identical `ResultData`/transcripts for all fixtures.

## Permissions

Not applicable.

## Test plan

Unit:
- Fixture replay: current-Pi event streams → identical parse results pre/post refactor.
- Unknown event type, unknown field, missing version → tolerated, single warning.
- Truncated/interleaved stdout lines → same behavior as today (fixtures assert it).

Integration:
- Full fake-runner and stub-pi runs still green.
- `preflight.json` contains the contract record.

### Workflow acceptance test

```text
1. Operator runs a slice with the current Pi; run completes; inspect shows the recorded
   Pi contract (binary, version, event vocabulary).
2. Operator upgrades to a stub "future Pi" that emits two unknown event types and an
   extra field on agent_end.
3. Edge condition: the unknown events flow through mid-run.
4. System completes the run normally with exactly one contract warning incident;
   worker result, checks, and economics are unaffected.
5. Invariant: grep confirms no file outside the contract module references Pi event
   type strings or parses Pi stderr; daemon state remained authoritative throughout
   (no correctness decision was made from an unparsed byte).
```

## Acceptance criteria

1. One module owns all Pi wire-format knowledge; boundary rule documented and grep-verifiable.
2. Contract inventory documented; observed contract recorded per run.
3. Unknown fields/events tolerated with bounded warnings.
4. Fixture corpus exists and passes; behavior byte-compatible for current Pi.
5. Actual model/provider captured when Pi reports it (feeds PI-03 attestation truthfulness).

## Open questions (block `ready`)

1. Does the installed Pi version the stdout event stream or expose a `--version`/capabilities probe? (Discovery task.)
2. Does Pi emit typed error events for launch/auth failures, or is stderr the only channel? (Determines whether PI-01's signature gets upgraded here.)
3. Does Pi report the resolved model per response (for fallback/alias resolution), enabling per-attempt model attestation?

## Definition of Done

- [ ] Data model changes — explicitly not needed (additive artifact field only).
- [ ] Module boundary documented; no external Pi-byte parsing (grep check in CI or test).
- [ ] All named output states implemented: unknown-event warning, version warning, unchanged success/error paths.
- [ ] Permissions — not applicable.
- [ ] Backward compatibility proven by fixture corpus.
- [ ] Unit tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: contract inventory + boundary rule in `docs/workflow-invariants.md`.
- [ ] Invariants checked: warnings bounded per run; no behavior change for current Pi fixtures.
