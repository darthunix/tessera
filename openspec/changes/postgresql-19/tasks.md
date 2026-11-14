## 0. How the work goes

Every task is one commit on the branch `postgresql-19`, which reaches
`main` in one pull request. Each commit passes
`.github/scripts/check-specs.sh`, run with the version of OpenSpec that
`ci.yml` names; the check named in a task comes on top.

The words used below:

- **"Both cores"**: PostgreSQL built as
  `.github/scripts/build-postgres.sh` builds it, with assertions and
  injection points, at the revisions of `.github/postgres.env`: master
  and REL_19_STABLE.
- **"The suites on both cores"**: on each of both cores, `make
  COPT=-Werror && make install COPT=-Werror && make installcheck
  COPT=-Werror EXTRA_REGRESS_OPTS="--temp-instance=..."` with
  `LANG=C.UTF-8`.

## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`.
      Check: `openspec validate postgresql-19 --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [x] 2.1 The five places read 19's interfaces under `PG_VERSION_NUM
      < 200000`; `abi.h` stops the build below 19 and says so; the
      comments of `sort.c` and of the Rust TOAST check name 19. Check:
      the suites on both cores; `objdump -d` of the changed object
      files built against master, before and after, the same but for
      line numbers
- [x] 2.2 The suite `index` gains index-only scans of a GiST index, of
      a btree index over `name`, and in parallel over pages not all
      visible. Check: the suites on both cores
- [x] 2.3 The section of `union` on grouping under `TessGather` moves,
      as it is, to a suite of its own, `union_parallel`, with the
      helper and the table it reads. Check: the suites on master; the
      moved lines of both expected files the same
- [x] 2.4 `union_parallel` first prints whether the core is master;
      `union_parallel_1.out`, its output on 19. Check: the suites on
      both cores, with the diff of the new file against the first read
      line by line: the line of the core, plans, and the partial
      grouping's early emits only; every `union_same` "same"
- [ ] 2.5 `POSTGRES_19` in `.github/postgres.env`, its row in the
      PostgreSQL job of `ci.yml`, and the random queries' findings
      named by core. Check: CI on the pull request
- [ ] 2.6 The documents: README, `docs/limitations.md` (the cores, and
      UNION's grouping by the workers not chosen on 19),
      `docs/bridge.md`, `docs/nodes.md`, `CONTRIBUTING.md`. Check:
      `.github/scripts/check-specs.sh`

## 3. The close

- [ ] 3.1 Close the change: the folder goes; the decisions on the
      index-only scan of 19 stay in the comments above its code; the
      entry `postgresql-19` leaves the roadmap, and the entries that
      named it (`pg-duckdb-comparison`, `greengage-port`) are updated;
      "Not placed" gains the unchecked `xs_recheckorderby` of the
      node's index-only scan.
      Check: `.github/scripts/check-specs.sh`
