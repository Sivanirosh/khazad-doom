# Pi TUI worker lifecycle proof — 2026-07-08

## Scope

This note records the native-Pi-TUI lifecycle proof step after the result-channel smoke proof. It was originally introduced as an opt-in experimental path; after the later timeout, invalid-result retry, targeted-repair, and four-worker proofs, native Herdr-hosted Pi TUI workers are the default when cockpit placement is available. Khazad-Doom-owned artifacts remain the only truth.

## Mechanism proven in code

- Native Pi TUI workers are the default when a real Pi command spec is available and cockpit mode is not direct.
- `--experimental-pi-tui-worker` remains as a deprecated compatibility flag; `--json-wrapper-worker`, `KHAZAD_JSON_WRAPPER_WORKER=1`, or `KHAZAD_DISABLE_PI_TUI_WORKER=1` select the legacy wrapper path. Run intent is interpreted by the CLI and sent to the daemon as an explicit run parameter, not read from ambient daemon process state.
- The daemon prepares per-attempt TUI artifacts next to the normal worker output:
  - prompt markdown
  - launch command JSON
  - result artifact path
  - a copied `khazad-worker` Pi extension
- The TUI launch uses `herdr agent start ... -- <pi argv>` through the existing cockpit adapter, not terminal text injection.
- The Pi argv strips JSON print-mode flags (`--mode json --no-session`) and loads the per-attempt worker extension explicitly with `--no-extensions --extension <artifact extension dir>`.
- The daemon waits only for the `submit_worker_result` artifact source `khazad_worker_submit_worker_result_v1`.
- On parent cancellation or worker-attempt timeout, the supervised job cancellation token is tripped and the TUI wait loop asks Herdr to close the worker pane. Terminal contents remain non-authoritative.
- If Herdr/TUI launch is unavailable, the daemon records a cockpit worker fallback and runs the existing direct Pi wrapper path.

## Lifecycle integration retained

The native TUI path is inside `run_recorded_slice_worker_job` and returns the same `ResultData` shape as the wrapper runner. That means the existing daemon-owned lifecycle remains downstream of the result artifact:

- worker result schema parsing and validation
- scope checks against authorized paths
- lightweight verification gates
- retry and bounded repair budgets
- per-slice commit/merge flow
- integration gate and handoff reporting
- economics/attempt accounting

No result is accepted from Herdr pane text, scrollback, agent metadata, Pi TUI display state, or extension UI state.

## Packaging policy

`extensions/khazad-worker` remains **not** listed in `package.json` `pi.extensions`. It is not a global/operator extension. For the native worker path, Khazad-Doom copies the worker extension source into the per-attempt artifact directory and passes it explicitly to Pi for that one launch.

Rationale:

- worker tools are privileged because `submit_worker_result` terminates a worker and writes authoritative result artifacts;
- loading is tied to a daemon-owned result path and slice/run environment;
- default Pi sessions should not receive worker result tools accidentally;
- the path is packageable because the Rust binary embeds the extension source at compile time.

## Verification run in this session

```bash
cargo check -q
cargo test -q experimental_tui_worker_flag_is_recorded_in_run_preflight
cargo test -q resume_after_completion_publication_is_idempotent
cargo test -q tui_worker
cargo test -q worker_attempt_failure_sequence_uses_envelope_retry_and_targeted_repair
node --check extensions/khazad-worker/index.js
node --test extensions/khazad-worker/index.test.mjs
npm run test:extension
python -m json.tool .workflow/slices/TUI-PROOF-02.json >/dev/null
bash -n scripts/proof-pi-tui-worker
scripts/proof-pi-tui-worker --dry-run
```

The earlier result-channel proof remains recorded in `docs/design/evidence/pi-tui-worker-proof-2026-07-08.md`.

## Live daemon dogfood — first attempt caught old behavior

Run `kd-20260708-004220-7eb38dc2` was started with only `KHAZAD_EXPERIMENTAL_PI_TUI_WORKER=1` on the client command. It still opened the wrapper painter pane:

```json
{
  "type": "cockpit_worker_ready",
  "payload": {
    "pane": "Worker kd-20260708-004220-7eb38dc2/TUI-DOGFOOD-01 attempt 1",
    "source_of_truth": "kd_artifact_files"
  }
}
```

This was the old behavior. The run was cancelled as failed proof evidence. Root cause: the first implementation read the opt-in from ambient daemon process environment. That is not a reliable run contract; an already-running daemon may not have the client environment, and the operator's intent must travel through daemon IPC/run state.

Fix: add an explicit `--experimental-pi-tui-worker` CLI flag and an explicit IPC/run worker-interface option. After default promotion, the internal option is named `native_pi_tui_worker`; the daemon still accepts the historical `experimental_pi_tui_worker` JSON field as an alias and still records it in preflight artifacts for compatibility. The CLI still maps `KHAZAD_EXPERIMENTAL_PI_TUI_WORKER=1` to native TUI selection for convenience, but the daemon no longer has to infer operator intent from its process environment.

## Live daemon dogfood — native TUI path succeeded

Run command:

```bash
cargo run --quiet -- run --allow-dirty --cockpit herdr --experimental-pi-tui-worker --slice TUI-DOGFOOD-01
```

Result:

```text
run_id: kd-20260708-005324-047ab49d
status: completed
integration branch: khazad/kd-20260708-005324-047ab49d/integration
final sha: 31d5a6b98a679fcaf157c1ff98e7b3628333ecf4
```

Authoritative TUI launch evidence:

```json
{
  "type": "cockpit_worker_ready",
  "payload": {
    "pane": "kd-tui-kd-20260708-005324-047ab49d-TUI-DOGFOOD-01-attempt-1",
    "pane_id": "w3N:p4",
    "source_of_truth": "kd_tui_result_artifact"
  }
}
```

Authoritative result artifact:

```text
.workflow/runs/kd-20260708-005324-047ab49d/outputs/TUI-DOGFOOD-01.worker.attempt-1.herdr-tui.result.json
```

The artifact has `source: "khazad_worker_submit_worker_result_v1"`, `run_id: "kd-20260708-005324-047ab49d"`, `slice_id: "TUI-DOGFOOD-01"`, `attempt: 1`, and a complete worker result. The daemon copied this to the normal worker output path, ran slice verification, merged the worker branch, ran the integration gate, and published completion artifacts.

Branch evidence:

```text
31d5a6b khazad(run): publish completion kd-20260708-005324-047ab49d
a187782 khazad(slice:TUI-DOGFOOD-01): merge Native Pi TUI daemon dogfood proof
ab91bb0 Add Pi TUI worker dogfood evidence note
```

The worker-created dogfood note exists on the integration branch and states that the authoritative proof is the `submit_worker_result` artifact, not terminal text.

## Still not a production replacement

Before defaulting or replacing the wrapper path, still prove:

- real Herdr-hosted Pi TUI cancellation from a KD daemon run;
- real worker-attempt timeout closing the Herdr-hosted Pi TUI;
- retry/repair behavior on an intentionally failing or invalid TUI result;
- release packaging review for the embedded per-attempt worker extension.
