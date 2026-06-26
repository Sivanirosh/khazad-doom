<p align="center">
  <img src="assets/khazad-doom-pixel.svg" width="760" alt="Pixel art parody: a JSON wizard says You shall not slop to a fiery slop daemon on a bridge">
</p>

<h1 align="center">Khazad-Doom</h1>

<p align="center">
  <em>You shall not slop.</em>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-2024-111111?style=flat-square" alt="Rust 2024">
  <img src="https://img.shields.io/badge/workflow-JSON%20Issue%20Slices-111111?style=flat-square" alt="JSON Issue Slices">
  <img src="https://img.shields.io/badge/daemon-local-111111?style=flat-square" alt="Local daemon">
  <img src="https://img.shields.io/badge/license-MIT-111111?style=flat-square" alt="MIT license">
</p>

---

Agent work goes downhill quietly: one vague instruction becomes six unrelated edits, a dirty worktree, and a victory paragraph where proof should be.

Khazad-Doom is the bridge guard for that moment: **You shall not slop!**

It makes the contract explicit before the agent starts. A **JSON Issue Slice** says what is authorized, what must be verified, and when the worker must stop and ask. The daemon runs that slice in an isolated worktree, demands a commit and JSON result, gates integration, and leaves a PR-ready handoff instead of vibes.

## The idea

A normal agent prompt is soft. Khazad-Doom makes it hard:

```text
JSON Issue Slice
      ↓
validated dependency graph
      ↓
one isolated worktree per slice
      ↓
JSON-only worker result + committed branch
      ↓
verification + integration repair + gate
      ↓
final report + handoff commands
```

The agent writes code. The daemon decides what counts as done.

## Before / after

Without Khazad-Doom:

```text
"Implement this issue"
      ↓
chat transcript
      ↓
unclear scope, unclear status, unclear proof
```

With Khazad-Doom:

```text
khazad-doom run --slice slice-001 --wait
      ↓
clean branch, structured result, verification output, final report
```

No vague victory laps. No hidden scope creep. No unreviewable agent sludge.

## Issue Slices

An Issue Slice is the smallest unit of work Khazad-Doom will hand to an agent. It is narrower than a GitHub issue and stricter than a prompt.

```json
{
  "id": "slice-001",
  "title": "Add retry policy",
  "goal": "Add bounded retry behavior for transient job failures.",
  "github_issue": "https://github.com/org/repo/issues/123",
  "depends_on": [],
  "areas": ["internal/jobs", "tests/jobs"],
  "acceptance": [
    "Transient failures retry up to 3 times.",
    "Permanent failures are not retried.",
    "Existing idempotency behavior is preserved."
  ],
  "must_ask_if": [
    "Public retry config shape must change.",
    "Auth/session behavior changes.",
    "Acceptance criteria conflict."
  ],
  "verify_profile": "quick",
  "verify": ["cargo test"],
  "verify_timeout_seconds": 600
}
```

The JSON wins over chat. `must_ask_if` is the line where the worker must stop and ask instead of guessing.

## What Khazad-Doom guarantees

- **Bounded work** — each worker receives exactly one slice.
- **Isolation** — each slice runs in its own git worktree and branch.
- **Structured output** — worker and repair results must be JSON.
- **Committed handoff** — completed slice work must be committed with a clean worktree.
- **Verification** — slice `verify` commands run before merge and again through the integration gate.
- **Dependency order** — requested slices automatically include their dependencies.
- **Parallel workers** — independent slices can run concurrently, then merge serially.
- **Repo/run locking** — one active run per repository prevents branch/worktree collisions.
- **Config defaults** — `.workflow/khazad.json` can set agent, parallelism, base branch, handoff, and verification profiles.
- **Timeout policy** — slice and profile verification commands have bounded runtime.
- **Durable checkpoints** — completed merges write checkpoints; `resume` continues remaining work explicitly.
- **Conflict artifacts** — merge conflicts become structured blocked reports instead of half-merged chaos.
- **No surprise PRs** — handoff prints commands by default and only pushes/creates PRs with explicit flags.

## Install

From a checkout:

```bash
cargo install --path .
```

Or install into a local prefix:

```bash
PREFIX="$HOME/.local" scripts/install.sh
```

Package a local release tarball:

```bash
scripts/package.sh
```

## Quick start

Inside a git repository:

```bash
khazad-doom init
khazad-doom slices new --id slice-001 --title "Add retry policy" --goal "Add bounded retries" --verify "cargo test"
khazad-doom slices validate
khazad-doom slices schema --write
khazad-doom run --slice slice-001 --wait
khazad-doom status --run <run-id>
khazad-doom handoff --run <run-id>
```

For a deterministic smoke test that does not invoke Pi:

```bash
khazad-doom run --agent fake --all --wait
```

## Commands

