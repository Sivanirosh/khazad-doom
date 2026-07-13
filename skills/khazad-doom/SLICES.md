# Slice contracts

Read this branch before initializing `.workflow/`, authoring or editing a JSON Issue Slice, validating slices, or selecting work for a run.

## 1. Discover the contract

Choose the read-only CLI action that matches the request:

```sh
khazad-doom slices list
khazad-doom slices validate
khazad-doom slices schema
khazad-doom slices new --help
khazad-doom slices import-github --help
```

For an explicit setup request, choose its mutation:

```sh
khazad-doom init
khazad-doom slices schema --write
```

Initialize a repository when the contract is absent and setup was requested. Write the schema when generation or refresh was requested. In an existing repository, inspect `.workflow/khazad.json`, `.workflow/AREA_CONTRACT.md`, and the relevant `.workflow/slices/*.json` before proposing a command.

**Discovery is complete when:** the contract source needed for the requested action is known, and any action that authors or selects work has a recorded validation pass or an exact validation error.

## 2. Keep authority explicit

A slice is the atomic worker authorization envelope:

- New slices are `open`. Successful daemon publication closes them with `closed_by_run` and `closed_at`.
- Closed dependencies are satisfied historical work. Select open work; represent new intent with a follow-up slice rather than rerunning a closed slice.
- `goal` and `acceptance` define bounded intent and minimum evidence, not a frozen implementation script.
- `areas` are repo-relative literal prefixes, never globs. Use directory prefixes such as `src/normia/` and exact files such as `README.md`.
- Include every expected source, test, helper, fixture, documentation, and generated-contract path. A narrow area is a hard stop.
- Put policy, security, public-semantics, dependency, credential, permission, release, and scope uncertainties in `must_ask_if`.
- Open dependencies precede dependents; cycles are invalid.

Learning directly implied by the goal or acceptance may be handled inside declared areas and reported. A discovery that changes intent, public behavior, dependencies, verification policy, or authorized paths becomes an operator question, an authorized replan, or a follow-up slice.

**Authority is complete when:** every intended mutation path is covered by `areas`, every intended verification command is declared, dependency order is valid, stop conditions cover known authority edges, and `khazad-doom slices validate` passes.

## 3. Preserve run admission truth

Run start is clean by default. Commit or stash repository changes before launch; use `--allow-dirty` only for an operator-approved exception that should appear in preflight evidence.

Use real `pi` workers for implementation. Select `--agent fake` only for a named deterministic test or smoke seam, and label its evidence accordingly.

**Selection is complete when:** every selected slice is open, all required open dependencies are included, the source state is admissible, and any test-double choice is explicit.
