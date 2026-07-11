# Timeline

## 2026-06-25

- Established Khazad-Doom as a Rust CLI/daemon for bounded agentic coding around JSON Issue Slices, isolated worktrees, JSON-only results, daemon-owned verification, durable state, and explicit handoffs.
- Implemented the first vertical workflow: repository initialization, slice validation, runner injection/cancellation, dependency scheduling, gates, reports, recovery, Pi/fake runners, and black-box lifecycle coverage.

## 2026-06-26

- Completed the initial roadmap through parallel workers with serial merge, verification profiles/timeouts, checkpoints/resume, handoff automation, packaging, open/closed slice lifecycle, and pre-release auditing.
- Dogfooded Khazad-Doom on itself and KataForge, exposing monitor detachment, closure/worktree resume, publication, and repair failure modes that shaped later hardening.
- Added durable progress/status, worker supervision, compact activity feeds, runtime economics, gate-driven verification/repair, structured acceptance evidence, and YAGNI worker guidance.

## 2026-06-27

- Hardened daemon health/socket behavior, process-group detachment, failure classification, cancellation evidence, confinement, terminal summaries, and latest-run inspection (`d6ac4b8`).
- Unified monitor/status vocabulary and added report/handoff `exit_states` and `evidence_attestation`, preserving worker claims versus daemon attestation.
- Introduced centrally enforced agent profiles and moved project memory into repo-local `.pi/memory`.

## 2026-07-04

- Made the roadmap explicitly Pi-native and implemented the typed Pi contract, effective-profile fidelity, daemon-owned status feed, durable `ask_operator`, pause-aware timeouts, and Pi feedback surfaces (`2f03af7`) after narrow auth-failure blocking (`55bb0ac`).

## 2026-07-05

- Aligned real workers with authenticated `openai-codex`, moved profiles to operator-wide configuration, and replaced the rich monitor-overlay direction with a thin explicit feed bridge (`622c760`).
- Added daemon-owned worktree setup as a reliability boundary (`3d640c6`).

## 2026-07-06

- Began the evidence-led revision with run-evidence harvest, failure ledger, roadmap-truth audit, invariant amendments, replan RFC, and architecture review (`fc86574` through `9ce9654`).
- Publication dogfood exposed a pre-publication advertised SHA. PUB-01A fixed it and PUB-01B proved the published SHA included implementation, close records, and committed reports (`51d324c`, `29a4e55`).
- Selected Herdr as the optional-default observability cockpit while retaining daemon artifacts/state as authoritative and direct Pi as fallback.

## 2026-07-07

- Completed the Herdr/replan/Pi-proof chain: shared feed, cockpit worker/gate panes, durable replan decisions, finding disposition, plan-revision reporting, roadmap linting, and black-box Pi evidence (`294ae48`).
- The first production `ask_operator` stop and accepted replan demonstrated area-fence compliance. Follow-up work added operator attention, authorized grants, `attend`, notifications, and layout governance (`cb37476`).
- Added bounded pre-merge envelope/mechanical recovery while preserving scope authority and parallel-layer atomicity (`78269e2`).

## 2026-07-08

- Proved native Herdr-hosted Pi TUI result submission through daemon-owned artifacts rather than terminal scraping, then integrated cancellation, timeout, retry, repair, and multi-worker lifecycle behavior.
- Adversarial proofs exposed stale cockpit anchors; live role-based resolution fixed them (`8c452e6`). Native Pi TUI workers were promoted to default after proof closure (`4991d74`).

## 2026-07-09

- Completed native-TUI cleanup (`9146cca`), release metadata and cwd-independent scripts (`4a076d3`, `81e20f9`), and stale-deleted-binary cockpit recovery (`45e5e31`).
- Enforced slice `areas` as literal repo-relative prefixes (`0311eeb`) and dogfooded worker-pane `ask_operator` (`b9eb5ca`).
- Completed AF-00 and AF-01..AF-08 plus HERDR-07. Merge conflicts led to area-disjoint batching between merge checkpoints; gate-only verification semantics and follow-up hardening landed with the completed run (`8b88e5a`, `a062b62`, `94b9b08`).
- Converted the complexity audit into CA-01..CA-09 and authorized the bounded recommendation fallback ahead of remediation.

## 2026-07-10

- Closed ASK-FALLBACK-01 with transactional operator/recommendation races and terminal-question safety (`1107f4f`, closure `d51bbda`).
- Closed CA-01 verification purity/explicit publication (`3819084`, closure `1ce8462`) and CA-02 recoverable terminalization/atomic authoritative artifacts (`b6d9d7d`, closure `5b345bf`).

## 2026-07-11

- Completed the remaining complexity-remediation chain:
  - CA-03 append-only worker launch ledger and process-tree cancellation: `272ba0f`; closure `0f6b07c`.
  - CA-04 transactional question/replan outcomes: `d35a300`; closure `6655198`.
  - CA-05 transactional admission and restart-safe merge authority: `be54497`; closure `5b4f403`.
  - CA-06 coherent status/read-model projection: `2f7dede`; closure `0e38344`.
  - CA-07 bounded long-session runtime and reproducible soak: `b2fd314`; closure `2c8a891`.
  - CA-08 typed wires/events/provenance with legacy preservation: `b7d6a91`; closure `40c3cfc`.
  - CA-09 shared Rust/Node fixtures, complete Node 22 CI, and regression/soak closure: `1de3d67`; closure `82bdb04`.
- Recorded the non-authoritative 19-open-slice ledger audit in `b8e3b8c`, consolidated project memory in `2fdacc1`, and pushed the 31-commit release candidate to `origin/main`.
- Deferred the `0.1.0` tag and selected the bounded LEDGER-01..04 reconciliation mission so historical acceptance, supersession, and negative-proof evidence can be dispositioned without falsifying old run states.
