## 0. How the work goes

Every task is one commit on the branch `source-scan`. Sections 2 to 6
reach `main` in one pull request; the close, section 7, is its last
commit or a pull request of its own after the maintainer's review of
the outcome. Each commit passes `.github/scripts/check-specs.sh`, run
with the version of OpenSpec that `ci.yml` names, and the check named
in its task. "The suites" means `make && make install` and `make
installcheck EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"`
on a PostgreSQL master built as CI builds it, with assertions and
injection points.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec delta, the design, the tasks and
      the contract of this change; in the roadmap, the entry
      `source-scan` taken out of turn, the planning contract and the
      answers per clause taken out of `compressed-table-source`, and
      `parquet-fdw` after it, depending on `source-scan`. Check:
      `openspec validate source-scan --strict`;
      `.github/scripts/check-specs.sh`

## 2. The registry

- [x] 2.1 `TessSource.scan_rows`, `TessScanClauses` with its answers
      (`TESS_CLAUSE_UNSUPPORTED`, `TESS_CLAUSE_PRUNE_ONLY`,
      `TESS_CLAUSE_EXACT`) and `TessSourceRegistryOps.next` in
      `include/tessera/source.h`, appended under the size rule, with
      comments on ownership, lifetime and errors; `next` in
      `bridge/source.c`; `tess_source_registry_check` in the runtime,
      declared in `include/tessera/runtime_api.h`. Tests in
      `test/tessera_source_test.c` and `test/sql/source.sql`: listing in
      order, removal, a pointer never registered, a source of the least
      size and one past the current size, and the bridge's table, a
      table that ends at `find`, one of another version and `NULL` given
      to `tess_source_registry_check`. Check: the suites `source` and
      `bridge`

## 3. The test source

- [x] 3.1 A test module, `test/tessera_scan_source_test.c`: a foreign
      data wrapper whose table generates its rows from its options, and
      a batch node that the source returns, built with the runtime's
      batch builder, serial and parallel, that serves rows too. The
      options: the rows; the cost; the columns whose comparisons with a
      constant, a parameter or an expression without columns it checks
      exactly, and those it uses only to skip groups, for the serial and
      the partial offer apart; partial paths on, off or not
      parallel-aware; a serial path not parallel-safe; foreign paths
      with path keys only; a parameterized path for a join; two paths of
      which one starts cheaper and one costs less in total, which the
      core keeps under a `LIMIT`. The node shows in `EXPLAIN` the
      clauses it checks and the wrapper's path it was built from; it
      keeps the clauses in its path's expressions, computes their sides
      without columns through PostgreSQL's evaluation of expressions,
      and, as an option, compares those values with its columns by code
      of its own, exactly and by a group's least and greatest values; it
      plans with the relation as its scan tuple. Beside it, sources that
      read nothing, are of the least size, are of a size past the
      current structure or have no `scan_rows`, one that answers and
      then returns `NULL`, a second source that reads the same tables at
      a lower cost, and ones that return each of the paths the spec
      refuses, answers not allowed, answers for a clause their path does
      not carry, a plan with qualifiers, or a plan without a clause they
      answered for among its expressions; the test registers and removes
      the sources case by case, since the order of registration decides
      the result. The rows mode only is used in this commit. Check:
      `make -C test`, and the cases of the wrapper's rows with Tessera
      off in a new suite `test/sql/source_scan.sql`
- [ ] 3.2 The installed set builds the test source from copies against
      the installed headers and runtime library, as it builds
      `TessLimit`'s sources. Check: `make installcheck` in
      `test/installed`

## 4. The planner

- [ ] 4.1 The walk over the sources and the path alone: the scan planner
      offers, for each of the serial and the partial path, the foreign
      scan path of the least total cost without parameters and path
      keys, a copy of the whole `ForeignPath` with the filter's input
      target, with the relation's clauses in the core's order and each
      marked usable or not, to each source in turn, after checking the
      registry's version and size; it checks the path and the answers
      returned, and adds the path alone, narrowed to the relation's
      target, with the rows of the path offered, for a relation without
      clauses; `TessGather` over the partial path.
      `tess_batch_scan_path` and `tess_batch_input_path` unchanged.
      Check: the suites, `planner`, `forward` and `pack` unchanged; the
      cases "Batch parents over a source", "A source read again", "A
      parallel grouping", "The path offered", "A path refused" and "No
      source reads the relation" of `source_scan.sql`
- [ ] 4.2 `TessFilter` over a source's path, serial and partial, with
      the clauses the source does not check exactly; the source's path
      alone when it checks them all; the filter's path records the
      clauses it keeps and its plan keeps only those. Check: the suites;
      the cases of the filter, of the source checking all, some or none
      of the clauses, of skipping, of a clause on an outer row's value,
      of a prepared statement's parameter and an uncorrelated subquery's
      value, of a policy over a foreign partition and a view with
      `security_barrier`, of a source's own check that reveals nothing,
      with the test source's own comparisons, of a volatile clause, of
      answers not allowed, of a source not safe in parallel, of the
      first source that reads the relation, and of parallel rows, a
      parallel count and a parallel read by a subquery's value
- [ ] 4.3 The costs of a source's paths: the source's own cost, and the
      rows of the path offered, for the path alone and for `TessFilter`
      over it; under `TessFilter` the start cost of the clauses left and
      their cost for each row the source returns;
      `tessera.scan_cost_factor` left to heap. Check: the suites; the
      cases of the costs, with a limit and a grouping over a source that
      costs more than its wrapper
- [ ] 4.4 The cases of the relations offered: Tessera off, a locking
      statement with and without `ORDER BY`, an `INSERT … SELECT` and an
      `UPDATE … FROM` over the foreign table, an `INSERT … SELECT …
      GROUP BY` whose grouping reads through the source, a lateral
      reference, a pseudoconstant clause, paths with path keys or
      parameters only, a partial path that is not parallel-aware, a
      foreign partition beside a heap one under a grouping, a foreign
      child of inheritance, a branch of a `UNION ALL`, heap tables
      beside a source that takes any path, the sources of the least size
      and of a size past the current structure, and a clause with a
      subquery. Check: the suites

## 5. The guides

- [ ] 5.1 `docs/source.md` as the guide to writing a source, with the
      test source as its example; `docs/runtime.md` ("Building paths"),
      `docs/nodes.md` (`TessFilter`), `docs/limitations.md` (foreign
      tables), `docs/bridge.md` (the source registry) and `docs/node.md`
      with the comment of `TessNode.scan_rows` in
      `include/tessera/node.h`, which point to the source's callback of
      the same name. Check: `.github/scripts/check-specs.sh`

## 6. The spec, the design and the contract

- [ ] 6.1 Every scenario of the spec delta names its test; the
      contract's pending checks name the installed set and the suite's
      cases of heap plans and of the policy over a foreign partition.
      Check: `openspec validate source-scan --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 6.2 `openspec/specs/source-scan/design.md`, written for a reader
      who knows nothing of the part: the registry, the call, the answers
      for the clauses and their security rule, the relations offered,
      the plans over a source, the costs, what was refused; `## Files`
      and `## Tests` last. Check: `.github/scripts/check-specs.sh`
- [ ] 6.3 Open the pull request, its description an index of evidence.
      Check: CI green

## 7. The close

- [ ] 7.1 `openspec archive source-scan --yes`, the folder it makes
      under `openspec/changes/archive/` deleted, the contract moved
      beside the spec; the entry `source-scan` leaves the roadmap, and
      what is left becomes entries. The outcome is the description of
      the closing pull request. Check: `.github/scripts/check-specs.sh`
