## 1. The capability spill-format

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [x] 1.2 Move the sections "What is written", "The block header" and
      "Files" of `docs/spill.md` to
      `openspec/specs/spill-format/design.md`, the text unchanged, under
      a head of files, tests and history. Check: every moved line is in
      the new file in its order; `.github/scripts/check-specs.sh`
- [ ] 1.3 Take the definitions out of `design.md`, now that the spec
      holds them, and bring its statements in line with the spec. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.4 Write `outcome.md` and archive the change, which makes
      `openspec/specs/spill-format/spec.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
