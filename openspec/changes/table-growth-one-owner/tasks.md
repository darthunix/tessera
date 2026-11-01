## 0. How the work goes

Every task is one commit on the branch `table-growth-one-owner`, in the
worktree of that name; the whole change, from the proposal to its
close, is one pull request, and code starts after the proposal is
approved. Each commit passes `make rust-check`, `make && make install`,
`make installcheck EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"`
and `.github/scripts/check-specs.sh`, run with the version of OpenSpec
that `ci.yml` names; the check named in a task comes on top. A fix is
committed with the test that shows its fault: the test is first run on
the commit before the fix, where it must fail, and that failure is
quoted in the commit's message. A scenario's "pending" becomes the
test's name in the commit that adds the test. Every changed line of an
expected output is read before it is accepted. Codex reviews the work
as it goes.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec of `table-memory`, the design
      and the tasks of this change. Check:
      `openspec validate table-growth-one-owner --strict`;
      `.github/scripts/check-specs.sh`

## 2. What the code does now

- [x] 2.1 Add the measured cases of the design's plan: in `join`, the
      serial joins built on 250 000, 300 000 and 420 000 rows at
      `hash_mem` = 16 MiB, their tables made by the family; in `spill`,
      the grouping at 3 MiB above the cap and the grouping that reads
      partitions back and regrows; in `anyagg`, the grouped
      `count(DISTINCT)` of 3000 pairs at `work_mem` = 256 kB; and make
      `bench/pg/run.sh` keep the `PGOPTIONS` it is given, so that a run
      can bound its statements, with a line in its README. Check: one
      run of each case under `statement_timeout`, its plan read in
      `plans.txt` to be Tessera's, its Memory Usage noted

## 3. The faults, shown and fixed

- [ ] 3.1 One note of a node's memory that takes a replaced index still
      alive; a DISTINCT set notes its first index, its chunks and its
      regrows through it, and the grouping's regrow notes the same
      whole. Check: the scenarios "A regrow is counted with both
      indexes" and "DISTINCT over no rows" in `agg`, and the peak of
      one set of 3000 values, 1205 kB (before: 73 kB)
- [ ] 3.2 The chunks of a DISTINCT set past the first take an eighth of
      `hash_mem`, at most 1 MiB. Check: the scenario "A DISTINCT set
      under a small limit" in `agg`, 245 kB (before: 1205 kB);
      `cargo ab measure` of the DISTINCT case against the parent
- [ ] 3.3 A grouping's reload keeps its totals: a merge counts the
      chunks it adds, starts from a reset total and ends with the index
      it has, and a rebuild notes the new index while the old is alive;
      in assert builds each note checks the count against the chunks
      it owns, the two reserved slots left out, and the index. Check:
      the scenario "Groups read back and merged" in `agg`; its reload
      cases, whose check fails before the fix; the peak's bound, which
      fails with the rebuild's note left out
- [ ] 3.4 The serial join checks, before a batch, the exact size of the
      index its records will need, without counting it as memory.
      Check: the scenarios of "A serial build keeps its index within
      the limit" in `join`; the plans of `join` read; `cargo ab
      measure` of the three joins against the parent
- [ ] 3.5 A grouping's first index is capped by its buckets. Check: the
      scenario "A limit that is not a power of two" in `agg`; `cargo
      ab measure` of the grouping at 3 MiB against the parent

## 4. One owner

- [ ] 4.1 Write `nodes/table_chunks.c`, the directory's reserve with
      54000 and the add of a chunk, and make the serial join, its
      spill, the shared join's adoption, the grouping and its spill,
      and the DISTINCT sets use it; each keeps its own bookkeeping.
      Check: the scenario "The limit of a directory" through the C
      test module; the suites, the shared and rescan cases of `join`
      and the reload cases of `agg`; `cargo run --release --locked -p
      tessera-bench -- --base HEAD~1 --disasm --module nodes` read
- [ ] 4.2 The merge of a grouping's reload adds its chunks through the
      helper. Check: the scenario "A merge at the most chunks", review
      only: every write of a chunk into a directory in `nodes/` listed
      and shown to go through the helper; the reload cases of `agg`
- [ ] 4.3 Every chunk past the first of a table in memory takes its
      length from `tess_spill_chunk_len` for one partition at a share
      of 8; `join_chunk_len_for` and the grouping's formula go. Check: a
      Rust test of `tess_spill_chunk_len` at the six limits of the
      design; the disassembly of `nodes` read
- [ ] 4.4 Bring the documents up to date: the design of `hash-table`
      names the one owner; `docs/nodes.md` on Memory Usage; task 3.9 of
      `spill-crate` moves the rule with its callers, the tables in
      memory included. Check: `.github/scripts/check-specs.sh`

## 5. The checks of the whole

- [ ] 5.1 Measure as the design's plan says, on an idle machine the
      person confirms. Check: `cargo ab measure` flags no case but the
      joins of 300 000 and 420 000 rows, given with their plans and
      memory; the runs' ids in the pull request

## 6. The close

- [ ] 6.1 Close the change: `openspec archive table-growth-one-owner`,
      which writes the spec of `table-memory`, and the archive's folder
      deleted in the same commit; the design of `table-memory` written
      from this design; the roadmap entry removed; the planner's gate
      of DISTINCT, which leaves out the set's index, and every other
      finding without a decision into "Not placed". Check:
      `.github/scripts/check-specs.sh`
