# Contributing

Changes reach `main` through pull requests: a series of commits on a
branch, CI green ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)),
and the maintainer's review.

A commit is one change that builds and passes the checks by itself;
mechanical moves and renames go in commits of their own, apart from changes
of behavior. A commit with more than about 500 lines of production code,
tests and documentation aside, says in its message why it is not split. The
message explains why the change is made, so that it reads on its own; it
may cite the item of the [working plan](docs/plan/README.md) that records
the reasons at length.

Before pushing, run what CI runs, with `PG_CONFIG` naming a PostgreSQL
master build and a UTF-8 locale (CI uses `C.UTF-8`):

```sh
make rust-check
make && make install
make installcheck EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"
```
