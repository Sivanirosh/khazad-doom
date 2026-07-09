# Khazad-Doom Area Contract

`areas` in `.workflow/slices/*.json` are repo-relative literal path prefixes, not globs.

Use directory prefixes with a trailing slash and exact file paths:

```text
src/normia/       ✅ directory prefix
tests/            ✅ directory prefix
roadmap/          ✅ directory prefix
legacy/           ✅ directory prefix
README.md         ✅ file path
pyproject.toml    ✅ file path

src/normia/**     ❌ glob
tests/*           ❌ glob
./src/normia/     ❌ leading ./
../foo            ❌ parent traversal
```

A valid area must be non-empty and must not:

- contain leading or trailing whitespace
- contain `*`, `?`, `[`, or `]`
- contain `..`
- start with `/`
- start with `./`

Khazad-Doom core is authoritative: `khazad-doom slices validate` rejects invalid areas before a worker can hit the path guard. Slice generators, PRDs, issues, and skills must generate areas that follow this contract.
