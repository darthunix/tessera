## Why

Tessera builds against PostgreSQL master (20devel) alone, and its
modules stop the build on any older core. No one runs master in
production, so no one can run Tessera there. PostgreSQL 19 is the
first released core close enough to master to support at a small
cost: a build against REL_19_STABLE fails in five places only. A
released core is also what a comparison with other extensions needs:
pg_duckdb supports PostgreSQL 14 to 18 and builds with 19. Older
cores (17, 18) differ much more, in the deforming of tuples and in the
planner, and wait until someone asks for them.

## What Changes

- The modules build and pass every suite on PostgreSQL 19 as well as
  on master. The check in `abi.h` stops the build below 19 (190000)
  rather than below 20devel.
- Five places of the code read the core in two ways, chosen when the
  modules are compiled by `PG_VERSION_NUM`:
  - the table's statistics counters (master puts them in a union of
    table and index counters);
  - the names of the comparators of abbreviated sort keys (master
    renamed them and added one for unsigned 32-bit keys);
  - the arguments of `index_beginscan` (master added whether the scan
    is index-only);
  - the next row of an index-only scan and of a lookup (master reads it
    through `table_index_getnext_slot`; 19 has none, so the node keeps
    the loop of 19's `IndexOnlyNext`);
  - the tags of TOAST pointers (master added one for 64-bit chunk ids
    and renamed the old one).
- One suite's output differs on 19, only in plans: 19 estimates a
  UNION's rows as all its input's, so the node's grouping of a UNION
  by the workers is not chosen there. That section becomes a suite of
  its own with a second expected file for 19.
- CI builds a pinned revision of REL_19_STABLE and runs every suite on
  it, on Linux x86-64 with the release kernels.
- The documents that say "master only" say "19 and master".

Nothing changes on master: every branch keeps master's code as it is.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. No capability's spec names the cores Tessera builds against; the
README, `docs/limitations.md` and `CONTRIBUTING.md` do. The change
declares `skip_specs: true`.

## Impact

- Code: `include/tessera/abi.h`, `nodes/heapscan.c`, `nodes/sort.c`,
  `runtime/spill.c`, `test/tessera_spill_test.c`.
- Tests: index-only scans that reach 19's own branches in `index`; the
  section of `union` on grouping by workers moves to `union_parallel`,
  which gets a second expected file for 19.
- CI: `.github/postgres.env` gains `POSTGRES_19`; `.github/workflows/
  ci.yml` gains a row of the PostgreSQL job.
- Documents: `README.md`, `docs/limitations.md`, `docs/bridge.md`,
  `CONTRIBUTING.md`, `openspec/roadmap.md`.
- Rust: one comment of `crates/tessera-capi`; the crates do not read
  the core's headers, and the TOAST pointer of 19 is master's 32-bit
  one, tag 18, 16 bytes.
