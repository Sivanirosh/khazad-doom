# Native Pi TUI worker packaging policy proof — 2026-07-08

## Scope and evidence boundary

This proof covers packaging/release policy for the native Pi TUI worker extension. Default promotion is justified by the separate timeout, invalid-result retry, targeted-repair, and four-worker lifecycle proofs; this document only proves the extension remains packaged as a per-attempt artifact, not globally registered.

## Policy checked

`package.json` registers only the monitor extension globally:

```json
"pi": {
  "skills": ["./skills"],
  "extensions": ["./extensions/khazad-monitor"]
}
```

`./extensions/khazad-worker` is intentionally not listed in `pi.extensions`. The worker extension is prepared per-attempt as a KD-owned artifact and passed to Pi with `--no-extensions --extension <attempt-extension-dir>`.

## Per-attempt artifact examples

Successful native TUI dogfood run:

```text
kd-20260708-021402-0ac6eb09
```

Per-attempt worker extension artifact:

```text
.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.extension/index.js
```

Per-attempt command artifact:

```text
.workflow/runs/kd-20260708-021402-0ac6eb09/outputs/LAYOUT-05.worker.attempt-1.herdr-tui.command.json
```

That command artifact records:

```json
{
  "contract": "khazad-owned-herdr-pi-tui-worker-v1",
  "result_source": "khazad_worker_submit_worker_result_v1"
}
```

and the argv includes `--no-extensions --extension .../.herdr-tui.extension ...`, proving per-attempt loading rather than global registration.

The post-anchor multi-worker proof run also generated per-attempt extension directories, for example:

```text
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02A.worker.attempt-1.herdr-tui.extension/index.js
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02B.worker.attempt-1.herdr-tui.extension/index.js
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02C.worker.attempt-1.herdr-tui.extension/index.js
.workflow/runs/kd-20260708-082104-2724f3e9/outputs/TUI-MULTI-02D.worker.attempt-1.herdr-tui.extension/index.js
```

## Release checks run

Commands run and passed:

```bash
npm pack --dry-run --json
cargo package --list
npm run check:extension
npm run test:extension
cargo test -q tui_worker
```

Captured local outputs were stored under `/tmp/khazad-proof-checks/` during this session:

```text
/tmp/khazad-proof-checks/npm-pack-dry-run.json
/tmp/khazad-proof-checks/cargo-package-list.txt
/tmp/khazad-proof-checks/npm-check-extension.txt
/tmp/khazad-proof-checks/npm-test-extension.txt
/tmp/khazad-proof-checks/cargo-test-tui-worker.txt
```

`npm pack --dry-run --json` listed both `extensions/khazad-monitor/index.js` and `extensions/khazad-worker/index.js` in the package payload. `cargo package --list` listed `extensions/khazad-worker/index.js` and `extensions/khazad-worker/index.test.mjs`. Extension syntax/tests passed, and `cargo test -q tui_worker` passed.

## What this proves

- The worker extension is shipped in releasable artifacts.
- The worker extension is not globally registered as a user-visible Pi extension in `package.json`.
- KD prepares and loads the worker extension per-attempt.
- `submit_worker_result` remains the source marker: `khazad_worker_submit_worker_result_v1`.

## What this does not prove

- It does not by itself prove native TUI timeout, cancellation, retry, repair, or multi-worker lifecycle readiness; those are separate daemon proof runs.
- It does not make Herdr or the Pi UI extension authoritative for correctness.
