CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_limit';

-- The rows of a query with the batch nodes and without them, sorted.
CREATE FUNCTION join_same(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text[];
    without_tessera text[];
    wrapped text := format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO without_tessera;
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('MISMATCH on %s off %s', with_tessera, without_tessera);
    END IF;
    RETURN coalesce(array_length(with_tessera, 1), 0) || ' rows, hash ' ||
           md5(coalesce(with_tessera::text, ''));
END $$;

-- EXPLAIN ANALYZE with the memory masked, since it depends on the allocator.
CREATE FUNCTION join_explain(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
        RETURN NEXT regexp_replace(line, '(Memory Usage|Overrun): \d+ kB', '\1: N kB');
    END LOOP;
END $$;

-- A dimension with a unique key, int8 copies of it and values past the
-- int4 range; a fact table whose keys repeat, miss and are NULL; a table
-- with three rows per key and NULL keys; a small one and an empty one.
CREATE TABLE jd (id int PRIMARY KEY, id8 bigint, big bigint, label text, n int);
INSERT INTO jd
SELECT g, g, g::bigint << 33, 'd' || g, CASE WHEN g % 5 = 0 THEN NULL ELSE g * 10 END
FROM generate_series(1, 300) AS g;
CREATE TABLE jf (fk int, fk8 bigint, fk_big bigint, v int, note text, m int, fk10 int);
INSERT INTO jf
SELECT fk, fk, fk::bigint << 33, g, 'f' || g, CASE WHEN g % 5 = 0 THEN NULL ELSE g END,
       fk * 10 + g % 2
FROM generate_series(1, 1000) AS g,
     LATERAL (SELECT CASE WHEN g % 9 = 0 THEN NULL ELSE (g * 37) % 350 + 1 END AS fk) AS key;
CREATE TABLE jdup (k int, w int, t text);
INSERT INTO jdup
SELECT CASE WHEN g % 13 = 0 THEN NULL ELSE g % 50 + 1 END, g, 't' || g
FROM generate_series(1, 150) AS g;
CREATE TABLE jsmall (k int, s text);
INSERT INTO jsmall SELECT g * 7, 's' || g FROM generate_series(1, 10) AS g;
CREATE TABLE jempty (k int, e text);
ANALYZE jd, jf, jdup, jsmall, jempty;

-- The core's merge and nested-loop joins stay out of the comparisons.
SET enable_mergejoin = off;
SET enable_nestloop = off;

-- Without the kernels there is no path.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
LOAD 'tessera_kernels';

-- An aggregate reads the join's batches, with no pack between them.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
SELECT join_same($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id$$);
SELECT join_same($$SELECT sum(jd.n), count(jd.n), sum(jf.v) FROM jf JOIN jd ON jf.fk = jd.id$$);
-- Rows to the client: columns of both sides, NULLs and text of the inner.
EXPLAIN (COSTS OFF) SELECT jf.v, jf.note, jd.label, jd.n FROM jf JOIN jd ON jf.fk = jd.id;
SELECT join_same($$SELECT jf.v, jf.note, jd.label, jd.n FROM jf JOIN jd ON jf.fk = jd.id$$);
-- The keys themselves as targets, and no target at all.
SELECT join_same($$SELECT jf.fk, jd.id FROM jf JOIN jd ON jf.fk = jd.id$$);
SELECT join_same($$SELECT 1 FROM jf JOIN jd ON jf.fk = jd.id$$);

-- int8 keys past the int4 range; an int4 key against an int8 one, both ways.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk_big = jd.big;
SELECT join_same($$SELECT jf.v, jd.big FROM jf JOIN jd ON jf.fk_big = jd.big$$);
SELECT join_same($$SELECT jf.v, jd.id8 FROM jf JOIN jd ON jf.fk = jd.id8$$);
SELECT join_same($$SELECT jf.fk8, jd.label FROM jf JOIN jd ON jf.fk8 = jd.id$$);

-- Three records per key: rounds; duplicates on both sides; NULL keys.
EXPLAIN (COSTS OFF) SELECT jf.v, jdup.w FROM jf JOIN jdup ON jf.fk = jdup.k;
SELECT join_same($$SELECT jf.v, jdup.w, jdup.t FROM jf JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT count(*), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT a.w, b.w FROM jdup a JOIN jdup b ON a.k = b.k$$);
-- Under an aggregate, the pairs of the rounds are copied into full batches
-- (compact mode): outer columns by value, NULLs among them, and text,
-- whose values are copied.
SELECT join_explain($$SELECT count(jf.m), sum(jf.m), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT count(jf.m), sum(jf.m), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_explain($$SELECT count(jf.note), max(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT count(jf.note), max(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
-- A limit reads batches too: the text values of compact batches, copied.
EXPLAIN (COSTS OFF) SELECT jf.note, jdup.t FROM jf JOIN jdup ON jf.fk = jdup.k LIMIT 1000;
SELECT join_same($$SELECT jf.note, jdup.t FROM jf JOIN jdup ON jf.fk = jdup.k LIMIT 1000$$);

-- Several keys: two int4 ones, NULL in the second on the inner side; an
-- int8 key next to an int4 one; three keys; composite keys with
-- duplicates, under an aggregate (compact batches) and as rows.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.fk10 = jd.n;
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id AND jf.fk10 = jd.n$$);
SELECT join_same($$SELECT jf.note, jd.n FROM jf JOIN jd ON jf.fk8 = jd.id AND jf.fk10 = jd.n$$);
SELECT join_same($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.fk8 = jd.id8 AND jf.fk_big = jd.big$$);
CREATE TABLE jpair (k int, j int, w int);
INSERT INTO jpair SELECT g % 10, g % 3, g FROM generate_series(1, 150) AS g;
ANALYZE jpair;
EXPLAIN (COSTS OFF) SELECT sum(b.w) FROM jpair a JOIN jpair b ON a.k = b.k AND a.j = b.j;
SELECT join_same($$SELECT count(*), sum(a.w), sum(b.w) FROM jpair a JOIN jpair b ON a.k = b.k AND a.j = b.j$$);
SELECT join_same($$SELECT a.w, b.w FROM jpair a JOIN jpair b ON a.k = b.k AND a.j = b.j$$);

-- Residual join clauses over the pairs: int4 columns of both sides with
-- NULLs, text, an OR over both sides, with rounds and compact batches, a
-- text equality next to the key, and a parameter of an outer query.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n;
SELECT join_same($$SELECT jf.v, jd.n FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n$$);
SELECT join_same($$SELECT count(*), sum(jd.n) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n$$);
SELECT join_same($$SELECT jf.note, jd.label FROM jf JOIN jd ON jf.fk = jd.id AND jf.note > jd.label$$);
SELECT join_same($$SELECT jf.v, jd.id FROM jf JOIN jd ON jf.fk = jd.id AND (jf.v > 900 OR jd.n < 100)$$);
SELECT join_explain($$SELECT count(*), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k AND jf.v > jdup.w$$);
SELECT join_same($$SELECT count(*), sum(jdup.w), sum(jf.m) FROM jf JOIN jdup ON jf.fk = jdup.k AND jf.v > jdup.w$$);
SELECT join_same($$SELECT jf.v, jdup.w FROM jf JOIN jdup ON jf.fk = jdup.k AND jf.v > jdup.w$$);
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id AND jf.note = jd.label$$);
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jf JOIN jdup ON jf.fk = jdup.k AND jf.v - jdup.w > jsmall.k) FROM jsmall$$);
-- In batches: a column against a chain over the other side, a chain on
-- both sides, int4 against int8 columns.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n * 2 AND jf.v + 1 < jd.n * 3;
SELECT join_same($$SELECT jf.v, jd.n FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n * 2 AND jf.v + 1 < jd.n * 3$$);
SELECT join_same($$SELECT count(*), sum(jd.n) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v * 10 >= jd.n$$);
SELECT join_same($$SELECT jf.v, jd.id8 FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.id8 * 3$$);
SELECT join_same($$SELECT jf.fk8, jd.n FROM jf JOIN jd ON jf.fk = jd.id AND jd.n <> jf.fk8$$);
SELECT join_same($$SELECT jf.v + jd.id8, jd.id8 - jf.v FROM jf JOIN jd ON jf.fk = jd.id AND jf.v + jd.id8 > 500$$);
-- A conditional value over both sides, under an aggregate.
SELECT join_same($$SELECT sum(CASE WHEN jf.v > jd.n THEN 1 ELSE 0 END), count(*) FROM jf JOIN jd ON jf.fk = jd.id$$);
-- A condition of several parts over both sides, NULLs in jd.n.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND (jf.v > jd.n OR jd.n IS NULL);
SELECT join_same($$SELECT jf.v, jd.n FROM jf JOIN jd ON jf.fk = jd.id AND (jf.v > jd.n OR jd.n IS NULL)$$);
-- The residual clauses in the core's order, by cost: the guard, row-wise,
-- before the division in batches, however they are written; without the
-- guard the division fails.
CREATE TABLE jguard AS
SELECT g AS k, CASE WHEN g % 4 = 0 THEN g ELSE g % 3 END AS d FROM generate_series(1, 300) AS g;
ANALYZE jguard;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jguard JOIN jd ON jguard.k = jd.id AND 1000 / (jd.id - jguard.d) > 5 AND jd.id IS DISTINCT FROM jguard.d;
SELECT join_same($$SELECT jguard.d, jd.id FROM jguard JOIN jd ON jguard.k = jd.id AND 1000 / (jd.id - jguard.d) > 5 AND jd.id IS DISTINCT FROM jguard.d$$);
SELECT join_same($$SELECT jguard.d, jd.id FROM jguard JOIN jd ON jguard.k = jd.id AND jd.id IS DISTINCT FROM jguard.d AND 1000 / (jd.id - jguard.d) > 5$$);
\set VERBOSITY terse
SELECT count(*) FROM jguard JOIN jd ON jguard.k = jd.id AND 1000 / (jd.id - jguard.d) > 5;
\set VERBOSITY default
DROP TABLE jguard;

-- Targets above the join are the node's: columns in another order than
-- the join's, which needs no Result under an aggregate, and expressions
-- over both sides, computed over the pairs, as rows, under a sort, with
-- rounds and a residual clause.
EXPLAIN (COSTS OFF) SELECT sum(jdup.w), count(jf.m), sum(jf.m) FROM jf JOIN jdup ON jf.fk = jdup.k;
SELECT join_same($$SELECT sum(jdup.w), count(jf.m), sum(jf.m) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
EXPLAIN (COSTS OFF, VERBOSE) SELECT jf.v + jd.id, jd.label || jf.note FROM jf JOIN jd ON jf.fk = jd.id;
SELECT join_same($$SELECT jf.v + jd.id, jd.label || jf.note FROM jf JOIN jd ON jf.fk = jd.id$$);
SELECT jf.v * 1000 + jd.id AS s FROM jf JOIN jd ON jf.fk = jd.id ORDER BY s DESC LIMIT 3;
SELECT join_same($$SELECT jf.v - jdup.w, jdup.t FROM jf JOIN jdup ON jf.fk = jdup.k AND jf.v > jdup.w$$);
SELECT join_same($$SELECT sum(jf.v - jdup.w), count(*) FROM jf JOIN jdup ON jf.fk = jdup.k$$);

-- A side of fewer than 64 rows, an empty side, a join over a join.
SELECT join_same($$SELECT jsmall.s, jd.label FROM jsmall JOIN jd ON jsmall.k = jd.id$$);
SELECT join_same($$SELECT jf.v, jempty.e FROM jf JOIN jempty ON jf.fk = jempty.k$$);
SELECT join_same($$SELECT jempty.e, jd.label FROM jempty JOIN jd ON jempty.k = jd.id$$);
EXPLAIN (COSTS OFF)
SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id JOIN jdup ON jd.id = jdup.k;
SELECT join_same($$SELECT jf.v, jd.label, jdup.w FROM jf JOIN jd ON jf.fk = jd.id JOIN jdup ON jd.id = jdup.k$$);

-- SEMI, ANTI and LEFT: the kinds that keep the outer side, which the node
-- probes with. SEMI returns each outer row with a match once, ANTI those
-- without one (a NULL key has none), LEFT the pairs and then the rows
-- without a pair, their inner columns NULL. A pair counts when it passes
-- the join clauses; a LEFT join's WHERE clause above filters every row.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf WHERE EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk);
SELECT join_same($$SELECT jf.v FROM jf WHERE EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk)$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf WHERE EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk AND jdup.w > jf.v);
SELECT join_same($$SELECT jf.v FROM jf WHERE EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk AND jdup.w > jf.v)$$);
SELECT join_same($$SELECT jf.v FROM jf WHERE jf.fk IN (SELECT k FROM jdup WHERE w > 100)$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf WHERE NOT EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk);
SELECT join_same($$SELECT jf.v, jf.fk FROM jf WHERE NOT EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk)$$);
SELECT join_same($$SELECT jf.v FROM jf WHERE NOT EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk AND jdup.w > jf.v)$$);
SELECT join_same($$SELECT jf.v, jf.fk FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.id IS NULL$$);
EXPLAIN (COSTS OFF) SELECT jf.v, jd.label FROM jf LEFT JOIN jd ON jf.fk = jd.id;
SELECT join_same($$SELECT jf.v, jd.label, jd.n FROM jf LEFT JOIN jd ON jf.fk = jd.id$$);
SELECT join_same($$SELECT jf.v, jdup.w, jdup.t FROM jf LEFT JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_explain($$SELECT count(*), count(jdup.w), sum(jdup.w) FROM jf LEFT JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT count(*), count(jdup.w), sum(jdup.w) FROM jf LEFT JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT jf.v, jdup.w FROM jf LEFT JOIN jdup ON jf.fk = jdup.k AND jdup.w > jf.v$$);
SELECT join_same($$SELECT count(*), count(jdup.w) FROM jf LEFT JOIN jdup ON jf.fk = jdup.k AND jdup.w > jf.v$$);
EXPLAIN (COSTS OFF) SELECT jf.v, jdup.w FROM jf LEFT JOIN jdup ON jf.fk = jdup.k WHERE jdup.w IS NULL OR jdup.w > jf.v;
SELECT join_same($$SELECT jf.v, jdup.w FROM jf LEFT JOIN jdup ON jf.fk = jdup.k WHERE jdup.w IS NULL OR jdup.w > jf.v$$);
SELECT jf.v, jd.label FROM jf LEFT JOIN jd ON jf.fk = jd.id ORDER BY jf.v DESC LIMIT 5;
-- An empty inner side: LEFT and ANTI return every outer row, SEMI none.
SELECT join_same($$SELECT jf.v, jempty.e FROM jf LEFT JOIN jempty ON jf.fk = jempty.k$$);
SELECT join_same($$SELECT count(*) FROM jf WHERE NOT EXISTS (SELECT 1 FROM jempty WHERE jempty.k = jf.fk)$$);
SELECT join_same($$SELECT count(*) FROM jf WHERE EXISTS (SELECT 1 FROM jempty WHERE jempty.k = jf.fk)$$);
-- Rescans with a parameter in the join clauses.
SELECT join_same($$SELECT jsmall.k, (SELECT count(jdup.w) FROM jf LEFT JOIN jdup ON jf.fk = jdup.k AND jdup.w > jsmall.k) FROM jsmall$$);
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jf WHERE NOT EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jf.fk AND jdup.w > jsmall.k)) FROM jsmall$$);

