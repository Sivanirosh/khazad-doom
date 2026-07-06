# PI-03 — Pi profile fidelity: one effective worker profile

Matrix row: [00-matrix.md](00-matrix.md) → PI-03. Status: `in_progress` after the 2026-07-06 truth audit: profile resolution tests pass, but dedicated integration proof across run/handoff/report/status surfaces is missing.

Reframes the audit's deferred "WorkerProfile as first-class module": the trigger is no longer hypothetical multi-provider support but Pi profile fidelity under the PI-00 commitment.

## Scope

One module computes the **effective worker profile** and everything downstream derives from it:

- Precedence chain computed and tested in exactly one place: CLI flags (`--agent`, `--pi-bin`, `--pi-args`) > env (`KHAZAD_AGENT`, `KHAZAD_PI_BIN`, `KHAZAD_PI_ARGS`) > `.workflow/khazad.json` agent choice for worker kind > operator-wide `~/.khazad-doom/agents.toml` profile > built-in default profile. Repo-local `.workflow/agents.toml` is not read.
- Pi launch args (model, provider, reasoning/thinking, mode) generated from the profile in one function.
- `RunnerMetadata` derived from the profile, never assembled ad hoc; `run_started` events, handoffs, reports, and monitor render one shared `launch_summary` string.
- Profile carries its own operator fix guidance (auth command, config file path) — consumed by PI-01's incident text so wording lives in one place.
- Validation at resolution time: unknown profile name, malformed `agents.toml`, or contradictory sources fail run admission with a direct CLI error (this *is* cheap true preflight — config parsing, not environment probing; it does not violate PI-01's no-probe decision).

## Out of scope

- Multi-provider abstraction layers; provider plug-ins.
- `fallbackModels` (rejected at matrix level, D5).
- Changing `agents.toml`/`khazad.json` file formats (additive keys allowed if needed for reasoning/mode; document them).
- Readiness probing of the live environment (PI-01 owns launch-failure truth).

## Data model changes

None to SQLite. Possible additive keys in `~/.khazad-doom/agents.toml` (e.g. explicit `thinking`/`mode`) are documented, optional, and defaulted.

## API changes

IPC `StartRunParams`/`ResumeRunParams` unchanged (existing `agent`, `pi_bin`, `pi_args` fields feed the resolver). CLI unchanged except clearer errors. `run_started` event and handoff JSON gain/normalize a `profile_summary` field (additive).

## UI states (CLI/monitor output)

- **Success:** `status`, monitor, report, and handoff show the identical profile summary line.
- **Invalid profile/config:** run admission fails synchronously with the offending file, key, and accepted values; no run row created (config errors are admission errors, matching existing dirty-repo behavior).
- **Empty/absent operator `agents.toml`:** built-in default operator profile used and *said so* in the summary (no silent implicitness).
- **Conflicting sources:** resolution is not an error — precedence applies — but `inspect` shows which source won for each field.

## Migration / backward compatibility

Current invocations keep working: same env vars and CLI flags. Intentional divergence: repo-local `.workflow/agents.toml` is ignored and may be deleted; this removes cross-repo drift where stale generated repo files point at an unauthenticated provider.

## Permissions

Not applicable.

## Test plan

Unit:
- Precedence table test: every source combination → expected effective profile (the table is the spec).
- Arg generation: profile → exact `pi` argv.
- Validation errors name file/key/accepted values.

Integration:
- `run_started` event, handoff JSON, and final report contain the identical profile summary.
- `--agent fake` bypasses Pi profile resolution and stays green.

### Workflow acceptance test

```text
1. Operator sets model X in `~/.khazad-doom/agents.toml`, then overrides with KHAZAD_PI_ARGS for one run.
2. Run starts; status and run_started show the env-derived profile and inspect attributes
   each field to its winning source.
3. Operator introduces a typo'd profile key in the operator-wide agents.toml and reruns.
4. Edge condition: malformed config → admission fails immediately with file, key, and
   accepted values; no run row, no worker attempt, no incident noise.
5. Operator fixes the typo; rerun completes; handoff JSON profile summary matches the
   run_started summary byte-for-byte.
6. Invariant: at no point did two surfaces (status, monitor, handoff, report) display
   different provider/model/profile for the same run.
```

## Acceptance criteria

1. Config precedence documented and tested in one place.
2. `RunnerMetadata` derived from the profile module only.
3. Pi launch args generated in one place.
4. Operator-facing surfaces display the same profile summary everywhere.
5. Config errors fail admission synchronously with actionable messages.

## Open questions (block `ready`)

1. Current exact precedence between `khazad.json`, operator-wide `agents.toml`, built-in defaults, and CLI/env overrides must be captured as the behavior-preservation table before finishing PI-03.
2. Are reasoning/mode currently expressible in config, or only via raw `pi_args`? If only raw args, decide whether to add typed keys now (additive) or defer.

## Definition of Done

- [ ] Data model changes applied or explicitly not needed (additive TOML keys documented if added).
- [ ] IPC/CLI contracts unchanged; additive event/handoff fields documented.
- [ ] All named output states implemented: success parity, invalid-config admission error, default-profile transparency, source attribution.
- [ ] Permissions — not applicable.
- [ ] Behavior-preservation table verified; intentional divergences listed.
- [ ] Unit tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs updated: precedence table in docs; operator-wide `agents.toml` reference.
- [ ] Invariants checked: single source of profile truth; no ad-hoc `RunnerMetadata` construction remains (grep check).
