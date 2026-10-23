## 1. The specs

- [x] 1.1 Write the proposal, this design and the deltas: the ten
      requirements of `relhash` in the crate's terms, and in
      `hash-table` the nine and "Errors of a call" removed, the
      participant that stops and the status of an error added, and
      "The C API over the crate" changed. Check:
      `openspec validate relhash-spec-partitions --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 1.2 Take out of `table-crate` what this change now carries: the
      entries of its `hash-table` delta for errors of a call, appending
      rows by partition, splitting a chunk by partition and groups by
      partition; say so in its design and tasks. Check:
      `openspec validate table-crate --strict`;
      `.github/scripts/check-specs.sh`

## 2. The designs

- [ ] 2.1 Move the sections of `hash-table/design.md` on partitions,
      the Bloom filter and the marks to `relhash/design.md`, word for
      word. Check: `.github/scripts/check-specs.sh`
- [ ] 2.2 Write them in the crate's terms, with the Files and Tests of
      `relhash`, and give back to `hash-table/design.md` what is
      Tessera's, pointing to `relhash`. Check:
      `.github/scripts/check-specs.sh`

## 3. The close

- [ ] 3.1 Archive the change, delete the folder the archive makes, put
      the requirements of both specs in the order of their purposes,
      and bring both purposes up to date; tick task 4.2 of
      `table-crate`. Check: `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
