# AF-07 — Frontier attestation in reports and handoffs

Matrix row: `docs/roadmap/auto-frontier/00-matrix.md` → AF-07. Status: `planned`
(after AF-05; extend once AF-06 lands).

## Scope

Make the learning chain auditable end to end, derived only from existing records
(proposals, decisions, slices, events) — evidence history, never a second queue
authority.

Extend the existing `plan_revisions` report/handoff section with a `frontier` block:

- envelope snapshot + autonomy level actually in force;
- generated-slice graph: parent → child edges with origin proposal ids, generations,
  authorizer per promotion (operator vs. `envelope:<run>`), tier reason codes;
- budget consumption (`auto_promotions_used/max`, generated count, max depth reached);
- deferred fog: candidates left pending/deferred/rejected with rationale and revisit
  conditions;
- operator-needed stops: every Tier-3 pause and budget stop with its resolution;
- shadow agreement metrics when the run was shadow-level (from AF-04).

Also: `scripts/roadmap-truth-check` treats generated slices exactly like hand-written
ones (closure requires closed JSON + named report evidence), and gains one new check —
a slice bearing `provenance.origin_proposal_id` must reference a proposal whose decision
is `accepted`; otherwise lint fails (detects silent slice fabrication).

## Out of scope

New truth stores, planning UI, cross-run aggregation dashboards (future; the per-run
JSON is the contract).

## Data model changes

None persisted — the block is derived in report/read_model. Truth-check script change.

## API changes

Handoff JSON schema additively gains the `frontier` block; documented in the RFC.

## UI states

- Run with promotions (full graph), run with none (block says "no frontier activity" —
  empty state must exist so absence is attestable), run with unresolved pending
  candidates (handoff readiness blocked, as today, with the frontier block explaining
  what is pending), legacy runs without envelopes (block omitted, schema optional).

## Migration / backward compatibility

Old handoff consumers ignore the additive block. Old runs re-rendered produce no block.

## Permissions

Read-only derivation; nothing to enforce beyond the new lint.

## Test plan

Snapshot tests: report/handoff for a fixture run containing one auto-promoted, one
operator-promoted, one rejected, one deferred candidate and one Tier-3 stop. Lint
fixtures: generated slice with orphan/rejected origin proposal fails; accepted origin
passes; hand-written slice without provenance passes.

### Workflow acceptance test

```text
1. Operator finishes the AF-06 cross-slice test run and runs `khazad-doom handoff`.
2. Handoff JSON contains the frontier block: graph S-1→C-A (auto), C-B (operator),
   budget 1/1, zero unresolved candidates; plan_revisions and frontier agree on every
   proposal id.
3. Operator runs scripts/roadmap-truth-check after closing the epic's matrix rows —
   generated slices satisfy the same closure evidence rules.
4. Edge condition: someone hand-edits a generated slice file to point provenance at a
   rejected proposal id; the truth-check lint fails naming the slice and proposal.
5. Invariant: deleting the frontier block and regenerating it from proposal/decision/
   slice records yields identical content — proving it is derived, not stored.
```

## Definition of done

- [ ] Frontier block derived in read_model/report; handoff schema documented.
- [ ] Data model: explicitly no new persisted state.
- [ ] All four UI/report states implemented (including the empty state).
- [ ] Migration: legacy handoff consumers unaffected (fixture).
- [ ] Snapshot + lint tests pass.
- [ ] Workflow acceptance test passes.
- [ ] Docs: handoff/report format docs updated; RFC attestation section closed.
- [ ] Invariants: derivation-only proven (regeneration test); truth-check remains the
      single roadmap-status gate.

## Open questions

- Should the frontier block also land in `run-summary.json` for terminal runs that never
  reach handoff? Recommendation: yes, abbreviated (counts + pending ids), since blocked
  autonomous runs are exactly where the audit matters most.