-- A build side larger than the planner thinks: the table grows.
CREATE TABLE jgrow (k int, g int);
ANALYZE jgrow;
INSERT INTO jgrow SELECT g % 400, g FROM generate_series(1, 20000) AS g;
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jsmall JOIN jgrow ON jsmall.k = jgrow.k$$);
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jd JOIN jgrow ON jd.id = jgrow.k$$);

SELECT join_explain($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id$$);
-- Rounds: every match of a key with three records counts.
SELECT join_explain($$SELECT sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
-- An inner side the planner thinks small: the table grows past hash_mem,
-- and is kept and reported, not split.
CREATE FUNCTION join_many() RETURNS SETOF int LANGUAGE sql ROWS 10
AS 'SELECT generate_series(1, 20000)';
SET work_mem = '64kB';
SELECT join_explain($$SELECT count(*) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$);
SELECT join_same($$SELECT count(*), sum(m.k) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$);
RESET work_mem;

-- Rescans: a parameter of the inner side builds the table again, one of
-- the outer side probes the same table.
SELECT join_explain($$SELECT jsmall.k, (SELECT count(*) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jdup.w > jsmall.k) FROM jsmall$$);
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jdup.w > jsmall.k) FROM jsmall$$);
SELECT join_explain($$SELECT jsmall.k, (SELECT count(*) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jf.v > jsmall.k * 10) FROM jsmall$$);
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jf.v > jsmall.k * 10) FROM jsmall$$);
-- A generic plan executed again with another parameter.
SET plan_cache_mode = force_generic_plan;
PREPARE joined(int) AS SELECT count(*), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jf.v > $1;
EXECUTE joined(100);
EXECUTE joined(900);
DEALLOCATE joined;
RESET plan_cache_mode;
SELECT count(*), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k WHERE jf.v > 900;

