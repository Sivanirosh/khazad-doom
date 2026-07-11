# Pre-0.1.0 ledger reconciliation mission

Status: operator-selected on 2026-07-11 after pushing `main` at `2fdacc1`.

## Mission

Reconcile the 19 historical slice records audited in `docs/design/evidence/open-slice-ledger-audit-2026-07-11.json` without rerunning landed implementations, weakening acceptance, or rewriting failed, blocked, and cancelled runs as completed.

The release remains untagged until this mission produces truthful terminal repository state and a final release gate.

## Required semantics

- `closed` continues to mean accepted historical work.
- A new repository-only `retired` status means non-runnable superseded intent, never accepted work. It does not add a per-run `SliceStatus`.
- Historical acceptance uses typed attestation metadata and preserves cited run/slice terminal states verbatim.
- A successful reconciliation run is the attesting authority; historical targets never appear in its ordinary `completed_slices` list.
- Superseded records use `retired`, not `closed`.
- Failed, blocked, or cancelled reconciliation leaves every target open.
- The daemon applies dispositions only after the ordinary integration gate and publishes the declaration, target records, attestation/retirement report collections, and ordinary reports atomically.
- Frontier autonomy is `off`; the operator must approve every target disposition and final declaration.
- Run admission pins the exact operator-committed declaration bytes and SHA-256 before worker launch. Worker, repair, replan, resume, recovery, or finalization divergence fails closed, and the pinned declaration identity is part of the publication manifest and receipt.

## Ordered slices

### LEDGER-01 — Lifecycle and attestation mechanism

Add the minimal repository lifecycle, typed declaration, validation, scheduling/frontier/roadmap semantics, and daemon-owned atomic publication path. It changes none of the 19 audited records.

This is the only slice instantiated before the new contract exists. After it closes, reinstall/restart the daemon before authoring the typed declarations used by later slices.

### LEDGER-02 — Settled evidence disposition

After LEDGER-01, create an operator-authored typed declaration outside worker-authorized areas, commit it before admission, and require the daemon to pin it before any worker starts. The declaration:

- historically accepts `CPLX-01`, `CPLX-02`, `PI-00`, `PI-01`, `PUB-01A`, `REPAIR-01`, `slice-041`, `TUI-CANCEL-01`, `TUI-DOGFOOD-01`, `TUI-PACKAGING-01`, `TUI-TIMEOUT-01`, and `FEED-02`;
- retires `CPLX-03`, `CPLX-04`, `TUI-PROOF-01`, and `TUI-PROOF-02` with closed replacements;
- records negative-proof, failed-run, blocked-run, legacy-report, and supersession limitations exactly;
- runs the declared current checks for PI, repair, packaging, and compatibility evidence but makes no opportunistic product changes.

### LEDGER-03 — Bounded current validation

Remeasure and validate `CPLX-05`, `HERDR-04B`, and `HERDR-05B` against the post-CA-09 tree. Prefer evidence-only closure. Add only tests or cleanup still required by an original criterion; broad extraction, authority changes, or acceptance weakening require a new operator decision. All three remain open if complete evidence cannot be produced truthfully.

### LEDGER-04 — Release truth seal

Verify exactly the original 19 IDs: 15 accepted historical records, 4 retired superseded records, and 0 open. Confirm every disposition's daemon publication evidence, preserve historical terminal limitations, run full release gates, update roadmap/memory truth, and produce the final pre-`0.1.0` ledger report. This slice changes no target status.

## Bootstrap constraint

Do not create LEDGER-02 through LEDGER-04 JSON records before LEDGER-01 lands. Their operator-authored reconciliation declarations require schema and daemon semantics that do not exist at `2fdacc1`; checking in future fields early would make current repository validation fail, while omitting them would permit an unsafe premature run.

## Stop conditions

Stop and ask the operator if:

- any disposition would falsify a historical run or ordinary `completed_slices`;
- a retired record lacks a closed replacement that covers its intent;
- evidence differs from the committed audit;
- a target needs new implementation rather than bounded validation;
- atomic publication/recovery cannot include the full disposition set;
- roadmap or memory truth would need to claim more than canonical slice/report evidence supports.
