# Evidence — stale cockpit anchor identity breaks native TUI slot placement

Date: 2026-07-08
Slice: COCKPIT-ANCHOR-01

## Observed failures (grade A)

- `kd-20260708-025608-0b5ce10e` (TUI-TIMEOUT-01): 4× `cockpit layout root pane is not available for TUI worker slot 1` as secondary failures during worker-timeout retries (`outputs/run-summary.json`).
- `kd-20260708-030047-bc43bb8c` (TUI-MULTI-01A..D): 4× `herdr pane layout --pane w45:p1 … pane_not_found`; only TUI-MULTI-01A produced a native `*.herdr-tui.result.json`; B/C/D completed through the wrapper fallback (`outputs/run-summary.json`).
- D7 held throughout: cockpit failures degraded visibility only; all workers produced valid JSON evidence. This is a native-TUI promotion blocker, not a correctness incident.

## Mechanism (code trace, `src/workflow/cockpit.rs` at `61c7f9e`)

One root cause: **anchor resolution is identity-based and cached where it must be role-based and live.**

- `CockpitWorkspaceRef.anchor_pane` is cached once at `workspace create` (`:1263`); the focus-existing path caches nothing (`:1236`).
- `root_pane_id()` returns the cached identity unconditionally (`:1005`) — no liveness or role validation; the `first_pane_in_workspace` fallback is order-dependent and role-blind.
- The slot-1 native TUI path targets that root, moves the TUI worker pane into its place, then closes the replaced root (`start_tui_worker_agent_in_slot`, `replaced_root`, `:1392+`). The cached identity is dead from that point and nothing invalidates it.

Both observed errors are branches of the same stale resolution:

1. Same workspace ref reused → dead cached id → `label_for_pane(dead)` → `None` → `unwrap_or_default()` → empty label **passes** the placeholder-availability check (`:1411`) → layout op targets the corpse → `pane_not_found`.
2. Re-focused workspace per attempt → `anchor_pane: None` → first live pane is the Dashboard after slot-1 replacement → real label fails the availability check → `root pane is not available for TUI worker slot 1`.

The fake adapter missed the class because it models no pane lifecycle: static pane ids, no close semantics, stable root.

## Fix contract (behavior, not structure)

- Anchors resolve live, by role label, at every layout operation; no unverified cached pane identity is authoritative.
- Dead-id lookups are explicit not-found outcomes, never empty-label "available".
- Worker region is always placeable: live worker panes, a placeholder/empty pane, or Dashboard only as a safe split base.
- `pane_not_found` → one re-inspect/re-resolve → else existing CockpitUnavailable fallback.

Regression seam: the unit suite now includes a stateful fake Herdr CLI fixture with pane registry, close semantics, and Herdr-shaped `pane_not_found`/`target_pane_not_found` failures. The three named COCKPIT-ANCHOR-01 regression tests exercise the production `HerdrCockpitAdapter` logic through that fixture; real-Herdr gated multi-slot and retry proofs still need to be rerun after the fix.

## Implementation notes

Implemented in `src/workflow/cockpit.rs` after this diagnosis:

- `inspect_layout` no longer trusts `CockpitWorkspaceRef.anchor_pane` as durable authority. It builds a live pane list and selects an anchor by role: worker-region placeholder, `worker-1` slot, verified unlabeled fresh root, Dashboard as last-resort split base, or another live pane.
- Dead pane ids do not collapse to empty labels. Slot-1 placement uses explicit live reusable panes; if only Dashboard survived worker cleanup, Dashboard is used only as a split base and is not renamed or closed.
- TUI worker placement performs one re-inspect/re-resolve retry for Herdr `pane_not_found`/`target_pane_not_found` races before falling back through the existing non-fatal cockpit-unavailable path.
- Worker pane layout mutations are serialized inside the daemon process, so parallel native TUI launchers allocate worker slots from the latest live layout instead of racing on the same stale pre-mutation inspection.
- Wrapper worker slot-1 placement uses the same live target rule, so Dashboard is not mistaken for a worker-region root.
- `docs/workflow-invariants.md` now records the live-role anchor contract.

Portable verification run:

```bash
cargo test tui_slot1_replacement_then_second_slot_placement_succeeds --quiet
cargo test tui_slot1_retry_after_close_recreates_placeholder --quiet
cargo test focused_existing_workspace_resolves_anchor_by_label_not_first_pane --quiet
cargo test cockpit --quiet
```