-- The Bloom filter: a build side of 5000 keys that about 1 % of the probe
-- rows find, so after the first 4096 probe rows the node builds a filter
-- and the rows it rejects skip the table.
CREATE TABLE jbuild AS SELECT g * 100 AS k, g AS w FROM generate_series(1, 5000) AS g;
CREATE TABLE jprobe AS
SELECT CASE WHEN g % 97 = 0 THEN NULL ELSE g END AS k, g AS v FROM generate_series(1, 20000) AS g;
CREATE TABLE jhit AS SELECT (g % 5000 + 1) * 100 AS k, g AS v FROM generate_series(1, 20000) AS g;
ANALYZE jbuild, jprobe, jhit;
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_explain($$SELECT count(*) FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_explain($$SELECT count(*), count(jbuild.w) FROM jprobe LEFT JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe LEFT JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe LEFT JOIN jbuild ON jprobe.k = jbuild.k AND jbuild.w > 100$$);
-- Every probe row finds its key: no filter.
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jhit JOIN jbuild ON jhit.k = jbuild.k$$);
-- A build side that fits in the cache: no filter though most rows miss.
SELECT join_explain($$SELECT count(*) FROM jprobe JOIN jd ON jprobe.k = jd.id$$);
-- A parameter of the inner side builds each table and decides on its filter again.
SELECT join_explain($$SELECT jsmall.k, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w > jsmall.k) FROM jsmall$$);
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w > jsmall.k) FROM jsmall$$);