| Command | What it does |
|---|---|
| `khazad-doom init` | Create `.workflow/` and register the repo. |
| `khazad-doom slices validate` | Validate slice JSON, IDs, dependencies, and cycles. |
| `khazad-doom slices list` | Print compact slice summaries. |
| `khazad-doom slices new ...` | Generate a JSON Issue Slice template. |
| `khazad-doom slices import-github --issue <url>` | Import a GitHub issue via `gh issue view`. |
| `khazad-doom slices import-github --issue <url> --dry-run` | Preview generated slice JSON without writing. |
| `khazad-doom slices schema --write` | Print/write the JSON Schema for slice files. |
| `khazad-doom run --slice <id>` | Run one slice plus its dependencies. |
| `khazad-doom run --all --parallel <n>` | Run all slices; independent workers may run concurrently. |
| `khazad-doom resume --run <id>` | Continue an interrupted/failed/cancelled run from checkpoint. |
| `khazad-doom status` | Show recent runs. |
| `khazad-doom status --run <id>` | Show one run, slice states, and events. |
| `khazad-doom cancel --run <id>` | Request cancellation. |
| `khazad-doom handoff --run <id>` | Print push/PR handoff JSON for a completed run. |
| `khazad-doom handoff --run <id> --dry-run` | Suppress configured push/PR defaults and print diagnostics only. |
| `khazad-doom handoff --run <id> --push --create-pr` | Explicitly push and open a PR with `gh`. |
| `khazad-doom inspect --run <id>` | List run artifacts and a bounded daemon log tail. |
| `khazad-doom daemon start` | Start the local daemon. |
| `khazad-doom daemon stop` | Stop the daemon when no runs are active. |

## Runners

Default runner: `pi`.

```bash
khazad-doom run --agent pi --slice slice-001
khazad-doom run --agent fake --all
KHAZAD_AGENT=fake khazad-doom run --all
KHAZAD_PI_BIN=/path/to/pi KHAZAD_PI_ARGS="--some-arg" khazad-doom run --agent pi --all
```

`fake` is deliberately boring: it commits predictable fixture files and returns valid worker JSON. Use it for daemon tests, demos, and dogfooding the workflow itself.

## Repository config

`khazad-doom init` creates `.workflow/khazad.json`. Commit it when you want shared defaults:

```json
{
  "agent": "pi",
  "parallelism": 1,
  "verify_timeout_seconds": 600,
  "handoff": { "push": false, "create_pr": false },
  "verify_profiles": {
    "quick": {
      "commands": [
        { "command": "cargo fmt --check", "timeout_seconds": 120 },
        { "command": "cargo test", "timeout_seconds": 240 }
      ]
    }
  }
}
```

A slice can reference `"verify_profile": "quick"` and still add inline `verify` commands. Profile commands support repo-relative `cwd`, `env`, and per-command timeouts.

## Files and state

| Path | Purpose |
|---|---|
| `.workflow/khazad.json` | Shared repo defaults and verification profiles. |
| `.workflow/slices/*.json` | Durable machine-readable Issue Slices. |
| `.workflow/schema/slice.schema.json` | JSON Schema for editor/CI validation. |
| `.workflow/plans/` | Optional planning artifacts. |
| `.workflow/reports/` | Reports committed to integration branches. |
| `.workflow/runs/` | Transient handoffs and raw outputs; gitignored. |
| `~/.khazad-doom/socket` | Daemon IPC socket. |
| `~/.khazad-doom/state.sqlite` | Run, slice, and event state. |
| `~/.khazad-doom/worktrees/` | Daemon-managed temporary worktrees. |

If the daemon starts and finds active runs from a previous process, it marks them `interrupted`, records recovery events, and cleans daemon worktrees where possible. `khazad-doom resume --run <id>` is explicit: it reuses the integration branch and checkpoint state for remaining slices. It does not pretend a crashed worker survived.

## Handoff

`khazad-doom handoff --run <run-id>` prints JSON containing:

- integration branch
- base branch and base SHA
- final SHA
- summary and report paths
- suggested `git push` command
- suggested `gh pr create` command

By default it does not push and does not open a PR. Add `--push --create-pr` when you want Khazad-Doom to run those commands explicitly. Use `--dry-run` to inspect commands and diagnostics even if repo config enables default handoff actions.

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
bash -n scripts/install.sh scripts/package.sh
```

Run the daemon path through the fake runner:

```bash
cargo test --test daemon_integration
```

Create a release by pushing a `v*` tag. CI builds the package tarball, writes `SHA256SUMS`, and attaches both to the GitHub release.

## FAQ

**Is Khazad-Doom an agent?**
No. It is the foreman. It gives agents bounded work, checks the result, and records evidence.

**Why JSON?**
Because prose is where scope creep hides. JSON is compact, diffable, validatable, and explicit.

**Can it resume a crashed worker?**
No. A lost worker becomes an `interrupted` slice. `resume` continues remaining slices from durable checkpoints; it does not resurrect dead processes.

**Does `handoff` create a PR?**
Only with `--create-pr`; use `--push` when the integration branch also needs to be pushed. You stay in control.

**Why the name?**
Because something has to stand on the bridge.

## License

MIT.
