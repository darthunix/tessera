## Why

The probe of a batch in `relhash` goes over the selected rows step by
step and, before each step, asks the processor to fetch the memory of
the next one: every row's bucket, then every row's record. The hint is
one instruction, `prfm pldl1keep`, on AArch64, and none on other
processors. It is given whatever the size of the table.

On the Intel Mac the close of `simd-primitives-avx2` measured the same
hint on x86-64, `prefetcht0` (`compare-aXu7WT` to `compare-4Dnv47`):

- on a table of 160 MiB, far larger than the caches (`table_large`),
  the five calls that probe took 10 to 26 % fewer cycles;
- on a table that fits in L1 (`table_int32`), the calls whose code it
  changed took up to 9 % more instructions, 33 of 86 took more than 3 %
  more cycles in both runs, up to 14 %, and one passed the bar of 10 %.

So x86-64 gives no hint, and loses the gain where it matters most: a
join's build side is mostly larger than the caches. A hint given only
to tables larger than some size would keep the gain and lose nothing.
On AArch64 the hint is given to every table, and what it costs a table
in the cache there has not been measured.

This change takes the last of the findings that the close of
`simd-primitives-avx2` left without a decision. The maintainer took
the six findings on 2026-11-01, in three changes. This one does not
depend on the other two: its question is the caches, not the branches.

## What Changes

- First, the rules. The hint adds its own instructions and lengthens
  the loops that give it, so it fails the rule of 1 % of instructions
  and the contract's rule "No cost to Tessera" on every table where it
  is given. The maintainer chose to propose narrowing both for a hint
  given by size, in a pull request of contracts and designs only; this
  change waits for that decision, and closes without the hint if the
  rules stay.
- The probe, the find or insert and the gathers of a batch give the
  prefetch hint when the table is larger than a bound of bytes, and
  none otherwise; the choice is made once a call, from the sizes the
  table already knows: its buckets and its records. Under the bound
  their machine code is today's. The bound is a constant for each
  architecture.
- x86-64 gets the hint, `prefetcht0`, on tables above that bound;
  AArch64 keeps `prfm pldl1keep` and gets a bound of its own, measured
  on the M5 Pro, if a table in the cache pays for the hint there too.
- `table_large` measures tables of eight sizes between the caches and
  memory, with a find or insert that creates records and a gather of
  records in no order, so that the bound can be read off the counters.
- The design of `relhash` says how the hint is chosen; the forecast of
  its contract says what the change touched.

Not in this change:

- The other findings: `bench-unlearned-inputs` and
  `whole-word-decisions`.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. A hint changes no result and no state a program can observe.
No public API of `relhash`, which is meant for use outside Tessera,
changes: the choice is inside the crate, and its snapshot of the API
stays. No behavior seen from SQL or format changes. The change
declares `skip_specs: true`.

## Impact

- Code: `crates/relhash`, `table/lanes.rs` (the hint), and the probe,
  the find or insert and the gathers of a batch, `table/batch.rs` and
  `table/writer.rs`, which choose it. The forecast of the contract, "A
  prefetch hint for x86-64", names `table/lanes.rs` alone; a hint
  chosen by size touches those loops too. The pull request says so
  under "Architecture", corrects the forecast, and adds the entry of
  the roadmap for simplifying the crate's hint that `CONTRIBUTING.md`
  asks for, naming the boundary it crossed.
- `unsafe`: the hint of x86-64 is one `unsafe` call of an SSE
  intrinsic in `table/lanes.rs`, a module on the design's list of
  unsafe code, as the hint of AArch64 is.
- The benchmark program `table_large` and its README entry.
- Documents: the design of `relhash`, its contract's forecast.
- No header, setting, SQL suite, format or dependency changes.
- The roadmap: the finding leaves "Not placed" for this change's
  entry; at the close the entry leaves.
