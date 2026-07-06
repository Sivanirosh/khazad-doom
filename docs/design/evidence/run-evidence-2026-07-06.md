# Phase -1 run evidence harvest

Date harvested: 2026-07-06
Harvest scope: REVISION_PLAN.md Phase -1 — preserve transient run evidence before it disappears.

Sources, with evidence grades per REVISION_PLAN.md Phase 0 (A raw artifact, B written audit, C commit reconstruction, D recollection):

- surviving `.workflow/runs/` directories in this repo (grade A; gitignored, transient);
- committed `.workflow/reports/` summaries (grade A);
- the daemon state store `~/.khazad-doom/state.sqlite` — tables `runs`, `slice_runs`, `repos`, `events` (grade A; live daemon state, snapshot CSVs in `raw/`);
- the unmerged run 8 integration branch (grade A; branch is a cleanup candidate, contents extracted to `raw/`);
- `docs/design/worker-run-complexity-audit.md` (grade B);
- git history after `2a6fc7c` (grade C where used).

Raw artifacts preserved verbatim under `docs/design/evidence/raw/`.

Snapshot provenance: the `state-*-2026-07-06.{csv,txt}` files were exported on 2026-07-06 from `~/.khazad-doom/state.sqlite` (tables `runs`, `slice_runs`, `repos`, `events`) while the daemon was running; they are point-in-time snapshots, not the live store. All other `raw/` files are byte-for-byte copies from `.workflow/runs/` in this repo or extractions from the named git objects.

Privacy split (this repository is public on GitHub): the `raw/state-*` snapshots and `local/` lookup contain other projects' paths and slice identifiers and are **gitignored, local-only, grade-A** evidence. This committed summary is the public record; it refers to the other repositories as repo-B..repo-E and truncates their slice IDs to family prefix + number. The mapping lives in the operator-local `docs/design/evidence/local/pseudonym-lookup.md`.

## Correction to the keystone framing

The state store records **32 runs across 5 repositories with runs (9 registered repositories total; the 4 `/tmp` smoke/init repos registered 2026-07-05 have no run rows)**, spanning 2026-06-26 → 2026-07-05 — not just the 8 runs visible in this repo. The REVISION_PLAN keystone claim stands for this repo — no Khazad-Doom-produced commit after `2a6fc7c` (2026-06-26) — but the tool did not fall out of use. It ran almost daily against four other repositories (repo-B..repo-E; see privacy split above) through 2026-07-05.

