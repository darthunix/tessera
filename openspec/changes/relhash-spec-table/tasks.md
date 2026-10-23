## 1. The specs

- [x] 1.1 Write the proposal, this design and the deltas: the nine
      requirements of `relhash` in the crate's terms, and in
      `hash-table` the nine removed and "The C API over the crate"
      added. Check: `openspec validate relhash-spec-table --strict`;
      `.github/scripts/check-specs.sh`
- [x] 1.2 Take out of `table-crate` what this change now carries: the
      entries of its `hash-table` delta for the index, a record,
      appending rows and the calls of one writer, and the purpose of
      its `relhash` delta; say so in its design and tasks. Check:
      `openspec validate table-crate --strict`;
      `.github/scripts/check-specs.sh`

## 2. The designs

- [x] 2.1 Move the sections of `hash-table/design.md` that explain the
      table itself to `relhash/design.md`, word for word. Check:
      `.github/scripts/check-specs.sh`
- [ ] 2.2 Write `relhash/design.md` in the crate's terms, with its
      Files and Tests, and make `hash-table/design.md` point to it.
      Check: `.github/scripts/check-specs.sh`

## 3. The close

- [ ] 3.1 Archive the change, delete the folder the archive makes, put
      "The C API over the crate" beside the requirements it serves, and
      bring the purpose of `hash-table` up to date; tick task 4.1 of
      `table-crate`. Check: `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
