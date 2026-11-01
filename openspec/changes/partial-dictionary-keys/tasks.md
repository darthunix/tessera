## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`, and its entry in the roadmap. Check:
      `openspec validate partial-dictionary-keys --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 The dictionaries share a quarter of `hash_mem`, and a table
      is too full only with a batch's groups at least; then remove
      `partial_keys` and its three conditions; tests of the plans and
      results in `parallel.sql` and `union.sql`; the documents. Check:
      `make installcheck` on a build of PostgreSQL master with
      assertions; `.github/scripts/check-specs.sh`
- [ ] 2.2 Measure as the design says; the ids of the runs go into the
      design. Check: the report of `cargo ab measure`

## 3. The close

- [ ] 3.1 Close the change: delete its folder; the entry leaves the
      roadmap; the run is copied to `docs/benchmarks/`; a finding
      without a decision goes to "Not placed". Check:
      `.github/scripts/check-specs.sh`; no line of the roadmap names
      `partial-dictionary-keys`
- [ ] 3.2 Open the pull request, with the outcome under Decision as
      `CONTRIBUTING.md`, "Closing a change", asks
