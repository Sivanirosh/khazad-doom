# Worker run complexity audit

Date: 2026-07-04

Trigger: run `kd-20260704-202818-77a66b53` failed before any useful worker execution because Pi exited immediately with:

```text
No API key found for openai.
Use /login to log into a provider via OAuth or API key.
```

The immediate failure is environmental. The design problem is broader: a simple user intent (`khazad-doom run --slice ...`) currently exposes too many hidden dependencies between slice selection, daemon state, worker profile config, Pi authentication, retry policy, failure classification, and recovery guidance.

## Design lens

Use the project's deep-module vocabulary:

- **Module**: anything with an interface and implementation.
- **Interface**: everything a caller/operator must know to use a module correctly, including informal requirements and error modes.
- **Depth**: leverage behind a small interface.
- **Seam**: the location where a module's interface lives.
- **Adapter**: a concrete implementation at a seam.

Complexity symptoms used for this audit:

- **Change amplification**: small behavior changes require edits across many places.
- **Cognitive load**: the operator or maintainer must know too much to complete a task.
- **Unknown unknowns**: required facts are not discoverable until failure.
- **Dependencies**: code/config cannot be understood or changed in isolation.
- **Obscurity**: important facts are hidden, implicit, or poorly classified.

## Current run path

Observed source path for a normal run:

```text
CLI run command
  -> cli::run_start
  -> RunnerSpec::from_agent_and_env
  -> daemon RPC startRun
  -> Manager::start_run
  -> artifact::Store::read_config
  -> artifact::Store::load_slices / topological_order
  -> dirty-repo preflight
  -> Manager::runner_for_options
  -> Manager::runner_for_parts
  -> artifact::Store::read_agent_profiles
  -> apply_implementer_profile_to_pi_spec
  -> runner_from_spec
  -> durable Run + SliceRun rows + preflight.json
  -> background Manager::execute_run
  -> Manager::run_slices
  -> integration worktree creation
  -> Manager::run_worker_layer
  -> Manager::run_slice_worker
  -> per-attempt handoff + prompt
  -> Manager::run_recorded_agent_job
  -> Manager::run_supervised_worker_job
  -> PiRunner::run
  -> external pi process
```

Relevant files:

- `src/cli.rs`
- `src/agent.rs`
- `src/workflow/manager.rs`
- `src/domain.rs`
- `src/artifact.rs`
- `.workflow/khazad.json`
- `~/.khazad-doom/agents.toml`
- optional `.workflow/agents.toml`
- `docs/workflow-invariants.md`

## What the user-facing interface suggests

The apparent interface is small:

```sh
khazad-doom run --allow-dirty --slice M11-frame-focus-mode-navigation
```

The mental model this suggests:

1. Validate the selected slice.
2. Start a worker if the run is runnable.
3. Produce an implementation, blocked result, or actionable failure.

That would be a deep module: the daemon hides orchestration, worktree, worker, and provider details.

## What the real interface requires today

The actual informal interface includes hidden requirements:

- The selected agent must be known (`pi` vs `fake`).
- The Pi binary must exist and be launchable.
- The default implementer profile must exist.
- The operator-wide implementer profile's provider/model/reasoning/mode must match an authenticated Pi setup.
- The Pi provider must be logged in before the run begins.
- The operator must understand that `failed` may mean "worker never actually ran".
- The operator must infer whether retries are useful for the failure class.
- The operator must know that `~/.khazad-doom/agents.toml` is the cross-repo provider/model setting.
- The operator must know whether `--agent fake` is valid for smoke testing but not implementation.

This makes the `run` module shallow at the operator seam: the command is short, but its informal interface is large and mostly implicit.

## Complexity findings

### 1. Provider readiness is checked too late

Current behavior discovers missing Pi/OpenAI auth inside `PiRunner::run`, after the daemon has accepted the run and entered worker attempt execution.

Impact:

- **Unknown unknown**: the operator learns about auth only after the run starts.
- **Change amplification**: improving the message may require touching runner errors, attempt handling, terminal summary rendering, docs, and monitor behavior.
- **Runtime waste**: deterministic setup failures can consume all worker attempts.
- **State pollution**: run/slice attempt artifacts imply implementation work happened even when the worker failed before reading the slice.

Design smell: environment readiness is part of the worker adapter's launch details, but run admission does not ask the adapter whether it is ready.

### 2. Retry policy does not distinguish deterministic blockers from transient failures

`MAX_WORKER_ATTEMPTS` is applied around the whole worker attempt loop. A Pi auth failure is retried like a bad worker answer, dirty worktree, missing commit, or transient process issue.

Impact:

- **Cognitive load**: the operator must decide whether three failures mean the implementation failed or the harness is misconfigured.
- **Obscurity**: repeated identical failures make the output look noisy rather than decisive.
- **Bad economics**: retries spend time without increasing success probability.

