## 1. The capability hash-table

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [x] 1.2 Move the sections of `docs/table.md` about the table itself
      to `openspec/specs/hash-table/design.md`, the text unchanged, and
      leave a pointer behind. Check: every moved line is in the new
      file in its order; `.github/scripts/check-specs.sh`
- [x] 1.3 Write `design.md` anew, from the whole to the details, with
      the definitions left to the spec. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.4 Add a test for every scenario that has none, and correct what
      the comparison of the documents, the code and the tests finds,
      each in a commit of its own with its test. Check:
      `make rust-check`; `make installcheck` for the C suite;
      `.github/scripts/check-specs.sh`
- [ ] 1.5 Archive the change, which makes
      `openspec/specs/hash-table/spec.md`; delete the folder the archive
      makes; put what is left into `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