So the true keystone statement is sharper: **the operator kept using Khazad-Doom on other projects while routing its own development around it.** The cross-repo runs are the richest failure evidence we have and are grade A. Phase 0 should widen its scope note accordingly (the current ledger scope names only this repo's runs/commits).

## The eight Khazad-Doom self-runs (2026-06-26)

Terminal states below are authoritative from the state store (`raw/state-runs-2026-07-06.csv`, `raw/state-slice-runs-2026-07-06.csv`).

### R1 — kd-20260626-101442-f66663f7 · slice-031 · **failed**

- Worker attempts: 3 (state store records 2 in `slice_runs.attempts`; 3 worker/check artifact pairs survive — discrepancy noted). All three worker attempts returned `status: complete` with commits and clean worktrees.
- All three daemon-side checks failed identically: `verify command failed: cargo fmt --check / sh: 1: cargo: not found` — the daemon verify shell lacked cargo on PATH while the worker shell had it. Deterministic environment failure consumed every attempt. Same class as the later Pi auth failures.
- **Fence violation observed:** in attempt 2 the worker repaired the verify failure by editing `.workflow/khazad.json` to prepend `$HOME/.cargo/bin` to the verification commands — a worker mutating verification policy (finding id `cargo-path`, `raw/kd-20260626-101442.slice-031.worker.attempt-2.json`). The check harness misclassified the environment failure as `action: "auto-fix"` (worker-fixable).
- No final report, no checkpoint, no run-summary (run-summary invariant postdates this run). Artifacts: `raw/kd-20260626-101442.slice-031.{worker,check}.attempt-{1,2,3}.json`.

### R2 — kd-20260626-102840-44c20dda · slice-031 · **completed**

- 1 attempt, check passed (PATH-prefixed verify commands now in effect; operator hand-fix `827295e` is the run's base). Gate passed, repair no-op. Report committed (`.workflow/reports/`). Merged as `5a76193`; slice JSON closed with `closed_by_run` = this run.

### R3 — kd-20260626-121011-06ed0834 · slice-032 · **completed**

- 1 attempt, gate passed, repair no-op. Report committed. Merged as `190f3a9`.

### R4 — kd-20260626-125734-111aa82a · slices 033–036 · **completed**

- 4 slices, 1 attempt each, gate passed. Report committed.
- **Integration repair made a real semantic fix** (`repair.status: "fixed"`, commit `338630d`): `run --repo <subdir>` emitted a `monitor_command` scoped to the subdirectory while the daemon stored runs under the git root, so `status/monitor --repo <subdir> --latest` returned null. Repair changed `src/cli.rs` — a file outside the four slices' declared areas. Evidence both that repair works and that repair authority exceeds slice fences.

### R5 — kd-20260626-183452-51f144a0 · slices 033–037 · **completed** (fake runner)

- A fake-runner smoke run that **re-executed slices 033–036 already integrated by R4** (close records were not yet written/enforced; the manual backfill came later at 19:45). All 5 "merged" in state store; gate passed on fake implementations.
- Its final report (`raw/kd-20260626-183452.final-report.json`) claims slices 033–037 `complete` and **records nothing identifying the runner as fake** — no agent/profile field exists in the report schema. A reader cannot distinguish this from a real run (PI-03's "identical profile summary everywhere" targets exactly this; `run_started` events do carry agent metadata).
- Report never promoted to `.workflow/reports/`; integration branch still exists.

### R6 — kd-20260626-184008-a6301dab · slices 033–039 · **cancelled**

- Cancelled 11 seconds after start (`run cancelled`, 0 attempts on all 7 slices). Only artifact: the slice-033 handoff (`raw/kd-20260626-184008.handoff.slice-033.json`). Note it again requested closed slices 033–037. No terminal summary existed at this era.

### R7 — kd-20260626-184032-7f7bc767 · slices 038–039 · **completed**

- 2 slices, 1 attempt each, gate passed, repair no-op. Report committed. Merges `37b2a38`, `654a279`.

### R8 — kd-20260626-195355-1aca1453 · slice-041 · **completed** (close record lost)

- 1 attempt, gate passed, checkpoint written, merged to main as `2a6fc7c`.
- The daemon correctly closed slice-041 (`status: closed`, `closed_at: 2026-06-26T20:09:02Z`) and wrote the implementation summary — in commit `641bc37` **on the integration branch, which was never merged to main**. Main received only the implementation commit. Result: slice-041 is still open per main's slice JSON today, and R8's report was never promoted.
- Extracted from the stranded branch: `raw/slice-041.closed.from-integration-branch.json`, `raw/kd-20260626-195355.implementation-summary.from-integration-branch.json`, plus the transient `raw/kd-20260626-195355.{final-report,checkpoint}.json`.
- This is the mechanistic root of status drift observed in this repo: **the daemon told the truth; the promotion path dropped it.** The 19:45 manual backfill of `closed_by_run` for slices 033–040 (`fb023bc`, = slice-040 `manual-root-cause-fix`) is the operator compensating for the same gap one run earlier.

## Cross-repo runs 2026-06-26 → 2026-07-05 (24 runs, other repositories)

Authoritative source: `raw/state-runs-2026-07-06.csv`. Recurring classes, each a candidate ledger entry:

1. **Deterministic Pi auth launch failure burned 3 attempts, three times, and persists after the PI-01 fix.** `kd-20260703-202442` (repo-C), `kd-20260704-202818` (repo-E, the audit's run), `kd-20260705-210534` (repo-E): each shows 3 `worker_error` events with `No API key found for openai`, terminal `failed` (not `blocked`), no `run_incident`. The July 5 run postdates `55bb0ac` (PI-01 fix, July 4) in source. Undetermined whether the running daemon binary predates the fix or the classifier misses the `did not become ready` launch path — **PI-01's acceptance criteria have never been observed to hold in production.** Full event dumps: `raw/state-events-auth-failures-and-incidents-2026-07-06.txt`.
2. **Closed-slice re-runs recur across repos and dates.** R5/R6 here (June 26); DRE-00/DRE-01 re-merged with attempts by `kd-20260629-193208` after being closed by `kd-20260629-125510`; `kd-20260703-213748` cancelled with operator note *"Operator noticed --all is rerunning already-completed CPLX slices"*; `kd-20260705-221736` re-merged M0 already merged by `kd-20260703-074946`. Protection is inconsistent: sometimes the daemon admits the slice and the **LLM worker** detects the closed state from the handoff and reports blocked (`kd-20260629-145351`, `kd-20260629-193208`, `kd-20260705-221736` M1); sometimes it silently re-runs. The four `run_incident` events in the entire store are all `slice_close_skipped` (`kd-20260628-204231`, EVF slices — slice metadata absent in the integration worktree), the same close-record fragility surfacing as an incident.
3. **Blocked semantics are overloaded.** `kd-20260627-122324`: worker reported blocked while its own message says the work "is present and committed". Runs blocked on "already closed" (class 2) also end as worker-blocked. "Blocked" currently mixes: needs operator intent, already done, and wrong-queue conditions.
4. **Scope-fence enforcement works and is exercised.** `kd-20260703-074509` (repo-E, M0): `worker changed files outside slice areas: src/cli.ts, src/index.ts, …` → run failed after 2 attempts; the immediately following run succeeded. The fence is real, not aspirational — at the cost of a full failed run.
5. **Operator trust bail-outs are recorded verbatim.** `kd-20260628-230119`: *"User requested stop before repair/integration; will validate current branch manually."* `kd-20260703-213748`: the `--all` re-run cancel above. Operators route around integration when they distrust the queue state.
6. **Integration gate failure at run end.** `kd-20260703-074946`: all 11 M-slices merged into the integration branch, then `integration gate failed` → run `failed`; slices show `merged` in slice state while the run failed. Partial-state readability issue feeding class 2 (M0 later re-ran).
7. **The escalation channel has never fired.** `worker_questions` has 0 rows across 32 runs. D3/PI-04's `ask_operator` path has no production evidence; every worker stop became blocked/failed instead.
8. **Positive evidence — the core loop earns its keep at scale.** 12-slice runs completed end-to-end (`kd-20260627-220920` REF, ~70 min; `kd-20260628-153707` EWD, ~93 min). Retries recovered real failures (REF-03 merged on attempt 3; EWD-03, EWD-10, DPR-03, UXV1-01 on attempt 2). 98 `slice_merged` + 98 `checkpoint_written` events; 6 `run_resumed` events; 13 `integration_repair_completed`. `run-summary.json` and `worktrees_cleaned` appear from June 27 onward (the forensics hardening works).

## Residue observed in this repo (2026-07-06)

- 24 stale `khazad/*` branches from all 8 self-runs, including completed ones; R5's and R8's integration branches carry content that exists nowhere else (R8's now extracted to `raw/`).
- 5 empty run-container directories under `~/.khazad-doom/worktrees/`.
- `.workflow/reports/` holds reports for R2, R3, R4, R7 only; R5/R8 reports were never promoted; R1/R6 predate terminal summaries.
- Slice provenance in `.workflow/slices/`: 021–030 `legacy-pre-rust-rewrite`; 031–036, 038–039 closed by runs; 037 `manual-2cfa8d0` and 040 `manual-root-cause-fix` (recorded mid-dogfooding bypasses); 041 open in main despite R8 (see R8); PI-00/PI-01 have no status field.

## Phase -1 done-when check

- Every surviving run directory has a committed summary: **R1–R8 above.**
- The existing audit is linked as processed evidence: `docs/design/worker-run-complexity-audit.md` covers `kd-20260704-202818-77a66b53`; its run directory lives in repo-E (and has already been cleaned there), and the run's authoritative row + full event stream are preserved in the local-only snapshots (grade A upgrade of the audit's grade B narrative).
- Evidence reviewable without `.workflow/runs/` or the live state store: yes — this summary plus the committed `raw/` artifacts for this repo's runs; cross-repo detail is reviewable by the operator via the local-only `raw/state-*` snapshots and `local/` lookup.

Committed 2026-07-06 together with `REVISION_PLAN.md`. The local-only snapshots are the operator's responsibility to retain (they are the sole grade-A record of the cross-repo runs; the source run directories in repo-B..repo-E are transient and at least one is already gone).
