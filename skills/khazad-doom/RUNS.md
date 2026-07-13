# Start and resume runs

Read this branch before constructing any `run` or `resume` command.

## 1. Establish launch intent

Before launch, identify:

- the repository and selected open slices;
- `.workflow/khazad.json` defaults, including `parallelism`, `cockpit`, worktree setup, verification profiles, and timeouts;
- whether a mission envelope and autonomy level were requested;
- whether the source repository is clean;
- whether this is a real Pi run or a named deterministic fake test.

Use the slice rules in [`SLICES.md`](SLICES.md) whenever initialization, contract editing, or slice selection is part of the request.

**Launch intent is complete when:** selection, authority, repository state, worker kind, cockpit policy, and optional envelope are known without guessing.

## 2. Choose the pit of success

Ordinary real runs omit `--cockpit`. The repository policy then applies; with the default `auto` policy, KD opens or focuses Herdr when usable and falls back to direct daemon execution when it is not.

Explicit execution overrides belong to these named exceptions:

- `--cockpit herdr`: an operator-requested or diagnostic one-run Herdr override.
- `--cockpit direct`: operator-requested headless execution or a bounded test that requires UI isolation.
- `--json-wrapper-worker`: an explicit compatibility test or diagnosed native-TUI incompatibility.

Whenever an exception is selected, state its reason in the run-start response. Record a durable repository preference in `.workflow/khazad.json`; keep command overrides one-run exceptions.

Common launch forms keep cockpit policy implicit:

```sh
khazad-doom run --slice <slice-id>
khazad-doom run --all --parallel <n>
khazad-doom run --envelope <mission.json> --autonomy off --slice <slice-id>
```

A mission envelope records goal, allowed areas, non-goals, `must_ask_if`, verification profile, and bounded frontier budget. `off` adds no frontier decisions; `shadow` classifies without mutation; `promote` may create eligible future slices; `run` may append eligible slices serially. Only deterministic Tier-1 follow-up proposals inside the envelope and remaining budget can be daemon-authorized. Ambiguity, exhausted bounds, prior rejection/deferment, policy changes, verification changes, and `must_ask_if` conditions remain operator decisions.

**Command selection is complete when:** every option traces to the requested work, and the ordinary case inherits repository cockpit policy and the native worker default.

## 3. Start in the background and return control

For real Pi work, launch without `--wait`. A successful command returns JSON containing `run_id`, `repo_path`, `monitor_command`, and `run_monitor_command`.

Reply with one compact handoff:

```text
Started KD run `<run-id>` in the background.
Monitor: `<run_monitor_command>`
Need action later: ask me to inspect, attend, resume, cancel, or handoff.
```

When an escape hatch was used, insert `Override: <override> — <reason>.` after the first line. Use the emitted monitor command rather than reconstructing it. End the turn after this handoff. Reserve a single status request for ambiguous startup output, startup failure, or an explicit operator request for observation. Monitoring belongs to the daemon and an attached monitor, not a chat polling loop.

**Start is complete when:** a `run_id` was returned and reported with its emitted monitor command, or the immediate launch blocker was reported exactly.

## 4. Resume from durable state

Inspect or attend first when the run has an unresolved operator question, pending replan, environment blocker, or incomplete terminal transition. Resolve the decision the operator authorizes, then run:

```sh
khazad-doom resume --run <run-id>
```

Resume schedules unfinished work from daemon state. It does not resurrect a lost process, reuse stale worker questions, or erase prior attempt evidence. Cockpit selection follows the same pit-of-success rule as a fresh run.

After a successful background resume, use the same compact handoff with `khazad-doom monitor --run <run-id>`; unlike run start, resume returns only the run ID.

**Resume is complete when:** the daemon accepted a new execution epoch and the exact run ID plus monitor command were reported, or the exact unresolved blocker remains visible to the operator.

## Optional origin feedback

The initial `run` command accepts `--origin-notification-target <target>` when a concrete target was supplied. Terminal and attention messages sent through that target are inert visibility evidence; delivery failure does not change run status, verification, merge, or handoff readiness.
