## Context

A TessFilter runs its clauses in the planner's order: the batch clauses
on whole batches, the row-wise clauses through PostgreSQL row by row.
A hash join that drops outer rows without a pair builds a Bloom filter
of its keys after its first 4096 probe rows, when fewer than
`tessera.join_bloom_ratio` of them found a record and the table holds
4096 records at least, and offers it to its outer child. A TessFilter
takes it only when it has a row-wise clause, and checks it right before
that clause: the batch clauses were thought too cheap to save.

They are not. A batch clause still reads its column for every row, and
a row's columns are read in order, so a clause on a late column walks
most of the row. Under EXPLAIN ANALYZE, in TPC-H Q19 the clauses of
the lineitem TessFilter take about 220 of the query's 290 ms on 6 M
rows, of which the join keeps 121; in Q21 the clause `l_receiptdate >
l_commitdate` takes about 115 ms on 6 M rows, and the join keeps 4 % of
the 3.8 M it passes. Both build tables smaller than 4096 records, which
get no filter.

## Goals / Non-Goals

**Goals:** the rows without a pair leave a TessFilter of batch clauses
alone before its clauses read their columns; a small table's filter
when it can go below; the join's rows unchanged.

**Non-Goals:** an order of the filter among the clauses by their
measured shares; a filter checked by the join itself for a small table;
a filter for a scan without a TessFilter (TessHeapScan alone); a change
of `TessQual` or of `TessKeyFilter`.

## Decisions

### First in a TessFilter of batch clauses alone

Such a TessFilter takes the filter and checks it before its first
clause: it reads the key columns, hashes them as the join does, and
clears the rows the filter rejects and those with a NULL key; then
`tess_qual_apply` runs the clauses over the rows left. The check is a
step of TessFilter, not of `TessQual`, so the C API stays as it is.

A TessFilter with a row-wise clause keeps the check where it is, before
that clause: there the row-wise clause costs the most, and the batch
clauses before it, chosen by the planner as cheap guards, stay in
front.

Refused: the filter first in every TessFilter. Before a row-wise clause
the gain is already taken, and a batch clause the planner put first may
be the more selective one. Refused for now: an order by the shares the
clauses and the filter keep, measured as the batches go; it needs the
join's share in the filter and counters per clause, and no query
measured asks for it.

### A small table's filter, to hand down only

A probe of a table of fewer than 4096 records stays in the cache, so
the join does not check a filter of its own for it: the check costs
about what the probe saves. Below the join the filter saves the
clauses' work instead. So such a table gets a filter under the same
share of rows found, built after the sample, and the join offers it to
its outer child; when the child refuses it, the join drops it and
probes as before. A shared table allocates the words of the filter with
its directory whatever its size, when the join may hand a filter down
at all: 16 to 32 bits a record of fewer than 4096 records is under
16 kB.

### Rows and errors

A row the filter rejects has no pair, and its clauses no longer run on
it, so an error a clause would raise on that row is not raised: the
same as for a row-wise clause after the filter, and PostgreSQL does not
fix the order of conditions either. The rows the join returns are the
same.

## Risks / Trade-offs

- The worst order: a filter that keeps nearly half the rows, the most
  the join's share allows, ahead of a cheap clause on an early column
  that keeps few of them. The filter's cost is a hash and a read of the
  filter a row, about one batch clause. A prototype in front of
  `l_shipdate = date '1995-01-01'`, which keeps one row in 100, still
  made the query faster, 124 ms to 98: the key column comes before the
  date in the row. The measurement takes a case for this order.
- A small table's filter that the child takes and that removes few rows
  costs a hash a row for nothing; the share rule keeps the filters to
  joins where most probe rows find no record.

## Measurement

`cargo tpch` Q19 and Q21, base and change twice in turn, serial and
with two workers, with Q1, Q6 and Q14 as controls; `cargo ab measure`
over the join family's cases with a filter (`selective`, `miss`) and a
new case of a filter that keeps about half its rows ahead of a cheap
selective clause.
