# Herdr dogfood review evidence — 2026-07-07

Scope: review of dogfood run `kd-20260706-230801-b54357e3`, which completed `FEED-01`, `HERDR-01`, `HERDR-02`, and `HERDR-03`.

## Observations

### D7 drift caught at occurrence one

`src/cli.rs` implemented Herdr protocol details for the operator-side cockpit open/focus path: workspace list/create/focus, `root_pane.pane_id` JSON parsing, and a Herdr version probe. `src/workflow/cockpit.rs` already exposed the seam intended to hide those details.

Correctness impact: none observed. Herdr does not enter the workflow truth path, and daemon-owned state remains authoritative.

Design impact: this is the same duplication/drift pattern D4 was written to prevent for Pi, now observed for D7 in the mildest possible form.

Disposition: create `HERDR-01B` to enforce one Herdr protocol implementation and make CLI delegate through the `Cockpit` seam.

### Invalid worker output loses attempt evidence

The invalid-output retry path has now reproduced twice:

- `FEED-01` attempt 1 retried without a durable worker attempt artifact/event that preserved the invalid worker output evidence.
- `HERDR-01` attempt 1 showed the same pattern.

Counterexample/localization:

- `HERDR-02` check-failed attempt 1 preserved full evidence, so the gap is localized to invalid worker-output handling rather than retry/check failure evidence in general.

Correctness impact: the daemon eventually retried and completed. Evidence impact: economics and attempt history can overstate success because an invalid-output agent call disappears from durable artifacts.

Disposition: fold into `RPL-02`, whose declared scope already covers worker output schemas, finding disposition, and repair-authority evidence. `RPL-02` must preserve invalid-output attempts as first-class attempt evidence before retry.

### `ask_operator` remains unproven in production

Across the FEED/HERDR run attempts and retries, no production `ask_operator` path fired. This does not disprove the implementation; it confirms the evidence gap remains open.

Disposition: keep `PI-PROOF-01` as the black-box proof for ask/answer/timeout/restart behavior.
