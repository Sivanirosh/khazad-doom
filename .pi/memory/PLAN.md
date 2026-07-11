# Plan

## Current baseline

- CA-01 through CA-09 are closed on `main`. The remediation program established verification/publication purity, recoverable terminalization, append-only launch identity, transactional decisions and admission, restart-safe merge authority, coherent status projection, bounded runtime evidence, typed authority/provenance contracts, and shared Rust/Node fixtures.
- CA-09 implementation `1de3d67` and closure `82bdb04` completed the program. Final Rust, Node, daemon-integration, confinement, clippy, format, fixture, roadmap-truth, and 1/3/10-worker soak gates passed without high- or medium-severity review findings.
- Preserve the product boundary: JSON Issue Slices authorize bounded work; the daemon owns lifecycle truth, verification, merge, incidents, economics, and handoff; Pi is the real worker harness; Herdr and Pi display state are never correctness evidence.

## Ledger reconciliation

- `docs/design/evidence/open-slice-ledger-audit-2026-07-11.json` non-authoritatively classifies the 19 records still open after CA-09: 6 provenance-repair candidates, 6 acceptance-only validations, 4 superseded intents, 2 negative-proof exemption candidates, and 1 bounded remeasurement case.
- These are historical lifecycle/provenance debt, not 19 fresh implementation tasks. Do not blindly rerun landed work or rewrite failed, blocked, and cancelled runs as completed.

## Ordered next actions

1. Decide the historical attestation/provenance policy: what immutable run, commit, artifact, replacement-slice, supersession, and negative-proof evidence can authorize truthful closure, and when explicit human attestation is required.
2. Perform only validation that policy cannot resolve. Remeasure `CPLX-05` before any YAGNI cleanup; validate the full `HERDR-04B`/`HERDR-05B` painter contracts; run the declared checks for the remaining acceptance-only records only where historical evidence is insufficient.
3. Commit approved policy, closure metadata, and bounded evidence separately; rerun affected release gates; keep the branch and worktree truthful and clean.
4. Push the reconciled release candidate, then explicitly approve release or record the concrete blocker. Tag/publish/install only after that decision.

## Fresh product work

- No new product implementation is ordered before the release decision. Completed Pi, Herdr, native-TUI, repair, attention, frontier, and CA plans are not an active backlog.
- Deferred post-release direction: a thin authenticated remote/iPhone operator bridge may notify on daemon attention and expose bounded existing daemon commands. It must use feed/RPC state with nonce, expiry, idempotency, and audit; it must not scrape terminals/Herdr or receive workflow authority. Telegram is the current MVP candidate, with Pushover/ntfy only as optional alert fallbacks.
- Any post-release capability or refactor requires a new bounded slice justified by current evidence.

## Memory hygiene

- Keep project-specific memory in this repo-local `.pi/memory`; reserve home-level memory for Pi/global tooling.
