# Roadmap truth audit — Pi-native matrix

Date: 2026-07-06
Scope: `docs/roadmap/pi-native/00-matrix.md`, workpackages PI-00..PI-05, and post-`2a6fc7c` work that changed Pi-native/runtime behavior without a Khazad-Doom run.

This audit does not accept or close any slice. It reconciles roadmap status with executable evidence during the feature freeze.

## Audit rules applied

- A row is `done` only when its own success criteria, required tests, and docs are satisfied.
- Source presence is not acceptance evidence.
- A row with implementation already landed by hand but without done-level evidence is `in_progress`, not `planned` and not `done`.
- Existing `.workflow/slices/PI-00.json` and `.workflow/slices/PI-01.json` remain `open`; PI-02..PI-05 have workpackages but no JSON Issue Slice yet.
- Historical dogfooding bypass remains evidence, even if current code now passes tests.

## Commands and checks run

| Check | Result |
|---|---|
| `cargo test` | Passed: 53 unit tests + 14 daemon integration tests. |
| `npm test` | Passed: 7 worker-extension/feed-adapter tests. |
| PI-00 normative grep (`! rg -ni "harness-agnostic|harness agnostic" README.md .pi/memory docs/workflow-invariants.md skills/khazad-doom/SKILL.md src`) | Passed. |
| PI-02 production-source grep for Pi event/auth strings outside `src/pi_contract.rs` | Passed for `src/`; tests/docs still contain fixtures and criteria. |
| PI-05 renderer grep | Failed done-level criterion: `src/cli.rs` still contains event/activity/progress interpretation that duplicates `src/workflow/projection.rs`. |
| Installed-binary PI-01 smoke | Current `/home/sivanirosh/.local/bin/khazad-doom` blocks missing Pi auth after one attempt with `worker_error.failure_kind=agent_auth_required`, `retryable=false`, `operator_action_required=true`, and a `run_incident`. |

Installed-binary notes: the binary at `/home/sivanirosh/.local/bin/khazad-doom` was timestamped 2026-07-06 00:45, after the historical July 5 auth-failure run. A throwaway repo and isolated `KHAZAD_HOME` used a fake `pi` that read stdin, emitted the observed `No API key found ... /login` stderr, and exited 1. The run ended `blocked`, not `failed`; slice attempts were `1`; no retry layer ran. This settles the current classifier path as working and makes stale installed daemon/binary the most likely explanation for the July 5 contradiction, while preserving that historical production evidence was real.

## Matrix row dispositions

| Row | Audited status | Evidence | Remaining gap before `done` |
|---|---|---|---|
| PI-00 — Pi-first doctrine | `in_progress` | Normative grep passed; docs/memory/invariants contain the Pi-first decision and rejected shortcuts. | `.workflow/slices/PI-00.json` is still open and no dogfooded acceptance run closed it. Either run/close it through Khazad-Doom after the freeze, or record an explicit documentation-only exemption. |
| PI-01 — auth launch classification | `in_progress` | Unit/integration tests pass; current installed-binary smoke proves `blocked` + one attempt + incident for the missing-auth signature. | Historical production never observed the fix before this audit; PI-01 slice is still open. Add installation/stale-daemon freshness to redesign evidence or operational checks, then dogfood/close. |
| PI-02 — Pi event/CLI contract | `in_progress` | `src/pi_contract.rs` exists; parser tolerance tests pass; production code no longer parses Pi event/auth strings outside the module; contract inventory and invariant docs exist. | Required proof is incomplete: no test asserts `preflight.json.pi_contract`, no recorded fixture corpus/workflow acceptance artifact is committed, and actual provider/model capture is only as truthful as Pi-reported metadata when available. |
| PI-03 — profile fidelity | `in_progress` | `src/agent_profile.rs` exists; profile precedence/arg-generation tests pass; full test suite passes. | Required integration proof is incomplete: no dedicated assertion that `run_started`, handoff/report, status/projection, and economics all render the identical effective profile summary. Changes landed by hand. |
| PI-04 — operator escalation | `in_progress` | State table, IPC methods, CLI commands, daemon progress phase, prompt text, worker extension, and extension socket tests exist; `worker_questions` unit test passes. | Work started despite workpackage open questions. Required black-box workflow tests are missing: scripted worker asks, operator answers, worker continues; timeout blocks with question incident; daemon-restart/answer-after-interrupt behavior; production `ask_operator` has not been observed. |
| PI-05 — status projection | `in_progress` | `src/workflow/projection.rs` exists and projection unit test passes; full test suite passes; feed adapter tests pass. | Done-level invariant is false: CLI rendering still interprets event/progress/activity independently. Required projection snapshot/parity tests and grep enforcement are absent. |

## Work-outside-slice and out-of-matrix dispositions

| Work | Disposition |
|---|---|
| Runtime economics and gate-driven repair (`640b507`) | Retroactively documented as failure/economics evidence; do not add more mechanism now. Phase 2/4 should decide invariant/test changes from F-003/F-006/F-009. |
| README badge polish (`ef75ce2`) | Legitimate documentation/cosmetic exemption candidate; no product row. |
| Incident surfacing, lifecycle recovery, failure forensics, guardrails (`ef69f7c`, `d6ac4b8`, `acb7440`) | Retroactively documented as evidence and freeze context. Future work should be redesign slices only after invariant diffs are accepted. |
| Worker launch complexity audit (`750cff7`) | Legitimate evidence/design document; already processed by F-002 and this audit. |
| PI-01 implementation (`55bb0ac`) | Fold into PI-01 audit scope; current evidence passes but remains `in_progress` until dogfooded/closed or explicitly exempted. |
| Pi-native roadmap creation (`2b37cf1`) | Legitimate planning document; corrected by this truth audit where it drifted. |
| Pi-native worker surfaces (`2f03af7`) | Split across PI-02/PI-04/PI-05 audit scopes; remains `in_progress` because done-level workflow tests are missing. |
| PI event alias cleanup (`907ae7f`, `7cf3e4b`) | Fold into PI-02 audit scope; no separate row unless fixture/provenance tests reveal remaining contract drift. |
| Operator-wide profiles and removal of repo-local worker profiles (`29e33af`, `8b76af2`) | Fold into PI-03 audit scope; no separate row. |
| Old Pi monitor overlay removal and feed widget adapter (`a907099`, `622c760`) | Fold into PI-05/D6 scope. Rich overlay remains removed/deferred; explicit read-only feed attachment remains package surface but is not acceptance evidence for PI-05. |
| Daemon-owned worktree setup (`3d640c6`) | Retroactively documented as F-012 evidence, not a Pi-native row. Phase 4 should preserve the daemon-owned setup seam or create a focused redesign slice if tests/invariants require it. |

## Structural status-source evaluation

Accepted as a Phase 2 invariant candidate: live roadmap status should have one source of truth. Slice JSON plus daemon/run state should be authoritative; roadmap docs may summarize, but must not silently become a competing status ledger.

Needed enforcement options to design next:

1. Generate matrix status from `.workflow/slices/*.json` plus run/close metadata.
2. Add a doc-lint that fails when matrix/workpackage status disagrees with slice JSON or named evidence.
3. Add a slice-close check that updates or validates the matrix row as part of handoff.

Until one is implemented, the audited matrix statuses are a manual reconciliation only, not durable enforcement.