-- Row-wise parents: a sort, a limit, a scrollable cursor.
EXPLAIN (COSTS OFF)
SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id ORDER BY jf.v DESC LIMIT 5;
SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id ORDER BY jf.v DESC LIMIT 5;
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id LIMIT 1000$$);
-- A scrollable cursor puts Material above the node, which scans forward only.
EXPLAIN (COSTS OFF) DECLARE c SCROLL CURSOR FOR
SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id;
BEGIN;
DECLARE c SCROLL CURSOR FOR
SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id;
FETCH 3 FROM c;
FETCH BACKWARD 2 FROM c;
COMMIT;

-- Under a Gather: the outer side divided among the participants, the
-- inner side built by each. The counters are the participants' totals.
CREATE FUNCTION join_property(query text, name text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == "TessHashJoin").%I', name)::jsonpath)::text;
END $$;
CREATE TABLE jbig AS SELECT g % 350 + 1 AS fk, g AS v FROM generate_series(1, 20000) AS g;
ANALYZE jbig;
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
-- The core's shared table divides the build among the participants and
-- costs less than a table in each; the node's shared table comes later.
SET enable_parallel_hash = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(jd.n) FROM jbig JOIN jd ON jbig.fk = jd.id;
SELECT join_same($$SELECT count(*), sum(jd.n), sum(jbig.v) FROM jbig JOIN jd ON jbig.fk = jd.id$$);
SELECT join_same($$SELECT jbig.v, jd.label FROM jbig JOIN jd ON jbig.fk = jd.id$$);
SELECT join_same($$SELECT count(*), sum(jdup.w) FROM jbig JOIN jdup ON jbig.fk = jdup.k$$);
SELECT join_same($$SELECT count(*), count(jd.n) FROM jbig LEFT JOIN jd ON jbig.fk = jd.id$$);
SELECT join_same($$SELECT count(*) FROM jbig WHERE EXISTS (SELECT 1 FROM jdup WHERE jdup.k = jbig.fk)$$);
SELECT join_same($$SELECT count(*), sum(jbig.v) FROM jbig WHERE NOT EXISTS (SELECT 1 FROM jd WHERE jd.id = jbig.fk)$$);
SELECT join_property($$SELECT count(*) FROM jbig JOIN jd ON jbig.fk = jd.id$$, 'Probe Rows') AS probe_rows,
       join_property($$SELECT count(*) FROM jbig JOIN jd ON jbig.fk = jd.id$$, 'Matches') AS matches;
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(jd.n), sum(jbig.v) FROM jbig JOIN jd ON jbig.fk = jd.id$$);
RESET parallel_leader_participation;
-- Each participant decides on a filter of its own table by its own rows.
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT count(*), sum(jprobe.v) FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Bloom Filters')::int >= 1 AS filtered;
-- A shared table: where the core may use a Parallel Hash, the inner
-- side's partial path divides the build among the participants into one
-- table in the query's shared memory.
SET enable_parallel_hash = on;
EXPLAIN (COSTS OFF) SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k;
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT count(*), sum(jprobe.v) FROM jprobe WHERE EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT count(*), sum(jprobe.v) FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT count(*), count(jbuild.w), sum(jprobe.v) FROM jprobe LEFT JOIN jbuild ON jprobe.k = jbuild.k$$);
-- Duplicate keys: the next record of a key is found down its chain.
EXPLAIN (COSTS OFF) SELECT count(*), sum(jgrow.g) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k;
SELECT join_same($$SELECT count(*), sum(jgrow.g), sum(jbig.v) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$);
SELECT join_same($$SELECT jbig.v, jgrow.g FROM jbig JOIN jgrow ON jbig.fk = jgrow.k WHERE jbig.v < 200$$);
SELECT join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Builds') AS builds,
       join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Table Grows') AS grows,
       join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Build Rows') AS build_rows;
