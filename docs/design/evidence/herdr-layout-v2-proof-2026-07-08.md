# Herdr cockpit layout v2 proof — 2026-07-08

## Scope

This is a scratch proof for pane mechanics only. It does not change daemon runtime behavior, worker scheduling, worker result collection, verification, or any KD correctness path.

Target cockpit shape:

- no unused root shell: the Herdr workspace root pane is renamed and used as `worker 1`;
- a left worker region;
- a right full-height dashboard;
- 1 worker fills the worker region;
- 2 workers are side-by-side inside the worker region;
- 4 workers form a 2x2 grid inside the worker region;
- 3 workers use the documented Herdr-backed fallback below instead of adding an unused spacer pane.

## Commands

Static and deterministic checks:

```bash
bash -n scripts/proof-herdr-layout-v2
scripts/proof-herdr-layout-v2 --dry-run
```

Run the scratch proof against Herdr with harmless shell commands, then clean up the scratch workspaces:

```bash
scripts/proof-herdr-layout-v2 --workers all
```

Inspect one live layout manually, leaving only the scratch proof workspace open:

```bash
KHAZAD_HERDR_LAYOUT_PROOF_SLEEP=3600 scripts/proof-herdr-layout-v2 --workers 4 --keep --focus
```

The real mode creates Herdr workspaces labelled `Khazad-Doom layout v2 proof workers=<N>`, exercises a move/close probe inside that scratch workspace, builds the target split tree, runs `printf ...; exec sleep <N>` in every proof pane, prints `herdr pane layout` JSON, and closes the scratch workspace unless `--keep` is used.

## Observed Herdr capabilities

Observed from `herdr pane --help`, `herdr workspace --help`, and scratch probe runs while authoring `scripts/proof-herdr-layout-v2`:

- `herdr workspace create --cwd ... --label ... --no-focus` returns a workspace id, tab id, and root pane id. The proof uses that returned root pane as worker 1 so there is no unused root shell.
- `herdr pane split <pane> --direction right|down --ratio <float> --cwd ... --no-focus` creates deterministic split trees suitable for the left worker region plus right full-height dashboard.
- `herdr pane layout --pane <root>` returns pane rectangles and split nodes. A scratch ratio probe showed `--direction right --ratio 0.32` produced a first/original pane about 32% wide and a new right pane about 68% wide, so the proof uses `--ratio 0.68` for the initial split to keep the left worker region larger than the right dashboard.
- `herdr tab create` plus `herdr pane move <probe> --tab <target-tab> --split down --target-pane <root> --ratio 0.80 --no-focus` moves a pane within the scratch workspace. When the source tab becomes empty, Herdr reports the source tab closed.
- `herdr pane close <probe>` collapses the temporary move probe back to the remaining root pane before the target layout is built.
- `herdr pane rename` and `herdr pane run` are enough to mark each proof pane with a harmless shell command; no KD workers are launched.

## Worker-count geometry

The script builds these layouts in real mode and prints the resulting Herdr layout JSON:

| Workers | Herdr split sequence after the move/close probe | Result |
| --- | --- | --- |
| 1 | Split root right at `0.68`. | `worker 1` occupies the left worker region; dashboard occupies the right full height. |
| 2 | Split root right at `0.68`, then split `worker 1` right at `0.50`. | `worker 1` and `worker 2` are side-by-side inside the worker region; dashboard remains right full height. |
| 3 | Split root right at `0.68`, split `worker 1` right at `0.50`, then split `worker 1` down at `0.50`. | Herdr-backed fallback: `worker 1` and `worker 3` stack in the left half of the worker region; `worker 2` occupies the right half full height; dashboard remains right full height. |
| 4 | Same as 3, then split `worker 2` down at `0.50`. | `worker 1`/`worker 2` over `worker 3`/`worker 4` as a 2x2 grid inside the worker region; dashboard remains right full height. |

## Limitations and fallback

Herdr's exposed pane primitive is a split tree, not a declarative grid with empty cells. A visually exact 2x2 grid for 3 workers would require a fourth non-worker spacer pane or an idle shell. This proof rejects that because the desired cockpit shape includes no unused root shell or spacer pane. The documented 3-worker fallback preserves the user-visible top-level target: left worker region and right full-height dashboard.

All move and close operations are limited to scratch workspaces created by the proof command and identified by returned Herdr ids. The proof does not require closing or moving panes outside a scratch Khazad-Doom layout proof workspace.

Pane layout/scrollback is observability only and not KD correctness evidence. Pane labels and live shell text are also visibility aids only; daemon-owned artifacts, worker result JSON, verification gates, and slice metadata remain the correctness path.
