# Khazad-Doom

> You shall not slop.

Khazad-Doom is a local agentic workflow daemon for turning GitHub-first, JSON-authoritative Issue Slices into isolated agent runs, integrated branches, gated reports, and PR handoffs.

Current production slice set:

1. initialize a repo-local `.workflow/` contract area;
2. read and validate schema-valid JSON Issue Slices;
3. start a per-user daemon under `~/.khazad-doom`;
4. create isolated git worktrees and branches;
5. dispatch injectable Pi/fake runner workers for deterministic execution and tests;
6. require worker commits and structured JSON results;
7. run lightweight per-slice verification;
8. schedule multiple slices serially in dependency order;
9. recover interrupted daemon runs safely after restart;
10. select runner via CLI/env (`pi` or deterministic `fake`);
11. produce branch/PR handoff JSON;
12. cover daemon behavior with black-box integration tests;
13. inspect run artifacts and daemon log tail;
14. build, install, package, and validate through CI scripts.

## Architecture

- **Global runtime state:** `~/.khazad-doom/` — daemon socket, SQLite state, logs, daemon-managed worktrees.
- **Repo-local durable contracts:** `.workflow/slices/*.json`, `.workflow/plans/`, `.workflow/reports/`.
- **Repo-local transient artifacts:** `.workflow/runs/` — handoffs and raw JSON outputs, gitignored.
- **Implementation language:** Rust.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- --help
```

## Installation

```sh
cargo install --path .
# or
PREFIX="$HOME/.local" scripts/install.sh
```

Create a local release tarball:

```sh
scripts/package.sh
```

## Commands

```sh
khazad-doom init
khazad-doom slices validate
khazad-doom slices list
khazad-doom daemon start
khazad-doom run --slice slice-008 --wait   # includes dependencies
khazad-doom run --all --wait               # all slices in dependency order
khazad-doom run --agent fake --all --wait  # deterministic local test runner
khazad-doom status
khazad-doom status --run <run-id>
khazad-doom cancel --run <run-id> --reason "operator requested"
khazad-doom handoff --run <run-id>
khazad-doom inspect --run <run-id> --log-tail 50
khazad-doom daemon stop
```

During development you can run the CLI through Cargo:

```sh
cargo run -- init
cargo run -- slices validate
cargo run -- run --agent fake --all --wait
cargo run -- status --run <run-id>
cargo run -- handoff --run <run-id>
cargo run -- inspect --run <run-id>
cargo run -- daemon stop
```

## Runner selection

Default runner is `pi`. Override per run or through environment:

```sh
khazad-doom run --agent pi --slice slice-001
khazad-doom run --agent fake --all
KHAZAD_AGENT=fake khazad-doom run --all
KHAZAD_PI_BIN=/path/to/pi KHAZAD_PI_ARGS="--some-arg" khazad-doom run --agent pi --all
```

The `fake` runner is opt-in and deterministic. It creates committed fixture files for each slice and returns JSON-only worker results, making daemon/integration tests possible without invoking Pi.

## Daemon recovery

If the daemon exits while runs are still `pending` or `running`, the next daemon start marks those runs `interrupted`, marks active slice runs `interrupted`, records recovery events, and attempts best-effort cleanup of daemon-managed worktrees. Khazad-Doom does not pretend to resume a lost Pi process.

## Reports, handoff, and inspection

- Worker handoffs and raw outputs are written under `.workflow/runs/<run-id>/` in the source repo.
- `implementation-summary.json` and `final-report.json` are written under `.workflow/runs/<run-id>/outputs/`.
- The integration branch also receives `.workflow/reports/<run-id>-implementation-summary.json` and `.workflow/reports/<run-id>-final-report.json` commits.
- `handoff --run <run-id>` prints JSON containing the integration branch, final SHA, report paths, and suggested `git push`/`gh pr create` commands. It does not push or open a PR.
- `inspect --run <run-id>` lists run-scoped artifacts and optionally includes a bounded daemon log tail.
- Daemon-managed worktrees under `~/.khazad-doom/worktrees/` are removed when a run reaches a terminal state.

## Minimal slice

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
  "verify": ["cargo test"]
}
```

GitHub issues carry rich human discussion. JSON slices are the daemon's compact machine source of truth.