SELECT join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Overflow Rows') AS overflow,
       join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Buckets') AS buckets;
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
RESET parallel_leader_participation;
-- One participant builds the shared Bloom filter for all; the others use
-- it once it is ready.
SELECT join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Bloom Filters') AS filters,
       join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Shared Table') AS shared;
SELECT join_same($$SELECT count(*), sum(jprobe.v) FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
-- A by-reference inner column: a table in each participant.
EXPLAIN (COSTS OFF) SELECT count(*), max(jd.label) FROM jbig JOIN jd ON jbig.fk = jd.id;
-- A rescan of the Gather builds the shared table anew.
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w > jsmall.k * 100) FROM jsmall$$);
RESET enable_parallel_hash;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET enable_parallel_hash;

-- No path: another join type, clauses without an integer key, another key type, the
-- core's hash join disabled, the batch nodes off.
EXPLAIN (COSTS OFF) SELECT count(jd.id) FROM jf FULL JOIN jd ON jf.fk = jd.id;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.v > jd.n AND jf.note = jd.label;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.note = jd.label;
SET enable_hashjoin = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET enable_hashjoin;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET tessera.enable;

DROP TABLE jd, jf, jdup, jsmall, jempty, jgrow, jbig, jpair, jbuild, jprobe, jhit;
DROP FUNCTION join_property(text, text);
DROP FUNCTION join_explain(text);
DROP FUNCTION join_many();
DROP FUNCTION join_same(text);
DROP EXTENSION tessera;
