## 1. The plan of a level

- [x] 1.1 Write the proposal, the spec, the design and the tasks of this
      change. Check: `.github/scripts/check-specs.sh`
- [ ] 1.2 Explain the plan of a level in the design of `hash-table`, and
      point `spill-format`'s design to it. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.3 Refuse what the comparison finds, with tests, and test the
      rule on examples. Check: `make rust-check`
- [ ] 1.4 Archive the change, delete the folder the archive makes, and
      take the finding out of `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
