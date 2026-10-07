## 1. Partitions, filters and marks of hash-table

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [ ] 1.2 Move the sections of `docs/table.md` about partitions and the
      Bloom filter to `openspec/specs/hash-table/design.md`, the text
      unchanged, and leave a pointer behind. Check: every moved line is
      in the new file in its order; `.github/scripts/check-specs.sh`
- [ ] 1.3 Write the design's sections on partitions, the filter and the
      marks anew, with the definitions left to the spec. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.4 Correct what the comparison of the documents, the code and the
      tests finds, each in a commit of its own with its test, and add a
      test for every scenario that has none. Check: `make rust-check`;
      `make rust-loom`; `make installcheck` for the C suite; the machine
      code of the benchmark programs and of the kernels' module against
      `main`; `.github/scripts/check-specs.sh`
- [ ] 1.5 Archive the change, which adds the requirements to
      `openspec/specs/hash-table/spec.md`; delete the folder the archive
      makes; put what is left into `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
