## 1. The capability partition-pruning

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [ ] 1.2 Move the text of `docs/nodes.md` about pruning while a query
      runs to `openspec/specs/partition-pruning/design.md`, the words
      unchanged, and leave pointers behind. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.3 Write the design anew. Check: `.github/scripts/check-specs.sh`
- [ ] 1.4 Correct what the comparison finds, each in a commit of its own
      with its test, and add the tests of the promises without one.
      Check: `make installcheck`; `.github/scripts/check-specs.sh`
- [ ] 1.5 Archive the change, delete the folder the archive makes, and
      put what is left into `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