Design smell: retry policy lacks a typed failure contract from worker launch.

### 3. `failed` is overloaded

The run ended as `failed`, but the user's summary correctly says no code worker actually ran.

Current terminal categories are useful but too coarse for this case:

- `completed`
- `failed`
- `blocked`
- `cancelled`
- `interrupted`

Impact:

- **Obscurity**: `failed` can mean implementation failure, gate failure, runner launch failure, or environment misconfiguration.
- **Unknown unknown**: users may inspect worker output expecting code evidence that does not exist.
- **Poor locality**: every renderer/report must compensate with ad-hoc wording.

Design smell: terminal status and failure kind are not separated enough. The status can remain `blocked`/`failed`, but the failure classification must be structured and first-class.

### 4. Worker profile and worker adapter are split awkwardly

Operator-wide `~/.khazad-doom/agents.toml` contains provider/model/reasoning profile information applied to every repo; optional repo-local `.workflow/agents.toml` is compatibility/fallback metadata. `RunnerSpec` carries Pi binary and args. `Manager::runner_for_parts` merges them. `PiRunner::run` executes the command. The actual provider readiness requirement is not represented as a module interface.

Impact:

- **Hidden dependency**: profile config and Pi auth must agree, but no module owns that relationship.
- **Change amplification**: adding provider-specific diagnostics likely touches profile parsing, runner construction, runner execution, attempt artifacts, and CLI docs.
- **Cognitive load**: the operator sees `agent: pi`, `provider: openai`, `model: gpt-5.5`; Khazad-Doom does not expose "this worker profile is ready/not ready" as a simple fact.

Design smell: `Runner` is a launch seam, but not a readiness seam.

### 5. `preflight.json` is useful but incomplete

The run writes `preflight.json` with repo, branch, dirty status, selected slices, path, and timestamp. It does not record worker readiness because that check does not exist.

Impact:

- **Obscurity**: postmortems show the repo state but not the worker environment state.
- **User burden**: the operator must reconstruct provider/model/auth from event logs and config.

Design smell: run preflight validates source repo state but not the configured worker profile's ability to start.

### 6. Pi adapter knows process I/O, not provider semantics

`PiRunner::run` is a good process adapter in several ways: it owns command launch, stdin/stdout/stderr, JSON extraction, cancellation, and transcript capture. But missing-auth text is currently just stderr text inside a generic `RunnerError`.

Impact:

- **Information leakage**: provider-specific failure semantics leak as raw stderr.
- **Low leverage**: caller code can only see a generic error string.

Design smell: the process adapter lacks a typed launch-failure interface.

## Current deep modules worth preserving

This audit should not flatten the system or turn it into a monolith. Several modules are already deep or moving in the right direction:

- `WorkflowGate` owns verification command execution details and returns typed gate/check results.
- `artifact::Store` centralizes workflow artifact paths and JSON persistence.
- `state::Store` is the source of truth for daemon state, progress, and events.
- `PiRunner` hides process supervision and transcript parsing from workflow code.
- `Manager` owns lifecycle ordering, retries, integration, repair, terminal summaries, and cleanup.

The problem is not "too many modules" by itself. The problem is that deterministic worker-launch/environment failures have no typed classification path, so their complexity leaks into retries, status, incidents, and operator recovery.

## Normative recommendation: classify first, do not preflight first

The design target remains: make `khazad-doom run --slice ...` truthful and low-cognitive-load when the selected worker cannot launch. The first implementation should deepen existing seams rather than add new modules.

A code review of this audit found the current code already has partial versions of several proposed concepts:

- `RunnerSpec`, `AgentProfile`, and `RunnerMetadata` already carry most worker-profile information.
- `WorkerAttemptContext`, `run_recorded_agent_job`, and `run_supervised_worker_job` already form the worker-execution seam.
- `failure_kind_needs_operator` in `workflow::gate` already classifies shell/gate failures as operator-fix vs auto-fix.
- `BlockedError` already maps errors to run/slice `blocked` status.
- A blocked result in a parallel layer already short-circuits later dependency layers: `run_parallel_worker_batch` records deterministic outcomes, returns `BlockedError` if any sibling blocked, and `run_worker_layer`/`run_slices` stop before later layers launch.

Therefore the first slice should **classify the first real Pi launch failure**, not predict readiness before launch.

Rationale:

- The most authoritative way to know whether Pi can run with the selected provider/model is to let Pi answer. Reimplementing Pi auth/provider resolution in Khazad-Doom would create a brittle dependency on Pi internals.
- The existing first attempt already captures stderr and parsed assistant output in `RunnerError`/`RunnerTranscript`.
- A high-confidence auth/config launch signature can combine structural evidence (`assistant_tail` is empty: no parsed assistant message/agent event was observed) with a narrow stderr pattern (`No API key found for ...`, `/login`, or equivalent known Pi startup-auth text).
- Deterministic operator-class launch failures should become `BlockedError`, record a `run_incident` with `failure_kind`, profile/provider/model, and fix commands, and skip remaining retries.
- Unknown launch errors must keep current retry behavior. A false positive that blocks a retryable run is worse than the current false negative of burning three attempts.

