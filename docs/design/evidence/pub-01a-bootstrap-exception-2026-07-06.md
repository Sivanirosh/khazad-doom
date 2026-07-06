# PUB-01A bootstrap exception

Date: 2026-07-06  
Status: active exception; validation pending through `PUB-01B`.

## Why this exception exists

`PUB-01A` changes the daemon publication/handoff code that writes a run's final receipt. A running daemon cannot use worker-modified source code to finalize that same run. Therefore `PUB-01A` could pass its worker and integration tests while the daemon-owned final handoff still came from the previously installed daemon binary.

## Evidence

- Failed review run: `kd-20260706-215228-0f3bba96`
  - advertised `final_sha`: `9a0eb84594c7c26edbcd648c8f807c249bf4ce08`
  - integration branch tip: `1d3f90c544a783e627faa83b25efce7333f967dc`
  - only the branch tip contained closed `PUB-01A` metadata and committed run reports.
- Failed rerun review: `kd-20260706-220818-313b2039`
  - advertised `final_sha`: `ef27e8983d71177268e68ac8606072c90e31b872`
  - integration branch tip: `5de179ed68e178accac155ad4cc2b6f607efae5c`
  - only the branch tip contained closed `PUB-01A` metadata and committed run reports.

## Exception decision

The worker implementation commit from the second `PUB-01A` run is bootstrapped manually:

- `7a76a4174aea882044b26d35f73aa0132698f162` — `khazad(slice:PUB-01A): advertise publication tip sha`

The stale daemon-generated publication commits from failed `PUB-01A` runs are not accepted as closure evidence.

## Validation plan

1. Cherry-pick the worker implementation to `main` as an explicit bootstrap exception.
2. Install/restart `khazad-doom` so the daemon process uses the new publication code.
3. Run `PUB-01B` as a validation slice.
4. Accept the bootstrap only if the fresh run's handoff/final-report/implementation-summary `final_sha` equals the integration branch tip and that commit contains closed slice JSON plus committed `.workflow/reports/*` artifacts.
