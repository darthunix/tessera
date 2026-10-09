## 1. The participants of a shared table

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [x] 1.2 Move the text of `docs/table.md` on several participants and
      the atomics to `openspec/specs/hash-table/design.md`, the words
      unchanged, and leave pointers behind. Check:
      `.github/scripts/check-specs.sh`
- [x] 1.3 Write the design anew. Check: `.github/scripts/check-specs.sh`
- [x] 1.4 Correct what the comparison finds, each in a commit of its own
      with its test, and add the tests of the promises without one.
      Check: `make installcheck`; `make rust-check`; `make rust-loom`;
      `.github/scripts/check-specs.sh`
- [ ] 1.5 Archive the change, delete the folder the archive makes, and
      put what is left into `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