## First implementation slice: classify non-retryable Pi launch failures

Scope:

- `src/agent.rs`
- `src/workflow/manager.rs`
- `tests/daemon_integration.rs`
- `docs/workflow-invariants.md`

Likely no schema change is needed. Reuse existing `RunStatus::Blocked`, `SliceStatus::Blocked`, `BlockedError`, `run_incident`, and the existing failure-kind vocabulary/predicate style.

Acceptance:

1. A Pi launch failure is classified as operator-action-required only when it has a narrow known signature, e.g. empty `RunnerTranscript.assistant_tail` plus Pi auth/config stderr such as `No API key found for openai` / `/login`.
2. Classified Pi auth failures use a failure kind such as `agent_auth_required` and are marked non-retryable.
3. The affected slice/run becomes `blocked`, not generic `failed`.
4. The worker attempt loop does not consume attempts 2 and 3 for non-retryable operator-class launch failures.
5. Unknown or ambiguous launch errors fall back to current retry behavior unchanged.
6. A `run_incident` or terminal summary includes provider/model/profile and concrete fix guidance (`pi /login` or update `~/.khazad-doom/agents.toml`).
7. The same classification path is usable by integration repair agent calls, so the auth failure does not merely move to a later phase.
8. A blocked slice in an earlier dependency layer prevents later layers from launching; in the same parallel layer, siblings may each make one doomed launch before the layer records outcomes and blocks.
9. The fake worker path remains exempt and the deterministic fake end-to-end smoke path still completes.

Suggested tests:

- Unit test: classifier recognizes empty assistant output plus Pi missing-auth stderr as `agent_auth_required`, `retryable = false`, `operator_action_required = true`.
- Unit test: stderr text without the structural no-assistant signal, or unknown stderr with empty assistant output, does **not** become non-retryable.
- Integration test: fake `pi` exits immediately with missing-auth stderr; run ends `blocked`, the slice has exactly one attempt, and an actionable incident/summary is present.
- Integration test: a blocked slice in one dependency layer prevents a dependent later-layer slice from dispatching.
- Integration test or existing smoke: `--agent fake` still completes.

Non-goals:

- Do not add new run/slice statuses.
- Do not redesign the slice schema.
- Do not add multi-agent planning.
- Do not make fake worker implement real features.
- Do not auto-login or mutate provider credentials.
- Do not add a broad worker-readiness preflight module unless later evidence requires it.

## Considered alternatives and evidence-gated deferrals

The earlier version of this audit proposed `WorkerReadiness`, `WorkerProfile`, `WorkerExecution`, and `FailureClassifier` as new or first-class modules. Those are now **considered alternatives**, not active recommendations.

### Alternative: readiness preflight before durable run creation

Potential benefit: fail before creating run state for obvious setup mistakes.

Why deferred: a faithful preflight would either duplicate Pi provider/auth logic or spend an extra real Pi invocation. Classify-on-first-failure gives the same operator outcome with less coupling and better runtime economy.

Reconsider only if Pi exposes a cheap, no-agent, stable readiness/auth command that Khazad-Doom can call once per run without duplicating provider internals.

### Alternative: first-class `WorkerProfile` module

Potential benefit: centralize config precedence, launch args, profile metadata, and display-safe summaries.

Why deferred: `RunnerSpec`, `AgentProfile`, and `RunnerMetadata` already cover most of this. Deepen those existing types first.

Reconsider when adding a second real provider/worker adapter or when profile/config changes require repeated edits across caller code.

### Alternative: broad terminal `FailureClassifier` module

Potential benefit: one module converts failures to status, retry behavior, incidents, terminal summaries, and next commands.

Why deferred: gate failures already have `failure_kind_needs_operator`, and run status already maps `BlockedError`/`CancelledError`. Slice 1 may only need a small runner-launch classification helper and incident writer.

Reconsider if multiple independent failure domains start duplicating classification or rendering logic.

## Strategic conclusion

Khazad-Doom's core idea remains strong: bounded slices, daemon-owned workflow, isolated worktrees, explicit verification, and observable handoff. The current pain is a missing classification path for deterministic worker-launch/environment failures.

The immediate design target is:

> Make deterministic Pi provider/auth launch failures become one-attempt, operator-actionable `blocked` results with narrow classification and safe fallback to existing retries.

That is a high-leverage simplification: it reduces user cognitive load, avoids wasted retries, improves status truthfulness, and preserves locality by deepening existing runner/failure seams before introducing new modules.
