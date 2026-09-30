CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set interrupt_test :libdir '/tessera_interrupt_test' :dlsuffix

CREATE FUNCTION interrupt_after(integer) RETURNS void
AS :'interrupt_test', 'tessera_test_interrupt_after' LANGUAGE C STRICT;
CREATE FUNCTION interrupt_probe(integer, integer) RETURNS boolean
AS :'interrupt_test', 'tessera_test_interrupt_probe' LANGUAGE C VOLATILE;
CREATE FUNCTION interrupt_calls() RETURNS integer
AS :'interrupt_test', 'tessera_test_interrupt_calls' LANGUAGE C STRICT;

-- A cancel reaches a node's loop that does not return to the executor
-- (plan 4.24, review item 11): the probe, a condition never true, marks
-- a cancel pending on its first call, as SIGINT does, and the node's own
-- check ends the statement within a chunk of 64, where without it the
-- loop would run through every row first.
SET max_parallel_workers_per_gather = 0;
CREATE TABLE interrupt_t AS SELECT i AS g, i AS v FROM generate_series(1, 20000) AS i;
CREATE TABLE interrupt_inner AS SELECT -i AS g, i AS v FROM generate_series(1, 5000) AS i;
ANALYZE interrupt_t, interrupt_inner;

-- TessAgg gives out its groups while HAVING rejects them, in memory and
-- over partitions spilled to disk.
EXPLAIN (COSTS OFF)
SELECT g FROM interrupt_t GROUP BY g HAVING interrupt_probe(count(*)::int, g);
SELECT interrupt_after(1);
SELECT g FROM interrupt_t GROUP BY g HAVING interrupt_probe(count(*)::int, g);
SELECT interrupt_calls() BETWEEN 1 AND 64 AS prompt;
SET work_mem = '64kB';
SELECT interrupt_after(1);
SELECT g FROM interrupt_t GROUP BY g HAVING interrupt_probe(count(*)::int, g);
SELECT interrupt_calls() BETWEEN 1 AND 64 AS prompt;
RESET work_mem;

-- TessHashJoin gives out the rows of the preserved side without a pair
-- after its child is done, while the query's clause rejects them.
EXPLAIN (COSTS OFF)
SELECT o.g FROM interrupt_t AS o RIGHT JOIN interrupt_inner AS i ON o.g = i.g
WHERE interrupt_probe(o.v, i.v);
SELECT interrupt_after(1);
SELECT o.g FROM interrupt_t AS o RIGHT JOIN interrupt_inner AS i ON o.g = i.g
WHERE interrupt_probe(o.v, i.v);
SELECT interrupt_calls() BETWEEN 1 AND 64 AS prompt;
SELECT interrupt_after(0);

DROP TABLE interrupt_t, interrupt_inner;
DROP FUNCTION interrupt_after(integer), interrupt_probe(integer, integer), interrupt_calls();
DROP EXTENSION tessera;
