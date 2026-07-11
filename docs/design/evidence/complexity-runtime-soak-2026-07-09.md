# CA-07 bounded-runtime and CA-09 closure soak evidence

Date measured: 2026-07-11

Baseline policy: CA-06 fixed polling, per-observation writes, no raw spill

Final candidate: CA-09 working tree after CA-07/CA-08 runtime and contract changes

Command: `scripts/soak-runtime --quick --output /tmp/ca09-final-soak.json`

## Method

The script reproducibly runs baseline-policy and final-policy profiles for 1, 3, and 10 concurrent workers. Both profiles execute the **same real `PiRunner` child path and deterministic workload**: each worker emits 1 MiB of stderr, remains quiet for 100 ms, and exits. Only the runtime policy differs.

The baseline profile disables ordinary-observation batching, uses fixed 100 ms polling, and disables raw spill—the CA-06 behavior under measurement. The final profile enables byte/time coalescing, adaptive polling, bounded capture, and append-only raw spill. Keeping the worker path identical makes wall time, RSS, SQLite, poll, and artifact comparisons reproducible from one command rather than relying on unpublished historical instrumentation.

Measurements are process-local wall time and Linux `VmHWM`; SQLite bytes include the database and WAL. SQLite rows count all non-internal tables after the workload. `state writes` counts attempted daemon activity updates, not filesystem syscalls. Command count is deliberately zero for this worker-only workload; agent calls equal worker count. Multi-megabyte verification-command capture is covered separately by `bounded_gate_output_reports_retention_and_append_only_spill_metadata`.

## Baseline policy

| Workers | Wall ms | Peak RSS KiB | SQLite bytes | SQLite rows | State writes | Artifact bytes | Polls | Commands | Agent calls |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 204 | 15,192 | 172,032 | 2 | 132 | 0 | 3 | 0 | 1 |
| 3 | 258 | 17,492 | 172,032 | 2 | 396 | 0 | 9 | 0 | 3 |
| 10 | 1,211 | 22,284 | 172,032 | 2 | 1,330 | 0 | 111 | 0 | 10 |

Rows and allocated SQLite bytes remain flat because ordinary observations update one current progress projection. The write counter exposes amplification that file/row size cannot. Baseline artifact bytes are zero because complete raw worker output was not retained as explicit attempt evidence.

## Final policy

| Workers | Wall ms | Peak RSS KiB | SQLite bytes | SQLite rows | State writes | Artifact bytes | Polls | Commands | Agent calls |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 166 | 15,496 | 172,032 | 2 | 67 | 1,050,044 | 4 | 0 | 1 |
| 3 | 224 | 17,216 | 172,032 | 2 | 204 | 3,150,132 | 12 | 0 | 3 |
| 10 | 777 | 21,868 | 172,032 | 2 | 680 | 10,500,440 | 90 | 0 | 10 |

Artifact growth is intentional and approximately linear with complete raw output. The small excess over payload bytes is bounded metadata. In-memory diagnostic tails remain limited independently of artifact size. Total final observation bytes include generated line delimiters.

The short workload does not reach the default 500 ms adaptive-poll ceiling; final poll counts therefore remain close to baseline. The dedicated quiet-worker regression exercises exponential backoff over a longer quiet interval and verifies configured maximum cancellation latency.

## Chosen bounds

Repository defaults and hard maxima are validated when `.workflow/khazad.json` is read:

- retained diagnostic output: 64 KiB and 1,000 lines per stream; hard maxima 4 MiB and 100,000 lines;
- ordinary observation flush: 16 KiB or 250 ms; hard maxima 1 MiB and 5 s;
- adaptive polling: 25 ms initial, 500 ms maximum; hard maximum 5 s;
- economics checkpoint: 500 ms; hard maximum 60 s;
- process termination grace: 30 s; hard maximum 300 s;
- raw output spill: enabled; zero retained bytes *or* lines requires spill.

Authoritative assistant result text is not truncated to the diagnostic-tail limit. Regressions preserve valid direct and wrapper results larger than 64 KiB while keeping transcript/observation tails bounded. Pi lines spool beyond 64 KiB and are parsed from the spool only at a delimiter/EOF, so multi-megabyte delimiter-free malformed output does not accumulate in parser memory; wrapper file polling and direct stderr are likewise chunked with bounded pending state. A valid authoritative result may allocate its domain value when decoded rather than being silently dropped.

The quick final profile lowers poll backoff to 10/100 ms so it completes quickly while exercising the same policy.

## Conclusion

At 10 workers in the CA-09 closure rerun, activity updates fell from 1,330 to 680 (1.96x fewer), wall time fell from 1,211 ms to 777 ms, and polls fell from 111 to 90 while complete raw bytes became append-only artifacts. Peak RSS was about 0.4 MiB lower in the final profile for this run; process-local RSS and timing remain observational rather than hard performance promises.

The evidence supports byte/time coalescing, adaptive polling, bounded diagnostics, append-only spill, spooled Pi framing, and checkpointed economics. It does **not** justify a database pool, external telemetry service, or pub/sub broker.
