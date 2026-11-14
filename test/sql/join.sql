CREATE EXTENSION tessera;
LOAD 'tessera_nodes';

-- The rows of a query with the batch nodes and without them, sorted. With
-- core_parallel_hash off the core answers without its shared hash table:
-- on a key most inner rows share it splits its batches without end (a
-- minute a query), where its serial hash join takes a tenth of a second.
CREATE FUNCTION join_same(query text, core_parallel_hash boolean DEFAULT true)
RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text[];
    without_tessera text[];
    wrapped text := format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    IF NOT core_parallel_hash THEN
        PERFORM set_config('enable_parallel_hash', 'off', true);
    END IF;
    EXECUTE wrapped INTO without_tessera;
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('MISMATCH on %s off %s', with_tessera, without_tessera);
    END IF;
    RETURN coalesce(array_length(with_tessera, 1), 0) || ' rows, hash ' ||
           md5(coalesce(with_tessera::text, ''));
END $$;

-- EXPLAIN ANALYZE with the memory as whether it stayed within hash_mem:
-- its bytes depend on the allocator, and an assert build's differ. With
-- VERBOSE, unless asked otherwise: the nodes' counters show only with it.
CREATE FUNCTION join_explain(query text, verbosely boolean DEFAULT true) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
    limit_kb bigint := (SELECT setting::bigint FROM pg_settings WHERE name = 'work_mem') *
                       current_setting('hash_mem_multiplier')::float8;
BEGIN
    FOR line IN EXECUTE format('EXPLAIN (ANALYZE, VERBOSE %s, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) %s',
                               CASE WHEN verbosely THEN 'on' ELSE 'off' END, query) LOOP
        -- VERBOSE adds the output lists and each worker's rows: not checked here.
        CONTINUE WHEN line ~ '^\s*(Output|Worker \d+):';
        -- Overrun repeats what the memory line says.
        CONTINUE WHEN line ~ 'Overrun: \d+ kB';
        IF line ~ 'Memory Usage: \d+ kB' THEN
            line := regexp_replace(line, 'Memory Usage: \d+ kB',
                CASE WHEN substring(line FROM 'Memory Usage: (\d+) kB')::bigint <= limit_kb
                     THEN 'Memory Usage: within hash_mem' ELSE 'Memory Usage: over hash_mem' END);
        END IF;
        RETURN NEXT line;
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
-- Without VERBOSE each node shows what the core's node in its place
-- shows: the join its buckets and memory, the scans and the aggregate
-- nothing past their rows.
SELECT join_explain($$SELECT count(jf.m), sum(jf.m), sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$, false);
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
-- The residual clauses in the core's order, by cost: the guard, row-wise
-- (a bitwise xor), before the division in batches, however they are
-- written; without the guard the division fails.
CREATE TABLE jguard AS
SELECT g AS k, CASE WHEN g % 4 = 0 THEN g ELSE g % 3 END AS d FROM generate_series(1, 300) AS g;
ANALYZE jguard;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jguard JOIN jd ON jguard.k = jd.id AND 1000 / (jd.id - jguard.d) > 5 AND (jd.id # jguard.d) <> 0;
SELECT join_same($$SELECT jguard.d, jd.id FROM jguard JOIN jd ON jguard.k = jd.id AND 1000 / (jd.id - jguard.d) > 5 AND (jd.id # jguard.d) <> 0$$);
SELECT join_same($$SELECT jguard.d, jd.id FROM jguard JOIN jd ON jguard.k = jd.id AND (jd.id # jguard.d) <> 0 AND 1000 / (jd.id - jguard.d) > 5$$);
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
-- A filter over an inner column no parent asks for: the join keeps it for
-- the filter alone.
SELECT join_same($$SELECT jf.v FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.n IS NULL$$);
SELECT join_same($$SELECT count(*) FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.n IS NULL$$);
-- An outer join whose inner key is tested for NULL above it is planned as
-- an anti join, and its target keeps the inner columns: NULL in every
-- row, as no row has a pair. Before, the join read them from the records
-- of earlier rows: other rows' values, or an internal error.
EXPLAIN (COSTS OFF) SELECT * FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.id IS NULL;
SELECT join_same($$SELECT * FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.id IS NULL$$);
SELECT join_same($$SELECT count(*), count(jd.label), count(jd.n) FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.id IS NULL$$);
SELECT join_same($$SELECT jf.v, jdup.w, jdup.t FROM jf LEFT JOIN jdup ON jf.fk = jdup.k AND jdup.w > jf.v WHERE jdup.k IS NULL$$);
SELECT join_same($$SELECT jf.v, jd.n FROM jf LEFT JOIN jd ON jf.fk = jd.id WHERE jd.id IS NULL AND coalesce(jd.n, 0) = 0$$);
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

-- A build side larger than the planner thinks: the records go on into
-- more chunks, and the index is made for as many as were read.
-- No autovacuum: its analyze would correct the planner's estimate.
CREATE TABLE jgrow (k int, g int) WITH (autovacuum_enabled = off);
ANALYZE jgrow;
INSERT INTO jgrow SELECT g % 400, g FROM generate_series(1, 20000) AS g;
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jsmall JOIN jgrow ON jsmall.k = jgrow.k$$);
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jd JOIN jgrow ON jd.id = jgrow.k$$);

SELECT join_explain($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id$$);
-- Rounds: every match of a key with three records counts.
SELECT join_explain($$SELECT sum(jdup.w) FROM jf JOIN jdup ON jf.fk = jdup.k$$);
-- An inner side the planner thinks small: the table goes past hash_mem
-- and spills; at a hash_mem this small the tails of its partitions do
-- not fit either, and the memory goes over it.
CREATE FUNCTION join_many() RETURNS SETOF int LANGUAGE sql ROWS 10
AS 'SELECT generate_series(1, 20000)';
SET work_mem = '64kB';
SELECT join_explain($$SELECT count(*) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$);
SELECT join_same($$SELECT count(*), sum(m.k) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$);
-- With a room of ten times what hash_mem leaves in the weights of a split
-- (docs/spill.md, "The weights"), no partition splits: each is joined in
-- pieces.
SET tessera.join_spill_split_room = 10;
SELECT count(*) AS splits
FROM join_explain($$SELECT count(*) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$) AS line
WHERE line ~ 'Split Partitions';
SELECT join_same($$SELECT count(*), sum(m.k) FROM jf JOIN join_many() AS m(k) ON jf.v = m.k$$);
RESET tessera.join_spill_split_room;
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
-- No autovacuum: these tables end in empty pages. It would cut them off
-- and, reading only part of jprobe and jhit, count a third fewer rows, and
-- the plans of the anti joins below would change.
CREATE TABLE jbuild WITH (autovacuum_enabled = off) AS
SELECT g * 100 AS k, g AS w FROM generate_series(1, 5000) AS g;
CREATE TABLE jprobe WITH (autovacuum_enabled = off) AS
SELECT CASE WHEN g % 97 = 0 THEN NULL ELSE g END AS k, g AS v FROM generate_series(1, 20000) AS g;
CREATE TABLE jhit WITH (autovacuum_enabled = off) AS
SELECT (g % 5000 + 1) * 100 AS k, g AS v FROM generate_series(1, 20000) AS g;
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
-- The filter below the join: the outer side's TessFilter checks its rows
-- against it after its batch clause and before its row-wise one, for
-- INNER and SEMI, where a row without a pair leaves; ANTI and LEFT keep
-- theirs, and check in the join. The TessFilter counts the rows with a
-- key its filter rejected as the join does, and those it removed for a
-- NULL key apart: with its clauses' they are the core's Rows Removed by
-- Filter.
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%' AND EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_explain($$SELECT count(*) FROM jprobe WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%' AND NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%' AND NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe LEFT JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
-- Each table built for a parameter takes its filter back before it goes.
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w > jsmall.k AND jprobe.v > 0 AND jprobe.v::text LIKE '%1%') FROM jsmall$$);
-- Only row-wise clauses: TessFilter takes them from the core scan, as the
-- relation's own path, so that for an inner or semi join the filter below
-- reaches them; an anti join reads the same node without it.
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE jprobe.v::text LIKE '%1%' AND EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jprobe WHERE jprobe.v::text LIKE '%1%' AND NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k);
SELECT join_same($$SELECT jprobe.v FROM jprobe WHERE jprobe.v::text LIKE '%1%' AND NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
-- tessera.join_bloom_ratio: at 1 a filter at once, whatever the sizes; at
-- 0 none.
SET tessera.join_bloom_ratio = 1;
SELECT join_explain($$SELECT count(*), sum(jd.n) FROM jf JOIN jd ON jf.fk = jd.id WHERE jf.v > 0 AND jf.note LIKE 'f1%'$$);
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk = jd.id WHERE jf.v > 0 AND jf.note LIKE 'f1%'$$);
SET tessera.join_bloom_ratio = 0;
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
RESET tessera.join_bloom_ratio;
-- A RIGHT join drops an outer row without a pair as an INNER join does,
-- and hands its filter down too; the subquery keeps the clauses on its
-- side, which a WHERE on the join would turn into an INNER join.
SELECT join_explain($$SELECT count(*), count(p.v), sum(jbuild.w) FROM (SELECT * FROM jprobe WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%') AS p RIGHT JOIN jbuild ON p.k = jbuild.k$$);
SELECT join_same($$SELECT p.v, jbuild.w FROM (SELECT * FROM jprobe WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%') AS p RIGHT JOIN jbuild ON p.k = jbuild.k$$);
-- A RIGHT ANTI join drops an outer row without a pair as well, and hands
-- its filter down.
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k AND jprobe.v > 0 AND jprobe.v::text LIKE '%1%')$$);
SELECT join_same($$SELECT jbuild.k, jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k AND jprobe.v > 0 AND jprobe.v::text LIKE '%1%')$$);
-- Batch clauses after the filter: a TessFilter of batch clauses alone
-- takes the filter and checks it before them, so that its clauses on v,
-- the column after the key, see only the rows it passed.
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v < 19000$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v < 19000$$);
-- A filter that keeps more rows than the clauses: jmove's keys come in
-- runs of 2000 in the table and 3000 not, and three in ten rows pass
-- v % 10 < 3. Once each has seen 4096 rows, the filter goes behind the
-- clauses, which then read every row, and checks only the rows they
-- kept: it removes far fewer rows than it would first all along.
-- No autovacuum, for the reason given at jbuild.
CREATE TABLE jmove WITH (autovacuum_enabled = off) AS SELECT (g % 5000 + 1) * 100 AS k, g AS v FROM generate_series(1, 60000) AS g;
ANALYZE jmove;
SELECT line FROM join_explain($$SELECT count(*) FROM jmove JOIN (SELECT * FROM jbuild WHERE w <= 2000) AS b ON jmove.k = b.k WHERE jmove.v % 10 < 3$$) AS line
WHERE line ~ 'Bloom Filter Below|Rows Removed by (Batch|Bloom) Filter';
SELECT join_same($$SELECT jmove.v, b.w FROM jmove JOIN (SELECT * FROM jbuild WHERE w <= 2000) AS b ON jmove.k = b.k WHERE jmove.v % 10 < 3$$);
-- The shares taken again in every window: the clauses keep three rows
-- in ten of the first 30000 and nine in ten after, so the filter goes
-- behind them, then first again; first, it spares the clauses most rows
-- of the second half, which then remove far fewer than the 3000 they
-- would remove reading them all.
SELECT line FROM join_explain($$SELECT count(*) FROM jmove JOIN (SELECT * FROM jbuild WHERE w <= 2000) AS b ON jmove.k = b.k WHERE (jmove.v <= 30000 AND jmove.v % 10 < 3) OR (jmove.v > 30000 AND jmove.v % 10 < 9)$$) AS line
WHERE line ~ 'Bloom Filter Below|Rows Removed by (Batch|Bloom) Filter';
SELECT join_same($$SELECT jmove.v, b.w FROM jmove JOIN (SELECT * FROM jbuild WHERE w <= 2000) AS b ON jmove.k = b.k WHERE (jmove.v <= 30000 AND jmove.v % 10 < 3) OR (jmove.v > 30000 AND jmove.v % 10 < 9)$$);
-- A table built anew for each value of a parameter, with keys apart
-- from the last one's: each takes the filter of the table before back
-- and hands its own down to the TessFilter of batch clauses alone.
SELECT line FROM join_explain($$SELECT v.x, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w BETWEEN v.x AND v.x + 999 AND jprobe.v < 19000) FROM (VALUES (1), (2001), (4001)) AS v(x)$$) AS line
WHERE line ~ 'Builds|Bloom Filter';
SELECT join_same($$SELECT v.x, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w BETWEEN v.x AND v.x + 999 AND jprobe.v < 19000) FROM (VALUES (1), (2001), (4001)) AS v(x)$$);
-- A small table's filter: a table of fewer than 4096 records gets one
-- only for the outer child, which takes it here; a key the child
-- computes is refused, and the join then checks no filter.
SELECT line FROM join_explain($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k WHERE jprobe.v < 19000$$) AS line
WHERE line ~ 'TessHashJoin|Build Rows|Bloom|TessFilter';
SELECT join_same($$SELECT jprobe.v, b.w FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k WHERE jprobe.v < 19000$$);
SELECT line FROM join_explain($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$) AS line
WHERE line ~ 'TessHashJoin|Build Rows|Bloom|TessFilter';
SELECT join_same($$SELECT jprobe.v, b.w FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$);
-- A table that spills keeps its filter: it knows only the rows in memory.
SET work_mem = '64kB';
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
RESET work_mem;
-- A key hashed from a text value: the TessFilter has the value, not the
-- hash the filter holds, and the join keeps its filter.
SET tessera.join_bloom_ratio = 1;
SELECT join_explain($$SELECT count(*) FROM jf JOIN jd ON jf.note = jd.label WHERE jf.v > 0 AND jf.note ~ '^f'$$);
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.note = jd.label WHERE jf.v > 0 AND jf.note ~ '^f'$$);
RESET tessera.join_bloom_ratio;
-- A partitioned outer side: TessAppend hands the filter to every partition
-- in the partition's own columns (the second's columns in another order),
-- and takes it only when every one does, since the join checks no more
-- rows once a node below took it.
CREATE TABLE jpprobe (v int, k int) PARTITION BY RANGE (v);
CREATE TABLE jpprobe_1 PARTITION OF jpprobe FOR VALUES FROM (1) TO (10001);
CREATE TABLE jpprobe_2 (k int, v int);
ALTER TABLE jpprobe ATTACH PARTITION jpprobe_2 FOR VALUES FROM (10001) TO (20001);
INSERT INTO jpprobe SELECT g, CASE WHEN g % 97 = 0 THEN NULL ELSE g * 37 % 20011 END FROM generate_series(1, 20000) AS g;
ANALYZE jpprobe;
SELECT join_explain($$SELECT count(*), sum(jbuild.w) FROM jpprobe JOIN jbuild ON jpprobe.k = jbuild.k WHERE jpprobe.v > 0 AND jpprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jpprobe.v, jbuild.w FROM jpprobe JOIN jbuild ON jpprobe.k = jbuild.k WHERE jpprobe.v > 0 AND jpprobe.v::text LIKE '%1%'$$);
SELECT join_same($$SELECT jpprobe.v FROM jpprobe WHERE jpprobe.v > 0 AND jpprobe.v::text LIKE '%1%' AND EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jpprobe.k)$$);
DROP TABLE jpprobe;

-- Pruning by the join's keys: an outer side partitioned by the join key,
-- whose partitions the inner side's keys cannot pair with are pruned once
-- the table is built, before the outer side is read (Subplans Removed by
-- Join, the others never executed). A short inner side lists its keys,
-- each pruning by itself; past 1024 rows with a key the lowest and the
-- highest prune as a range, which a hash-partitioned side has none of.
CREATE FUNCTION join_pruned(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        '$[0]."Plan".** ? (@."Custom Plan Provider" == "TessAppend")."Subplans Removed by Join"')::text;
END $$;
-- The partitions of jpr no execution read.
CREATE FUNCTION join_unread(query text) RETURNS bigint
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN (SELECT count(DISTINCT node ->> 'Relation Name')
            FROM jsonb_path_query(plan, 'strict $[0]."Plan".**') AS node
            WHERE node ->> 'Relation Name' LIKE 'jpr%' AND (node ->> 'Actual Loops')::numeric = 0);
END $$;
CREATE TABLE jpr (k int, v int) PARTITION BY RANGE (k);
CREATE TABLE jpr_1 PARTITION OF jpr FOR VALUES FROM (1) TO (10001);
CREATE TABLE jpr_2 PARTITION OF jpr FOR VALUES FROM (10001) TO (20001);
CREATE TABLE jpr_3 PARTITION OF jpr FOR VALUES FROM (20001) TO (30001);
CREATE TABLE jpr_d PARTITION OF jpr DEFAULT;
INSERT INTO jpr SELECT g, g % 7 FROM generate_series(1, 32000) AS g;
INSERT INTO jpr VALUES (NULL, 1);
CREATE TABLE jpl (k int, v int) PARTITION BY LIST (k);
CREATE TABLE jpl_1 PARTITION OF jpl FOR VALUES IN (1, 2, 3);
CREATE TABLE jpl_2 PARTITION OF jpl FOR VALUES IN (4, 5, 6);
CREATE TABLE jpl_3 PARTITION OF jpl FOR VALUES IN (7, 8, 9);
INSERT INTO jpl SELECT g % 9 + 1, g FROM generate_series(1, 9000) AS g;
CREATE TABLE jph (k int, v int) PARTITION BY HASH (k);
CREATE TABLE jph_0 PARTITION OF jph FOR VALUES WITH (MODULUS 4, REMAINDER 0);
CREATE TABLE jph_1 PARTITION OF jph FOR VALUES WITH (MODULUS 4, REMAINDER 1);
CREATE TABLE jph_2 PARTITION OF jph FOR VALUES WITH (MODULUS 4, REMAINDER 2);
CREATE TABLE jph_3 PARTITION OF jph FOR VALUES WITH (MODULUS 4, REMAINDER 3);
INSERT INTO jph SELECT g, g % 7 FROM generate_series(1, 20000) AS g;
-- Two levels: by v, then by the join key k.
CREATE TABLE jp2 (k int, v int) PARTITION BY RANGE (v);
CREATE TABLE jp2_a PARTITION OF jp2 FOR VALUES FROM (0) TO (4) PARTITION BY RANGE (k);
CREATE TABLE jp2_a1 PARTITION OF jp2_a FOR VALUES FROM (1) TO (10001);
CREATE TABLE jp2_a2 PARTITION OF jp2_a FOR VALUES FROM (10001) TO (20001);
CREATE TABLE jp2_b PARTITION OF jp2 FOR VALUES FROM (4) TO (7) PARTITION BY RANGE (k);
CREATE TABLE jp2_b1 PARTITION OF jp2_b FOR VALUES FROM (1) TO (10001);
CREATE TABLE jp2_b2 PARTITION OF jp2_b FOR VALUES FROM (10001) TO (20001);
INSERT INTO jp2 SELECT g, g % 7 FROM generate_series(1, 20000) AS g;
CREATE TABLE jkf AS SELECT g AS k, g % 5 AS w FROM generate_series(10001, 10500) AS g;
INSERT INTO jkf VALUES (NULL, 1), (NULL, 2);
CREATE TABLE jkm AS SELECT g AS k, g % 5 AS w FROM generate_series(20001, 22000) AS g;
CREATE TABLE jk8 AS SELECT g::bigint AS k FROM generate_series(10001, 10300) AS g;
CREATE TABLE jkn (k int, w int);
INSERT INTO jkn VALUES (NULL, 1), (NULL, 2);
CREATE TABLE jkl AS SELECT g AS k FROM generate_series(4, 5) AS g;
CREATE TABLE jk2 AS SELECT k FROM (VALUES (5), (15005)) AS v(k);
CREATE TABLE jk19 AS SELECT k FROM (VALUES (1), (9)) AS v(k);
CREATE TABLE jkw AS SELECT g AS k FROM generate_series(20001, 30000) AS g;
ANALYZE jpr, jpl, jph, jp2, jkf, jkm, jk8, jkn, jkl, jk2, jk19, jkw;
SELECT join_explain($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkf ON jpr.k = jkf.k$$);
SELECT join_same($$SELECT jpr.k, jpr.v, jkf.w FROM jpr JOIN jkf ON jpr.k = jkf.k$$);
-- By the range of 2000 keys; by an int8 key; by two keys of two
-- partitions, which add up; a list partitioning by keys at its two ends,
-- which a range would not prune; a hash one by the listed keys and past
-- them; the second level of two.
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS range,
       join_pruned($$SELECT count(*) FROM jpr JOIN jk8 ON jpr.k = jk8.k$$) AS int8,
       join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jk2 ON jpr.k = jk2.k$$) AS two_keys,
       join_pruned($$SELECT count(*), sum(jpl.v) FROM jpl JOIN jk19 ON jpl.k = jk19.k$$) AS list,
       join_pruned($$SELECT count(*), sum(jph.v) FROM jph JOIN jkl ON jph.k = jkl.k$$) AS hash_listed,
       join_pruned($$SELECT count(*), sum(jph.v) FROM jph JOIN jkm ON jph.k = jkm.k$$) AS hash_range,
       join_pruned($$SELECT count(*), sum(jp2.v) FROM jp2 JOIN jkf ON jp2.k = jkf.k$$) AS two_levels;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkm ON jpr.k = jkm.k$$);
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jk8 ON jpr.k = jk8.k$$);
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jk2 ON jpr.k = jk2.k$$);
SELECT join_same($$SELECT jpl.k, jpl.v FROM jpl JOIN jk19 ON jpl.k = jk19.k$$);
SELECT join_same($$SELECT jph.k, jph.v FROM jph JOIN jkl ON jph.k = jkl.k$$);
SELECT join_same($$SELECT jp2.k, jp2.v FROM jp2 JOIN jkf ON jp2.k = jkf.k$$);
-- The list's bound: 1024 rows with a key of jpr_1 or of jpr_3 are a list,
-- which keeps those two partitions; at 1025 rows the range from the
-- lowest to the highest keeps jpr_2 between them too.
CREATE TABLE jk1024 AS SELECT CASE WHEN g % 2 = 0 THEN 5 ELSE 25005 END AS k FROM generate_series(1, 1024) AS g;
CREATE TABLE jk1025 AS SELECT CASE WHEN g % 2 = 0 THEN 5 ELSE 25005 END AS k FROM generate_series(1, 1025) AS g;
ANALYZE jk1024, jk1025;
SELECT join_pruned($$SELECT count(*) FROM jpr JOIN jk1024 ON jpr.k = jk1024.k$$) AS list_1024,
       join_pruned($$SELECT count(*) FROM jpr JOIN jk1025 ON jpr.k = jk1025.k$$) AS range_1025;
SELECT join_same($$SELECT jpr.k, count(*) FROM jpr JOIN jk1024 ON jpr.k = jk1024.k GROUP BY jpr.k$$);
SELECT join_same($$SELECT jpr.k, count(*) FROM jpr JOIN jk1025 ON jpr.k = jk1025.k GROUP BY jpr.k$$);
-- The key that prunes: the first that can, here the second key; a key
-- that is the partition key's second column cannot; nor can any with
-- enable_partition_pruning off.
CREATE TABLE jpk (k int, v int) PARTITION BY RANGE (v, k);
CREATE TABLE jpk_1 PARTITION OF jpk FOR VALUES FROM (0, 1) TO (3, 1);
CREATE TABLE jpk_2 PARTITION OF jpk FOR VALUES FROM (3, 1) TO (7, 1);
INSERT INTO jpk SELECT g, g % 7 FROM generate_series(1, 20000) AS g;
ANALYZE jpk;
SELECT join_pruned($$SELECT count(*) FROM jpr JOIN jkf ON jpr.v = jkf.w AND jpr.k = jkf.k$$) AS second_key,
       join_pruned($$SELECT count(*) FROM jpk JOIN jkf ON jpk.k = jkf.k$$) AS second_column;
SET enable_partition_pruning = off;
SELECT join_pruned($$SELECT count(*) FROM jpr JOIN jkf ON jpr.k = jkf.k$$) AS pruning_off;
RESET enable_partition_pruning;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkf ON jpr.v = jkf.w AND jpr.k = jkf.k$$);
SELECT join_same($$SELECT jpk.k, jpk.v FROM jpk JOIN jkf ON jpk.k = jkf.k$$);
-- With the node's own pruning: the keys reach jpr_1 and jpr_2, an
-- initplan's value leaves jpr_2 of them, and only jpr_2 is read.
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jk2 ON jpr.k = jk2.k WHERE jpr.k > (SELECT 12000)$$) AS by_join,
       join_unread($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jk2 ON jpr.k = jk2.k WHERE jpr.k > (SELECT 12000)$$) AS unread;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jk2 ON jpr.k = jk2.k WHERE jpr.k > (SELECT 12000)$$);
-- SEMI and RIGHT keep no outer row without a pair and prune; LEFT and ANTI
-- keep every outer row and do not. Keys that are all NULL pair with no
-- partition.
SELECT join_pruned($$SELECT count(*) FROM jpr WHERE EXISTS (SELECT 1 FROM jkf WHERE jkf.k = jpr.k)$$) AS semi,
       join_pruned($$SELECT count(*), count(jpr.v) FROM jpr RIGHT JOIN jkf ON jpr.k = jkf.k$$) AS right_join,
       join_pruned($$SELECT count(*), count(jkf.w) FROM jpr LEFT JOIN jkf ON jpr.k = jkf.k$$) AS left_join,
       join_pruned($$SELECT count(*) FROM jpr WHERE NOT EXISTS (SELECT 1 FROM jkf WHERE jkf.k = jpr.k)$$) AS anti,
       join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkn ON jpr.k = jkn.k$$) AS all_null,
       join_pruned($$SELECT count(*), count(jpr.v) FROM jpr RIGHT JOIN jkn ON jpr.k = jkn.k$$) AS right_all_null;
SELECT join_same($$SELECT jpr.k, jkn.k, jkn.w FROM jpr RIGHT JOIN jkn ON jpr.k = jkn.k$$);
SELECT join_same($$SELECT jpr.k FROM jpr WHERE EXISTS (SELECT 1 FROM jkf WHERE jkf.k = jpr.k)$$);
SELECT join_same($$SELECT jpr.k, jkf.k, jkf.w FROM jpr RIGHT JOIN jkf ON jpr.k = jkf.k$$);
SELECT join_same($$SELECT jpr.k, jkf.w FROM jpr LEFT JOIN jkf ON jpr.k = jkf.k$$);
SELECT join_same($$SELECT jpr.k FROM jpr WHERE NOT EXISTS (SELECT 1 FROM jkf WHERE jkf.k = jpr.k)$$);
-- RIGHT ANTI keeps the inner rows without a pair, not the outer ones,
-- and prunes as RIGHT does.
SELECT line FROM join_explain($$SELECT count(*) FROM jkf WHERE NOT EXISTS (SELECT 1 FROM jpr WHERE jpr.k = jkf.k)$$, false) AS line
WHERE line ~ 'Join Type';
SELECT join_pruned($$SELECT count(*) FROM jkf WHERE NOT EXISTS (SELECT 1 FROM jpr WHERE jpr.k = jkf.k)$$) AS right_anti;
SELECT join_same($$SELECT jkf.k, jkf.w FROM jkf WHERE NOT EXISTS (SELECT 1 FROM jpr WHERE jpr.k = jkf.k)$$);
-- A rescan: a table kept for the outer side's new parameter prunes again
-- by the same keys, which go down anew; a table built anew for the inner
-- side's parameter by its own: jkr's keys of w = 4 reach jpr_1, those of
-- w = 5 jpr_3, and each loop reads its partition alone.
CREATE TABLE jkr AS SELECT CASE WHEN g <= 100 THEN g ELSE 20000 + g END AS k,
                           CASE WHEN g <= 100 THEN 4 ELSE 5 END AS w
                    FROM generate_series(1, 200) AS g;
ANALYZE jkr;
SELECT join_unread($$SELECT jkl.k, (SELECT count(*) FROM jpr JOIN jkf ON jpr.k = jkf.k WHERE jpr.v = jkl.k) FROM jkl$$) AS kept,
       join_unread($$SELECT jkl.k, (SELECT count(*) FROM jpr JOIN jkr ON jpr.k = jkr.k WHERE jkr.w = jkl.k) FROM jkl$$) AS rebuilt;
SELECT join_same($$SELECT jkl.k, (SELECT count(*) FROM jpr JOIN jkf ON jpr.k = jkf.k WHERE jpr.v = jkl.k) FROM jkl$$);
SELECT join_same($$SELECT jkl.k, (SELECT sum(jpr.k) FROM jpr JOIN jkr ON jpr.k = jkr.k WHERE jkr.w = jkl.k) FROM jkl$$);
-- The planner expects the pruning. A side larger than jpr whose keys
-- reach jpr_3 alone is built, and jpr, pruned to that partition, probes
-- it: without the expectation the smaller jpr would be built and nothing
-- pruned, as for a side whose keys reach every partition. The bounds come
-- from the inner side's statistics and from its clauses on the key,
-- carried over to the outer key (the core carries no inequality), and
-- prune at the second level of jp2 too; a hash partitioning expects none,
-- and jph is built. join_outer: the relation of the join's outer child,
-- or of an Append's first child.
CREATE FUNCTION join_outer(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (FORMAT JSON, COSTS OFF) %s', query) INTO plan;
    RETURN (SELECT coalesce(child ->> 'Relation Name', child #>> '{Plans,0,Relation Name}')
            FROM jsonb_path_query_first(plan,
                '$[0]."Plan".** ? (@."Custom Plan Provider" == "TessHashJoin")."Plans"[0]') AS child);
END $$;
-- No autovacuum, for the reason given at jbuild.
CREATE TABLE jkx WITH (autovacuum_enabled = off) AS SELECT 20001 + g % 10000 AS k, 10001 + g % 10000 AS k2 FROM generate_series(1, 50000) AS g;
CREATE TABLE jkwide AS SELECT 1 + g % 32000 AS k FROM generate_series(1, 100000) AS g;
-- Keys of jp2's second level, few enough that building them costs less
-- than reading the partitions pruned (building jkx's 50 000 rows would
-- not: the smaller jp2 is built, and nothing pruned).
CREATE TABLE jkx2 AS SELECT 10001 + g % 10000 AS k2 FROM generate_series(1, 5000) AS g;
-- The same for jpr's third partition, for the parallel join below.
CREATE TABLE jkp AS SELECT 20001 + g % 10000 AS k FROM generate_series(1, 5000) AS g;
ANALYZE jkx, jkwide, jkx2, jkp;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jpr JOIN jkx ON jpr.k = jkx.k;
SELECT join_outer($$SELECT count(*) FROM jpr JOIN jkwide ON jpr.k = jkwide.k$$) AS every_partition,
       join_outer($$SELECT count(*) FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE jkwide.k > 20000$$) AS clause,
       join_outer($$SELECT count(*) FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE 20000 < jkwide.k$$) AS commuted,
       join_outer($$SELECT count(*) FROM jp2 JOIN jkx2 ON jp2.k = jkx2.k2$$) AS second_level,
       join_outer($$SELECT count(*) FROM jph JOIN jkx ON jph.k = jkx.k2$$) AS hash;
SELECT join_pruned($$SELECT count(*) FROM jpr JOIN jkx ON jpr.k = jkx.k$$) AS statistics,
       join_pruned($$SELECT count(*) FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE jkwide.k > 20000$$) AS clause,
       join_pruned($$SELECT count(*) FROM jp2 JOIN jkx2 ON jp2.k = jkx2.k2$$) AS second_level;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkx ON jpr.k = jkx.k$$);
SELECT join_same($$SELECT jpr.k, jkwide.k FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE jkwide.k > 20000$$);
SELECT join_same($$SELECT jp2.k, jp2.v FROM jp2 JOIN jkx ON jp2.k = jkx.k2$$);
-- A table that spills keeps its range.
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS spilled,
       EXISTS (SELECT FROM join_explain($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS line
               WHERE line ~ 'Spilled Chunks: [1-9]') AS on_disk;
SELECT join_same($$SELECT jpr.k, jpr.v, jkm.w FROM jpr JOIN jkm ON jpr.k = jkm.k$$);
-- A partition on disk chosen again: weighed ten times, its tail holds
-- the most, and the join writes and frees it rather than asking the rule
-- again without end. The timeout ends the query should it still loop.
SET tessera.join_spill_spilled_weight = 10;
SET statement_timeout = '60s';
SELECT join_same($$SELECT jpr.k, jpr.v, jkm.w FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS chosen_again;
RESET statement_timeout;
RESET tessera.join_spill_spilled_weight;
RESET hash_mem_multiplier;
RESET work_mem;

-- Spilling: an inner side of about 3 MB with duplicates, NULL keys and
-- text of 1 to 60 bytes, past a hash_mem of 1 MB. The first partitions
-- stay in memory, the others go to disk with the outer rows of theirs,
-- which pass the Bloom filter of every inner row first, and are joined
-- after the outer side; text of both sides comes back from disk.
CREATE TABLE jsb (k int, t text, n bigint);
INSERT INTO jsb
SELECT CASE WHEN g % 17 = 0 THEN NULL ELSE g % 15000 END,
       CASE WHEN g % 7 = 0 THEN NULL ELSE repeat('b', g % 60) || g END, g
FROM generate_series(1, 40000) AS g;
CREATE TABLE jsp (k int, s text);
INSERT INTO jsp
SELECT CASE WHEN g % 11 = 0 THEN NULL ELSE g % 20000 END, 'p' || g
FROM generate_series(1, 100000) AS g;
-- One key held by 60000 inner rows the planner expects 10 of: its
-- partition, larger than hash_mem, cannot split, and is joined in pieces,
-- its outer rows read once per piece.
CREATE FUNCTION jskew() RETURNS TABLE (k int, w int) LANGUAGE sql ROWS 10
AS 'SELECT CASE WHEN g <= 60000 THEN 7 ELSE g END, g FROM generate_series(1, 62000) AS g';
ANALYZE jsb, jsp;
SET work_mem = '512kB';
SELECT join_explain($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), sum(length(jsb.t)), sum(jsp.k), sum(jsb.n) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
-- The weights of the rule that sends partitions to disk (docs/spill.md,
-- "Weights"): without the share of the inner rows below which those in
-- memory all go, some stay; without the outer tails' reserve, fewer go.
SET tessera.join_spill_resident_share = 0;
SELECT join_explain($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SET tessera.join_spill_tail_weight = 0;
SELECT join_explain($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), sum(length(jsb.t)), sum(jsp.k), sum(jsb.n) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
RESET tessera.join_spill_tail_weight;
RESET tessera.join_spill_resident_share;
SELECT join_same($$SELECT jsp.s, jsb.t, jsb.n FROM jsp JOIN jsb ON jsp.k = jsb.k WHERE jsp.s LIKE '%7'$$);
-- spec: relspill/Values longer than a chunk
-- Values of up to 130 kB, longer than any chunk of the join: each goes
-- to disk in a chunk of values of its own, and reads back by the longest
-- chunk of values the set wrote. OFFSET 0 keeps the value computed below
-- the join, which stores it; else the join stores the short text and
-- computes the value above.
SELECT EXISTS (SELECT FROM join_explain($$SELECT count(*), sum(length(w.t)), md5(string_agg(md5(w.t), ',' ORDER BY w.n, jsp.s)) FROM jsp JOIN (SELECT k, n, CASE WHEN n % 997 = 0 THEN repeat(t, 2000) ELSE t END AS t FROM jsb OFFSET 0) AS w ON jsp.k = w.k$$) AS line
               WHERE line ~ 'Spilled Chunks: [1-9]') AS spilled;
SELECT join_same($$SELECT count(*), sum(length(w.t)), md5(string_agg(md5(w.t), ',' ORDER BY w.n, jsp.s)) FROM jsp JOIN (SELECT k, n, CASE WHEN n % 997 = 0 THEN repeat(t, 2000) ELSE t END AS t FROM jsb OFFSET 0) AS w ON jsp.k = w.k$$);
-- A hashed key that is an expression its child computes, such as
-- lower(t), goes to disk as the 64-bit hash the table keys by, not the
-- address of a value its batch frees: a batch read back is not hashed
-- again.
SELECT join_explain($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$, false);
SELECT join_same($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$);
SELECT join_same($$SELECT count(*), count(jsb.n) FROM jsp LEFT JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$);
-- 70 outer columns kept on disk, past the 64 that one lane of NULL bits
-- holds: each 64 have a lane of their own, and NULLs, numbers and text
-- past column 64 come back. No partition stays in memory (Resident
-- Partitions: 0), so every match is of outer rows read back from disk.
DO $$
BEGIN
    EXECUTE (SELECT format('CREATE TABLE jw70 WITH (autovacuum_enabled = off) AS SELECT NULLIF(g %% 20000, 7) AS k, %s FROM generate_series(1, 20000) AS g',
                           string_agg(format(CASE WHEN c % 10 = 0
                                                  THEN 'CASE WHEN g %% %1$s = 1 THEN NULL ELSE ''w'' || g * %1$s END AS c%1$s'
                                                  ELSE 'CASE WHEN g %% %1$s = 1 THEN NULL ELSE g + %1$s END AS c%1$s' END, c),
                                      ', ' ORDER BY c))
             FROM generate_series(1, 70) AS c);
END $$;
ANALYZE jw70;
SELECT format('SELECT count(*), sum(hashtext(concat_ws('','', %s, jsb.n))) FROM jw70 %%s JOIN jsb ON jw70.k = jsb.k',
              string_agg('jw70.c' || c, ', ' ORDER BY c)) AS jw70_query
FROM generate_series(1, 70) AS c \gset
SELECT line FROM join_explain(format(:'jw70_query', '')) AS line
WHERE line ~ 'TessHashJoin|Resident Partitions|Matches';
SELECT join_same(format(:'jw70_query', ''));
SELECT join_same(format(:'jw70_query', 'LEFT'));
-- A function's plans freed, the allocator recycles their contexts (aset.c)
-- with the largest chunk a block shares, 8 kB: the RIGHT join below got
-- one for a partition read back and took 1579 kB of a hash_mem of 1024.
SELECT count(*) FROM join_explain($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_explain($$SELECT count(*), count(jsb.n), sum(length(jsb.t)) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsb.n), sum(length(jsb.t)), sum(length(jsp.s)) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k WHERE jsp.s LIKE '%3'$$);
SELECT join_same($$SELECT count(*), sum(length(jsp.s)) FROM jsp WHERE EXISTS (SELECT 1 FROM jsb WHERE jsb.k = jsp.k)$$);
SELECT join_same($$SELECT jsp.s FROM jsp WHERE NOT EXISTS (SELECT 1 FROM jsb WHERE jsb.k = jsp.k)$$);
SELECT join_same($$SELECT count(*), count(jsb.n), count(jsb.t) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k WHERE jsb.k IS NULL$$);
SELECT join_same($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON jsp.k = jsb.k AND jsb.n > jsp.k * 2$$);
SELECT join_explain($$SELECT count(*), sum(jskew.w) FROM jsp JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT count(*), sum(jskew.w), sum(jsp.k) FROM jsp JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT count(*), count(jskew.w), sum(jskew.w) FROM jsp LEFT JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT jsp.s, jskew.w FROM jsp LEFT JOIN jskew() AS jskew ON jsp.k = jskew.k WHERE jsp.s LIKE '%07'$$);
SELECT join_same($$SELECT count(*), sum(length(jsp.s)) FROM jsp WHERE EXISTS (SELECT 1 FROM jskew() AS jskew WHERE jskew.k = jsp.k)$$);
SELECT join_same($$SELECT count(*), sum(length(jsp.s)) FROM jsp WHERE NOT EXISTS (SELECT 1 FROM jskew() AS jskew WHERE jskew.k = jsp.k)$$);
SELECT join_same($$SELECT count(*) FROM jsp JOIN jskew() AS jskew ON jsp.k = jskew.k AND jskew.w % 3 = jsp.k % 3$$);
-- The same key with a text column NULL in every row: its partition
-- writes no block of values, whose start would end a piece, and is
-- still joined in pieces, since its records refer to no value.
CREATE FUNCTION jskewnull() RETURNS TABLE (k int, w int, t text) LANGUAGE sql ROWS 10
AS 'SELECT CASE WHEN g <= 60000 THEN 7 ELSE g END, g, CASE WHEN g < 0 THEN g::text END FROM generate_series(1, 62000) AS g';
SELECT line FROM join_explain($$SELECT count(*), count(n.t), sum(n.w) FROM jsp JOIN jskewnull() AS n ON jsp.k = n.k$$) AS line
WHERE line ~ 'Memory Usage|Extra Passes';
SELECT join_same($$SELECT count(*), count(n.t), sum(n.w), sum(jsp.k) FROM jsp JOIN jskewnull() AS n ON jsp.k = n.k$$);
SELECT join_same($$SELECT count(*), count(n.t), sum(n.w) FROM jsp LEFT JOIN jskewnull() AS n ON jsp.k = n.k$$);
-- 200000 inner rows with text the planner expects 10 of: the first
-- level's partitions are still larger than hash_mem, and split by the
-- next bits of the hash into a level of their own.
CREATE FUNCTION jwide() RETURNS TABLE (k int, t text) LANGUAGE sql ROWS 10
AS 'SELECT g % 50000, repeat(''x'', g % 30) || g FROM generate_series(1, 200000) AS g';
SELECT join_explain($$SELECT count(*), sum(length(w.t)) FROM jsp JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT count(*), sum(length(w.t)), sum(jsp.k) FROM jsp JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT jsp.s, w.t FROM jsp JOIN jwide() AS w ON jsp.k = w.k WHERE jsp.s LIKE '%13'$$);
SELECT join_same($$SELECT count(*), count(w.t), sum(length(w.t)) FROM jsp LEFT JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT count(*), sum(length(jsp.s)) FROM jsp WHERE EXISTS (SELECT 1 FROM jwide() AS w WHERE w.k = jsp.k)$$);
SELECT join_same($$SELECT jsp.s FROM jsp WHERE NOT EXISTS (SELECT 1 FROM jwide() AS w WHERE w.k = jsp.k)$$);
-- 1100 outer columns kept on disk: a chunk of a partition's outer rows
-- holds four of them at least, where a chunk of the inner records' size,
-- 8 kB here, would hold none, and the join would try it again without
-- end. No partition stays in memory.
DO $$
BEGIN
    EXECUTE (SELECT format('CREATE TABLE jw1100 AS SELECT g AS k, %s FROM generate_series(1, 50) AS g',
                           string_agg(format('CASE WHEN g %% %1$s = 1 THEN NULL ELSE g + %1$s END AS c%1$s', c),
                                      ', ' ORDER BY c))
             FROM generate_series(1, 1100) AS c);
END $$;
ANALYZE jw1100;
SELECT line FROM join_explain($$SELECT jw1100.*, w.t FROM jw1100 JOIN jwide() AS w ON jw1100.k = w.k$$) AS line
WHERE line ~ 'TessHashJoin|Resident Partitions|Matches';
SELECT join_same($$SELECT jw1100.*, w.t FROM jw1100 JOIN jwide() AS w ON jw1100.k = w.k$$);
-- The same with values of 100 bytes, which outgrow a partition's chunk
-- of values between two of its chunks of records: the split writes them
-- as it goes, and queues each partition once until it does.
CREATE FUNCTION jlong() RETURNS TABLE (k int, t text) LANGUAGE sql ROWS 10
AS 'SELECT g % 50000, repeat(''x'', 100) || g FROM generate_series(1, 200000) AS g';
SELECT join_explain($$SELECT count(*), sum(length(w.t)) FROM jsp JOIN jlong() AS w ON jsp.k = w.k$$, false);
SELECT bool_or(line ~ 'Split Partitions: [1-9]') AS split
FROM join_explain($$SELECT count(*), sum(length(w.t)) FROM jsp JOIN jlong() AS w ON jsp.k = w.k$$) AS line;
SELECT join_same($$SELECT count(*), sum(length(w.t)), sum(jsp.k) FROM jsp JOIN jlong() AS w ON jsp.k = w.k$$);
-- A rescan with a parameter of the outer side reads the inner side again.
SELECT join_same($$SELECT v.x, (SELECT count(*) FROM jsp JOIN jsb ON jsp.k = jsb.k WHERE jsp.k < v.x) FROM (VALUES (100), (15000)) AS v(x)$$);
-- RIGHT and FULL over a table that spills: each table in memory, the
-- resident partitions', a partition's, each piece of one, and a level's,
-- returns its records without a pair before it goes.
SELECT join_explain($$SELECT count(*), count(jsp.s), sum(length(jsb.t)) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), sum(length(jsb.t)), sum(jsb.n) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jsb.t), sum(jsb.n) FROM jsp FULL JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t FROM jsp FULL JOIN jsb ON jsp.k = jsb.k WHERE jsb.n % 100 = 3 OR jsp.s LIKE '%77'$$);
SELECT join_explain($$SELECT count(*), count(jsp.s), sum(jskew.w) FROM jsp RIGHT JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), sum(jskew.w) FROM jsp RIGHT JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jskew.w), sum(jskew.w) FROM jsp FULL JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_explain($$SELECT count(*), count(jsp.s), sum(length(w.t)) FROM jsp RIGHT JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), sum(length(w.t)) FROM jsp RIGHT JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(w.t), sum(length(w.t)) FROM jsp FULL JOIN jwide() AS w ON jsp.k = w.k$$);
-- At 64kB: more partitions and levels, chunks of a few records, each with
-- its own marks.
SET work_mem = '64kB';
SELECT join_explain($$SELECT count(*), count(jsp.s), sum(length(jsb.t)) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), sum(length(jsb.t)), sum(jsb.n) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jsb.t), sum(jsb.n) FROM jsp FULL JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t FROM jsp FULL JOIN jsb ON jsp.k = jsb.k WHERE jsb.n % 100 = 3 OR jsp.s LIKE '%77'$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jskew.w), sum(jskew.w) FROM jsp FULL JOIN jskew() AS jskew ON jsp.k = jskew.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(w.t), sum(length(w.t)) FROM jsp FULL JOIN jwide() AS w ON jsp.k = w.k$$);
SELECT join_same($$SELECT jsp.s FROM jsp WHERE NOT EXISTS (SELECT 1 FROM jsb WHERE jsb.k = jsp.k)$$);
-- RIGHT ANTI over the same tables: the marks of every table it spills
-- into, and only the records without a pair go out, NULL keys among them;
-- jskew's one key of 60000 records is joined in pieces.
SELECT line FROM join_explain($$SELECT count(*), sum(jsb.n) FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$) AS line
WHERE line ~ 'TessHashJoin|Join Type|Disk Usage|Build Rows|Probe Rows|Matches';
SELECT join_same($$SELECT jsb.k, jsb.n FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$);
SELECT line FROM join_explain($$SELECT count(*), sum(jskew.w) FROM jskew() AS jskew WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jskew.k)$$) AS line
WHERE line ~ 'TessHashJoin|Join Type|Disk Usage|Extra Passes|Build Rows|Matches';
SELECT join_same($$SELECT jskew.k, jskew.w FROM jskew() AS jskew WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jskew.k)$$);
-- A join clause that marks two thirds of the key's records, piece by
-- piece.
SELECT line FROM join_explain($$SELECT jskew.k, jskew.w FROM jskew() AS jskew WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jskew.k AND jskew.w % 3 <> jsp.k % 3)$$) AS line
WHERE line ~ 'Join Type|Extra Passes';
SELECT join_same($$SELECT jskew.k, jskew.w FROM jskew() AS jskew WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jskew.k AND jskew.w % 3 <> jsp.k % 3)$$);
RESET work_mem;

-- RIGHT and FULL: the inner side's records that find a pair are marked,
-- and those without one, NULL keys among them, go out after the outer
-- side with NULL outer columns.
EXPLAIN (COSTS OFF) SELECT jf.v, jd.label FROM jf RIGHT JOIN jd ON jf.fk = jd.id;
EXPLAIN (COSTS OFF) SELECT jf.v, jd.label FROM jf FULL JOIN jd ON jf.fk = jd.id;
SELECT join_same($$SELECT jf.v, jd.id, jd.label FROM jf RIGHT JOIN jd ON jf.fk = jd.id$$);
SELECT join_same($$SELECT jf.v, jf.fk, jd.id, jd.n FROM jf FULL JOIN jd ON jf.fk = jd.id$$);
SELECT join_same($$SELECT count(*), count(jf.v), count(jd.id) FROM jf FULL JOIN jd ON jf.fk = jd.id$$);
-- NULL keys on both sides, and duplicates: four inner rows per key.
SELECT join_same($$SELECT jf.v, jdup.w, jdup.t FROM jf RIGHT JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT jf.v, jdup.w FROM jf FULL JOIN jdup ON jf.fk = jdup.k$$);
SELECT join_same($$SELECT jdup.w, jf.v FROM jdup FULL JOIN jf ON jdup.k = jf.fk$$);
-- A join clause beyond the keys decides the pairs, and the marks.
SELECT join_same($$SELECT jf.v, jd.id FROM jf RIGHT JOIN jd ON jf.fk = jd.id AND jf.v > jd.n$$);
SELECT join_same($$SELECT jf.v, jd.id FROM jf FULL JOIN jd ON jf.fk = jd.id AND jf.v % 3 = 0$$);
-- Filters above the join over either side's columns.
SELECT join_same($$SELECT jf.v, jdup.w FROM jf FULL JOIN jdup ON jf.fk = jdup.k WHERE jdup.w IS NULL$$);
SELECT join_same($$SELECT jdup.w FROM jf RIGHT JOIN jdup ON jf.fk = jdup.k WHERE jf.fk IS NULL$$);
EXPLAIN (COSTS OFF)
SELECT x, (SELECT count(*) FROM (SELECT jd.id FROM jsmall FULL JOIN jd ON jsmall.k = jd.id AND jd.id > x) s)
FROM generate_series(0, 300, 100) AS x;
SELECT join_same($$SELECT jf.v, jd.id FROM jf FULL JOIN jd ON jf.fk = jd.id WHERE jd.n > 1000 OR jd.n IS NULL$$);
-- Empty sides.
SELECT join_same($$SELECT jf.v, jempty.e FROM jf FULL JOIN jempty ON jf.fk = jempty.k$$);
SELECT join_same($$SELECT jempty.e, jd.id FROM jempty RIGHT JOIN jd ON jempty.k = jd.id$$);
SELECT join_same($$SELECT jempty.e, jd.id FROM jempty FULL JOIN jd ON jempty.k = jd.id$$);
-- A rescan: the table stays, and its marks start anew.
SELECT join_same($$
SELECT x, (SELECT count(*) FROM (SELECT jd.id FROM jsmall FULL JOIN jd ON jsmall.k = jd.id AND jd.id > x) s)
FROM generate_series(0, 300, 100) AS x$$);

-- RIGHT ANTI, the core's plan of NOT EXISTS that builds the side it keeps:
-- the pairs mark their records and go nowhere, and the records left
-- without a mark, NULL keys among them, go out after the outer side.
EXPLAIN (COSTS OFF) SELECT jd.id, jd.label FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v < 500);
SELECT join_same($$SELECT jd.id, jd.label FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v < 500)$$);
-- NULL keys and duplicates on the kept side, three records a key; and a
-- join clause true, false or NULL for the records of one key, which marks
-- only those it passes.
SELECT line FROM join_explain($$SELECT jdup.k, jdup.w, jdup.t FROM jdup WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jdup.k)$$, false) AS line WHERE line ~ 'Join Type';
SELECT join_same($$SELECT jdup.k, jdup.w, jdup.t FROM jdup WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jdup.k)$$);
SELECT line FROM join_explain($$SELECT jdup.k, jdup.w FROM jdup WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jdup.k AND jf.m > jdup.w * 5)$$, false) AS line WHERE line ~ 'Join Type';
SELECT join_same($$SELECT jdup.k, jdup.w FROM jdup WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jdup.k AND jf.m > jdup.w * 5)$$);
-- A join clause beyond the keys decides the marks: NULL in jd.n marks none.
EXPLAIN (COSTS OFF) SELECT jd.id FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > jd.n);
SELECT join_same($$SELECT jd.id, jd.n FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > jd.n)$$);
-- Empty sides: every kept row goes out, or none. jsp's rows all leave
-- by a value the planner does not know.
SELECT line FROM join_explain($$SELECT jd.id FROM jd WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jd.id AND jsp.k < (SELECT -1))$$, false) AS line WHERE line ~ 'Join Type';
SELECT join_same($$SELECT jd.id FROM jd WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jd.id AND jsp.k < (SELECT -1))$$);
SELECT line FROM join_explain($$SELECT jempty.e FROM jempty WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jempty.k)$$, false) AS line WHERE line ~ 'Join Type';
SELECT join_same($$SELECT jempty.e FROM jempty WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jempty.k)$$);
-- A rescan with a parameter of the outer side only: the table stays, its
-- marks start anew, and fewer outer rows mark fewer records each time.
SELECT line FROM join_explain($$SELECT x, (SELECT count(*) FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > x)) FROM generate_series(0, 1000, 250) AS x$$) AS line
WHERE line ~ 'Join Type|Builds';
SELECT join_same($$SELECT x, (SELECT count(*) FROM jd WHERE NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > x)) FROM generate_series(0, 1000, 250) AS x$$);
-- A parameter of the kept side too: the table is built anew.
EXPLAIN (COSTS OFF)
SELECT x, (SELECT count(*) FROM jd WHERE jd.id > x AND NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > x)) FROM generate_series(0, 300, 100) AS x;
SELECT join_same($$
SELECT x, (SELECT count(*) FROM jd WHERE jd.id > x AND NOT EXISTS (SELECT 1 FROM jf WHERE jf.fk = jd.id AND jf.v > x)) FROM generate_series(0, 300, 100) AS x$$);

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
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == "TessHashJoin").%I', name)::jsonpath)::text;
END $$;
-- No autovacuum, for the reason given at jbuild.
CREATE TABLE jbig WITH (autovacuum_enabled = off) AS SELECT g % 350 + 1 AS fk, g AS v FROM generate_series(1, 20000) AS g;
ANALYZE jbig;
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
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
SELECT join_same($$SELECT count(*), count(jd.n), count(jd.label) FROM jbig LEFT JOIN jd ON jbig.fk = jd.id WHERE jd.id IS NULL$$);
SELECT join_property($$SELECT count(*) FROM jbig JOIN jd ON jbig.fk = jd.id$$, 'Probe Rows') AS probe_rows,
       join_property($$SELECT count(*) FROM jbig JOIN jd ON jbig.fk = jd.id$$, 'Matches') AS matches;
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(jd.n), sum(jbig.v) FROM jbig JOIN jd ON jbig.fk = jd.id$$);
RESET parallel_leader_participation;
-- Each participant spills a table of its own.
SET work_mem = '512kB';
SELECT join_same($$SELECT count(*), sum(length(jsb.t)), sum(jsp.k) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsb.n) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsb.n), count(jsb.t) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k WHERE jsb.k IS NULL$$);
SELECT join_property($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$, 'Parallel Aware') AS parallel,
       join_property($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$);
RESET work_mem;
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
-- RIGHT and FULL take the shared table only: every participant marks the
-- records of its pairs in shared memory, and the last one to leave the
-- table returns those without a mark.
EXPLAIN (COSTS OFF) SELECT count(*), count(jprobe.v) FROM jprobe RIGHT JOIN jbuild ON jprobe.k = jbuild.k;
SELECT join_same($$SELECT count(*), count(jprobe.v), sum(jbuild.w) FROM jprobe RIGHT JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT count(*), count(jprobe.v), count(jbuild.w), sum(jbuild.w) FROM jprobe FULL JOIN jbuild ON jprobe.k = jbuild.k$$);
SELECT join_same($$SELECT jprobe.v, jbuild.w FROM jprobe FULL JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w % 10 = 3 OR jprobe.v % 1000 = 7$$);
SELECT join_same($$SELECT count(*), count(jbig.v), count(jgrow.g), sum(jgrow.g) FROM jbig FULL JOIN jgrow ON jbig.fk = jgrow.k$$);
SELECT join_same($$SELECT count(*), count(jbig.v), count(jd.label), max(jd.label) FROM jbig RIGHT JOIN jd ON jbig.fk = jd.id AND jbig.v % 7 = 0$$);
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), count(jprobe.v), count(jbuild.w), sum(jbuild.w) FROM jprobe FULL JOIN jbuild ON jprobe.k = jbuild.k$$);
RESET parallel_leader_participation;
-- A rescan of the Gather builds the table and its marks anew.
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jprobe RIGHT JOIN jbuild ON jprobe.k = jbuild.k AND jprobe.v > jsmall.k * 100) FROM jsmall$$);
-- Under the core's Gather a LIMIT bounds each worker's tuples, and a
-- worker that has its share stops while it probes: it marks the table
-- first, and the last participant to leave then returns no inner row
-- without a mark, since the pairs of the rows it left were never marked.
-- Every row of jbuild has a pair in jhit: no row may come without one.
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT jhit.v FROM jhit RIGHT JOIN jbuild ON jhit.k = jbuild.k LIMIT 100;
SELECT count(*), count(*) FILTER (WHERE v IS NULL) AS without_pair
FROM (SELECT jhit.v FROM jhit RIGHT JOIN jbuild ON jhit.k = jbuild.k LIMIT 100) AS q;
RESET tessera.batch_gather;
-- RIGHT ANTI takes the shared table only too, and returns nothing while
-- it probes: a worker with its share of the LIMIT stops in the tail only,
-- after every participant has marked. Every row of jbuild has a pair in
-- jhit: none may come out.
EXPLAIN (COSTS OFF) SELECT count(*), sum(jbuild.w) FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k);
SELECT join_same($$SELECT jbuild.k, jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k)$$);
SET parallel_leader_participation = off;
SELECT join_property($$SELECT jbuild.k, jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k)$$, 'Join Type') AS kind, join_property($$SELECT jbuild.k, jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k)$$, 'Shared Table') AS shared;
SELECT join_same($$SELECT jbuild.k, jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k)$$);
RESET parallel_leader_participation;
SELECT join_property($$SELECT jsmall.k, (SELECT count(*) FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k AND jprobe.v > jsmall.k * 100)) FROM jsmall$$, 'Join Type') AS kind, join_property($$SELECT jsmall.k, (SELECT count(*) FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k AND jprobe.v > jsmall.k * 100)) FROM jsmall$$, 'Shared Table') AS shared;
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k AND jprobe.v > jsmall.k * 100)) FROM jsmall$$);
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jhit WHERE jhit.k = jbuild.k) LIMIT 100;
SELECT count(*) FROM (SELECT jbuild.w FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jhit WHERE jhit.k = jbuild.k) LIMIT 100) AS q;
-- 4802 records without a pair in jprobe: the LIMIT stops in the tail,
-- each row returned without a pair.
EXPLAIN (COSTS OFF) SELECT jbuild.k FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k) LIMIT 100;
SELECT count(*), count(*) FILTER (WHERE EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = q.k)) AS with_pair
FROM (SELECT jbuild.k FROM jbuild WHERE NOT EXISTS (SELECT 1 FROM jprobe WHERE jprobe.k = jbuild.k) LIMIT 100) AS q;
RESET tessera.batch_gather;
-- Duplicate keys: the next record of a key is found down its chain.
EXPLAIN (COSTS OFF) SELECT count(*), sum(jgrow.g) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k;
SELECT join_same($$SELECT count(*), sum(jgrow.g), sum(jbig.v) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$);
SELECT join_same($$SELECT jbig.v, jgrow.g FROM jbig JOIN jgrow ON jbig.fk = jgrow.k WHERE jbig.v < 200$$);
-- The planner expects no rows of jgrow: the participants append them all
-- to chunks of their own, and the index is made once, for all of them.
SELECT join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Builds') AS builds,
       join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Build Rows') AS build_rows,
       join_property($$SELECT count(*) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Buckets') AS buckets;
-- The links count the duplicates of the shared table: jgrow has them,
-- and its pairs go out in compact batches; jbuild has none, and no rounds
-- or compact batches follow the first.
SELECT join_property($$SELECT count(*), sum(jgrow.g) FROM jbig JOIN jgrow ON jbig.fk = jgrow.k$$, 'Compact Batches')::int > 0 AS dup_compact,
       join_property($$SELECT count(*), sum(jbuild.w) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Compact Batches') IS NULL AS unique_rounds;
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$);
RESET parallel_leader_participation;
-- One participant builds the shared Bloom filter for all; the others use
-- it once it is ready.
SELECT join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Bloom Filters') AS filters,
       join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k$$, 'Shared Table') AS shared;
-- A shared table of fewer than 4096 rows never wants a filter of its own
-- at the default ratio: a LEFT join, which keeps its outer rows, can hand
-- none down, and none is allocated; at a ratio of 1 every table may want
-- one, and its 1025 words are allocated with the table. An INNER join's
-- small table allocates them for its outer child at the default ratio.
EXPLAIN (COSTS OFF) SELECT count(*), count(b.w) FROM jprobe LEFT JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k;
SET tessera.join_bloom_ratio = 1;
SELECT join_property($$SELECT count(*), count(b.w) FROM jprobe LEFT JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k$$, 'Memory Usage') AS with_filter \gset
RESET tessera.join_bloom_ratio;
SELECT join_property($$SELECT count(*), count(b.w) FROM jprobe LEFT JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k$$, 'Memory Usage') AS without_filter \gset
SELECT :with_filter - :without_filter >= 8 AS filter_only_where_wanted;
SELECT join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k$$, 'Memory Usage') AS inner_filter \gset
SELECT :inner_filter - :without_filter >= 8 AS for_the_child;
-- A shared small table hands its filter to each participant's TessFilter.
SELECT join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k WHERE jprobe.v < 19000$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k WHERE jprobe.v < 19000$$, 'Bloom Filter Below') AS below;
SELECT join_same($$SELECT jprobe.v, b.w FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k = b.k WHERE jprobe.v < 19000$$);
-- A key each participant's TessFilter computes: every one refuses the
-- filter, and no participant counts or checks one.
SELECT join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$, 'Bloom Filters') AS filters,
       join_property($$SELECT count(*) FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$, 'Rows Removed by Bloom Filter') AS removed;
SELECT join_same($$SELECT jprobe.v, b.w FROM jprobe JOIN (SELECT * FROM jbuild WHERE w <= 4000) AS b ON jprobe.k + 0 = b.k WHERE jprobe.v < 19000$$);
SELECT join_same($$SELECT count(*), sum(jprobe.v) FROM jprobe WHERE NOT EXISTS (SELECT 1 FROM jbuild WHERE jbuild.k = jprobe.k)$$);
-- The shared filter below the join: each participant's TessFilter checks
-- its rows once the filter is ready.
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$);
SELECT join_property($$SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v > 0 AND jprobe.v::text LIKE '%1%'$$, 'Bloom Filter Below') AS below;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v::text LIKE '%1%';
SELECT join_same($$SELECT count(*), sum(jbuild.w), sum(jprobe.v) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jprobe.v::text LIKE '%1%'$$);
-- By-reference inner columns: their values live in the query's shared
-- memory, in blocks of each participant's, and the payload holds where.
EXPLAIN (COSTS OFF) SELECT count(*), max(jd.label) FROM jbig JOIN jd ON jbig.fk = jd.id;
SELECT join_same($$SELECT jbig.v, jd.label FROM jbig JOIN jd ON jbig.fk = jd.id$$);
SELECT join_same($$SELECT count(*), count(jd.label), max(jd.label) FROM jbig LEFT JOIN jd ON jbig.fk = jd.id$$);
SELECT join_same($$SELECT count(*) FROM jbig WHERE EXISTS (SELECT 1 FROM jd WHERE jd.id = jbig.fk AND jd.label <> 'd' || jbig.v)$$);
-- numeric and text with NULLs, values of 32 to 1920 bytes over many
-- blocks; a value computed from them goes out above the join, so the
-- table keeps the stored ones.
CREATE TABLE jref (k int, n numeric, t text);
INSERT INTO jref
SELECT g, CASE WHEN g % 4 = 0 THEN NULL ELSE g / 7.0 END,
       CASE WHEN g % 5 = 0 THEN NULL ELSE repeat(md5(g::text), 1 + g % 60) END
FROM generate_series(1, 20000) AS g;
ANALYZE jref;
-- No autovacuum, for the reason given at jbuild.
CREATE TABLE jrefprobe WITH (autovacuum_enabled = off) AS SELECT g % 25000 AS k, g AS v FROM generate_series(1, 60000) AS g;
ANALYZE jrefprobe;
EXPLAIN (COSTS OFF) SELECT count(*), sum(jref.n), max(jref.t) FROM jrefprobe JOIN jref ON jrefprobe.k = jref.k;
SELECT join_same($$SELECT count(*), sum(jref.n), count(jref.t), sum(length(jref.t)), max(jref.t) FROM jrefprobe JOIN jref ON jrefprobe.k = jref.k$$);
SELECT join_same($$SELECT jrefprobe.v, jref.n, jref.t FROM jrefprobe LEFT JOIN jref ON jrefprobe.k = jref.k WHERE jrefprobe.v % 97 = 0$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(length(big.t)) FROM jrefprobe JOIN (SELECT k, CASE WHEN k % 500 = 0 THEN repeat(t, 20) ELSE t END AS t FROM jref) AS big ON jrefprobe.k = big.k;
SELECT join_same($$SELECT count(*), sum(length(big.t)) FROM jrefprobe JOIN (SELECT k, CASE WHEN k % 500 = 0 THEN repeat(t, 20) ELSE t END AS t FROM jref) AS big ON jrefprobe.k = big.k$$);
-- A table larger than the planner thinks keeps its values where they
-- are, and its index is made for every row.
-- No autovacuum: its analyze would correct the planner's estimate.
CREATE TABLE jrefgrow (k int, t text) WITH (autovacuum_enabled = off);
ANALYZE jrefgrow;
INSERT INTO jrefgrow SELECT g % 400, 'g' || g FROM generate_series(1, 20000) AS g;
SELECT join_same($$SELECT count(*), max(jrefgrow.t), sum(length(jrefgrow.t)) FROM jbig JOIN jrefgrow ON jbig.fk = jrefgrow.k$$);
SELECT join_property($$SELECT count(*), max(jrefgrow.t) FROM jbig JOIN jrefgrow ON jbig.fk = jrefgrow.k$$, 'Buckets') AS buckets;
-- A rescan of the Gather frees the blocks and fills new ones.
SELECT join_same($$SELECT jsmall.k, (SELECT max(jref.t) FROM jrefprobe JOIN jref ON jrefprobe.k = jref.k WHERE jref.k > jsmall.k * 1000) FROM jsmall$$);
-- A rescan of the Gather builds the shared table anew.
SELECT join_same($$SELECT jsmall.k, (SELECT count(*) FROM jprobe JOIN jbuild ON jprobe.k = jbuild.k WHERE jbuild.w > jsmall.k * 100) FROM jsmall$$);
-- A shared table that spills: past every participant's hash_mem it splits
-- into partitions, two per participant at least; the largest go to disk,
-- each participant's chunks of them to files of its own, and every outer
-- row is written before any row goes out, those of the partitions in
-- memory and those without a pair to be probed by the shared table. Each
-- partition on disk is then joined by the participant that takes it, from
-- every participant's files. Every participant's hash_mem of 512 kB:
-- jsb's text takes the table well past them.
SET work_mem = '256kB';
SELECT join_property($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(length(jsb.t)), sum(jsp.k), sum(jsb.n) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t, jsb.n FROM jsp JOIN jsb ON jsp.k = jsb.k WHERE jsp.s LIKE '%7'$$);
SELECT join_same($$SELECT count(*), count(jsb.n), sum(length(jsb.t)), sum(length(jsp.s)) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k WHERE jsp.s LIKE '%3'$$);
SELECT join_same($$SELECT count(*), sum(jsb.n), sum(length(jsb.t)) FROM jsp JOIN jsb ON jsp.k = jsb.k AND jsb.n > jsp.k * 2$$);
SELECT join_same($$SELECT count(*), count(jsb.n), sum(length(jsb.t)) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k AND jsb.n > jsp.k * 2$$);
SELECT join_property($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(jsb.n) FROM jsp JOIN jsb ON lower(jsp.s) = lower('p' || jsb.n)$$);
-- spec: spill-format/Bounds agreed by the participants
-- Keys longer than any chunk: a hashed key its child computes goes to
-- disk by its bytes, each participant's of its own length, and each
-- participant reads the others' files by the longest chunk of values any
-- of them wrote.
SELECT join_property($$SELECT count(*), sum(length(a.t)), md5(string_agg(md5(a.t), ',' ORDER BY a.n)) FROM jsb AS a JOIN jsb AS b ON (CASE WHEN a.n % 997 = 0 THEN repeat(a.t, 2000) ELSE a.t END) = (CASE WHEN b.n % 997 = 0 THEN repeat(b.t, 2000) ELSE b.t END)$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(length(a.t)), md5(string_agg(md5(a.t), ',' ORDER BY a.n)) FROM jsb AS a JOIN jsb AS b ON (CASE WHEN a.n % 997 = 0 THEN repeat(a.t, 2000) ELSE a.t END) = (CASE WHEN b.n % 997 = 0 THEN repeat(b.t, 2000) ELSE b.t END)$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(length(a.t)), md5(string_agg(md5(a.t), ',' ORDER BY a.n)) FROM jsb AS a JOIN jsb AS b ON (CASE WHEN a.n % 997 = 0 THEN repeat(a.t, 2000) ELSE a.t END) = (CASE WHEN b.n % 997 = 0 THEN repeat(b.t, 2000) ELSE b.t END)$$);
-- One key held by 60000 inner rows the planner does not expect: the
-- table splits late, and the key's partition, larger than hash_mem, is
-- joined in pieces by the participant that takes it.
-- No autovacuum: its analyze would correct the planner's estimate.
CREATE TABLE jsskew (k int, w int) WITH (autovacuum_enabled = off);
ANALYZE jsskew;
INSERT INTO jsskew SELECT CASE WHEN g <= 60000 THEN 7 ELSE g END, g FROM generate_series(1, 62000) AS g;
SELECT join_property($$SELECT count(*), sum(jsskew.w) FROM jsp JOIN jsskew ON jsp.k = jsskew.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(jsskew.w) FROM jsp JOIN jsskew ON jsp.k = jsskew.k$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(jsskew.w), sum(jsp.k) FROM jsp JOIN jsskew ON jsp.k = jsskew.k$$);
SELECT join_same($$SELECT jsp.s, jsskew.w FROM jsp JOIN jsskew ON jsp.k = jsskew.k WHERE jsp.s LIKE '%07'$$);
-- spec: relspill/A partition taken alone
-- One key held by 2000 inner rows of 1 kB of text, among 2000 rows of
-- other keys: its partition has few records but some 2 MB of values,
-- past the room of a participant. The one that takes it alone splits
-- it, as a serial join splits a partition that large, until the key
-- holds most of a partition's rows, and reads that one in pieces; it
-- does not load the partition whole.
CREATE TABLE jslong (k int, t text);
INSERT INTO jslong
SELECT CASE WHEN g <= 2000 THEN 7 ELSE g END,
       CASE WHEN g <= 2000
            THEN (SELECT string_agg(md5(g || ':' || i), '') FROM generate_series(1, 32) AS i)
            ELSE 'o' || g END
FROM generate_series(1, 4000) AS g;
ANALYZE jslong;
SELECT join_property($$SELECT count(*), sum(length(jslong.t)) FROM jsp JOIN jslong ON jsp.k = jslong.k$$, 'Partitions Joined Alone')::int > 0 AS alone,
       coalesce(join_property($$SELECT count(*), sum(length(jslong.t)) FROM jsp JOIN jslong ON jsp.k = jslong.k$$, 'Split Partitions')::int, 0) > 0 AS split,
       coalesce(join_property($$SELECT count(*), sum(length(jslong.t)) FROM jsp JOIN jslong ON jsp.k = jslong.k$$, 'Extra Passes')::int, 0) > 0 AS in_pieces;
SELECT join_same($$SELECT count(*), sum(length(jslong.t)), sum(jsp.k) FROM jsp JOIN jslong ON jsp.k = jslong.k$$);
SELECT coalesce(join_property($$SELECT count(*), count(jslong.t), sum(length(jslong.t)) FROM jsp LEFT JOIN jslong ON jsp.k = jslong.k$$, 'Extra Passes')::int, 0) > 0 AS left_in_pieces;
SELECT join_same($$SELECT count(*), count(jslong.t), sum(length(jslong.t)) FROM jsp LEFT JOIN jslong ON jsp.k = jslong.k$$);
-- RIGHT and FULL: the shared table's tail by the last one to leave it,
-- a partition's on disk by the participant that joins it, after each
-- piece.
SELECT join_property($$SELECT count(*), count(jsp.s), sum(length(jsb.t)) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), count(jsp.s), sum(length(jsb.t)) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), count(jsp.s), sum(length(jsb.t)), sum(jsb.n) FROM jsp RIGHT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jsb.t), sum(jsb.n) FROM jsp FULL JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT jsp.s, jsb.t FROM jsp FULL JOIN jsb ON jsp.k = jsb.k WHERE jsb.n % 100 = 3 OR jsp.s LIKE '%77'$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jsskew.w), sum(jsskew.w) FROM jsp FULL JOIN jsskew ON jsp.k = jsskew.k$$);
-- RIGHT ANTI: the same marks, and only the tails.
SELECT join_property($$SELECT count(*), sum(jsb.n) FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$, 'Join Type') AS kind,
       join_property($$SELECT count(*), sum(jsb.n) FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(jsb.n) FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT jsb.k, jsb.n FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000)$$);
SELECT join_same($$SELECT count(*), sum(jsskew.w) FROM jsskew WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsskew.k)$$);
SET tessera.batch_gather = off;
SELECT count(*), count(*) FILTER (WHERE EXISTS (SELECT 1 FROM jsp WHERE jsp.k = q.k + 7000)) AS with_pair
FROM (SELECT jsb.k FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k + 7000) LIMIT 100) AS q;
RESET tessera.batch_gather;
-- The workers alone, and a rescan of the Gather: the files go with the
-- set, and the table spills anew.
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(length(jsb.t)), sum(jsp.k) FROM jsp JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsb.n), sum(length(jsb.t)) FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k$$);
SELECT join_same($$SELECT count(*), count(jsp.s), count(jsb.t), sum(jsb.n) FROM jsp FULL JOIN jsb ON jsp.k = jsb.k$$);
RESET parallel_leader_participation;
SET enable_material = off;
SELECT join_same($$SELECT x, n, t FROM (SELECT count(*) AS n, sum(length(jsb.t)) AS t FROM jsp LEFT JOIN jsb ON jsp.k = jsb.k) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$);
RESET enable_material;
-- Semi and anti joins keep no inner column: jsp's keys, the larger side,
-- are the table.
SELECT join_property($$SELECT count(*), sum(length(jsb.t)) FROM jsb WHERE EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k)$$, 'Spilled Chunks')::int > 0 AS semi_spilled,
       join_property($$SELECT jsb.t FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k)$$, 'Spilled Chunks')::int > 0 AS anti_spilled;
SELECT join_same($$SELECT count(*), sum(length(jsb.t)) FROM jsb WHERE EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k)$$);
SELECT join_same($$SELECT jsb.t, jsb.n FROM jsb WHERE NOT EXISTS (SELECT 1 FROM jsp WHERE jsp.k = jsb.k)$$);
-- Rounds: at a hash_mem of 2 MB a partition on disk that fits in one
-- participant's is loaded into shared memory by every participant
-- together, each taking files, and probed by all; one too large, here
-- the key 100000 rows share, which statistics taken before them do not
-- show, is joined by one participant alone.
SET work_mem = '1MB';
-- jsouter, the smaller side, is built; the heavy key's rows come after
-- its statistics, which the planner scales by its pages. Its other rows
-- outgrow hash_mem several times over, so that partitions go to disk
-- whatever the participants' timing.
-- No autovacuum: its analyze would correct the planner's estimate.
CREATE TABLE jsouter (k int, s text) WITH (autovacuum_enabled = off);
INSERT INTO jsouter
SELECT CASE WHEN g % 11 = 0 THEN NULL ELSE g % 50000 END, 'q' || g FROM generate_series(1, 200000) AS g;
ANALYZE jsouter;
INSERT INTO jsouter SELECT 7, 'h' || g FROM generate_series(1, 100000) AS g;
CREATE TABLE jsheavy AS SELECT g % 60000 AS k, g AS w FROM generate_series(1, 400000) AS g;
ANALYZE jsheavy;
SELECT join_property($$SELECT count(*), sum(jsheavy.w) FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(jsheavy.w) FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k$$, 'Partitions Joined Together')::int > 0 AS together;
SELECT join_same($$SELECT count(*), sum(jsheavy.w), sum(length(jsouter.s)) FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
SELECT join_same($$SELECT jsouter.s, jsheavy.w FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k WHERE jsouter.s LIKE '%77'$$, false);
SELECT join_same($$SELECT count(*), count(jsheavy.w), sum(jsheavy.w) FROM jsouter LEFT JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
SELECT join_same($$SELECT jsouter.s FROM jsouter LEFT JOIN jsheavy ON jsouter.k = jsheavy.k WHERE jsheavy.w IS NULL$$, false);
SELECT join_same($$SELECT count(*), sum(length(jsouter.s)) FROM jsouter WHERE EXISTS (SELECT 1 FROM jsheavy WHERE jsheavy.k = jsouter.k)$$, false);
SELECT join_same($$SELECT jsouter.s FROM jsouter WHERE NOT EXISTS (SELECT 1 FROM jsheavy WHERE jsheavy.k = jsouter.k)$$, false);
SELECT join_same($$SELECT count(*), sum(jsheavy.w) FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k AND jsheavy.w % 3 = jsouter.k % 3$$, false);
-- RIGHT and FULL: a round's marks in shared memory, its tail by the last
-- participant to leave it.
SELECT join_property($$SELECT count(*), count(jsouter.s), sum(jsheavy.w) FROM jsouter RIGHT JOIN jsheavy ON jsouter.k = jsheavy.k$$, 'Partitions Joined Together')::int > 0 AS together;
SELECT join_same($$SELECT count(*), count(jsouter.s), sum(jsheavy.w) FROM jsouter RIGHT JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
SELECT join_same($$SELECT count(*), count(jsouter.s), count(jsheavy.w), sum(jsheavy.w) FROM jsouter FULL JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
SELECT join_same($$SELECT jsouter.s, jsheavy.w FROM jsouter FULL JOIN jsheavy ON jsouter.k = jsheavy.k WHERE jsheavy.w % 1000 = 11 OR jsouter.s LIKE '%77'$$, false);
SELECT join_same($$SELECT count(*), count(jsouter.s), sum(jsheavy.w) FROM jsouter RIGHT JOIN jsheavy ON jsouter.k = jsheavy.k AND jsheavy.w % 3 = jsouter.k % 3$$, false);
SET parallel_leader_participation = off;
SELECT join_same($$SELECT count(*), sum(jsheavy.w), sum(length(jsouter.s)) FROM jsouter JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
SELECT join_same($$SELECT count(*), count(jsouter.s), count(jsheavy.w), sum(jsheavy.w) FROM jsouter FULL JOIN jsheavy ON jsouter.k = jsheavy.k$$, false);
RESET parallel_leader_participation;
SET enable_material = off;
SELECT join_same($$SELECT x, n, t FROM (SELECT count(*) AS n, sum(jsheavy.w) AS t FROM jsouter LEFT JOIN jsheavy ON jsouter.k = jsheavy.k) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$, false);
RESET enable_material;
SELECT count(*) AS temporary_files FROM pg_ls_tmpdir();
RESET work_mem;
-- Pruning by the join's keys in parallel: every participant prunes its
-- partitions alike, by the keys of a shared table, which each adds its
-- share of before the build's barrier, or of a table of its own; with the
-- leader and without.
SET enable_parallel_hash = on;
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS shared_range,
       join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkf ON jpr.k = jkf.k$$) AS shared_listed,
       join_property($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$, 'Shared Table') AS shared;
SELECT join_same($$SELECT jpr.k, jpr.v, jkm.w FROM jpr JOIN jkm ON jpr.k = jkm.k$$);
SELECT join_same($$SELECT jpr.k, jpr.v, jkf.w FROM jpr JOIN jkf ON jpr.k = jkf.k$$);
-- A shared table that spills reads the outer side in the build's phases,
-- each participant its share to the partitions: pruned there, once the
-- build's barrier has every participant's keys.
SET work_mem = '64kB';
SET hash_mem_multiplier = 1;
SELECT join_unread($$SELECT count(*) FROM jpr JOIN jkw ON jpr.k = jkw.k$$) AS shared_spilled,
       join_property($$SELECT count(*) FROM jpr JOIN jkw ON jpr.k = jkw.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*) FROM jpr JOIN jkw ON jpr.k = jkw.k$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkw ON jpr.k = jkw.k$$);
-- A shared table's partition on disk chosen again, with no limit of
-- partitions a check: each participant frees the tail it holds of it,
-- and one that holds none ends its check.
SET tessera.join_spill_spilled_weight = 10;
SET tessera.join_shared_spill_evictions = 0;
SET statement_timeout = '60s';
SELECT join_property($$SELECT count(*) FROM jpr JOIN jkw ON jpr.k = jkw.k$$, 'Shared Table') AS shared,
       join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkw ON jpr.k = jkw.k$$) AS chosen_again;
RESET statement_timeout;
RESET tessera.join_shared_spill_evictions;
RESET tessera.join_spill_spilled_weight;
RESET hash_mem_multiplier;
-- Outer rows of 17 texts of about 100 bytes: a batch fills a
-- partition's chunk of columns several times before the queue of
-- partitions to write is emptied, and the queue holds each partition
-- once.
SET work_mem = '256kB';
CREATE TABLE jqs_inner AS SELECT g % 1000 AS k, repeat('i', 40) || g AS pad FROM generate_series(1, 120000) AS g;
CREATE TABLE jqs_outer AS SELECT g % 1000 AS k, repeat(md5((g * 1)::text), 3) || 1 AS t1, repeat(md5((g * 2)::text), 3) || 2 AS t2, repeat(md5((g * 3)::text), 3) || 3 AS t3, repeat(md5((g * 4)::text), 3) || 4 AS t4, repeat(md5((g * 5)::text), 3) || 5 AS t5, repeat(md5((g * 6)::text), 3) || 6 AS t6, repeat(md5((g * 7)::text), 3) || 7 AS t7, repeat(md5((g * 8)::text), 3) || 8 AS t8, repeat(md5((g * 9)::text), 3) || 9 AS t9, repeat(md5((g * 10)::text), 3) || 10 AS t10, repeat(md5((g * 11)::text), 3) || 11 AS t11, repeat(md5((g * 12)::text), 3) || 12 AS t12, repeat(md5((g * 13)::text), 3) || 13 AS t13, repeat(md5((g * 14)::text), 3) || 14 AS t14, repeat(md5((g * 15)::text), 3) || 15 AS t15, repeat(md5((g * 16)::text), 3) || 16 AS t16, repeat(md5((g * 17)::text), 3) || 17 AS t17 FROM generate_series(1, 1000) AS g;
ANALYZE jqs_inner, jqs_outer;
SELECT join_property($$SELECT count(*), sum(length(o.t1) + length(o.t2) + length(o.t3) + length(o.t4) + length(o.t5) + length(o.t6) + length(o.t7) + length(o.t8) + length(o.t9) + length(o.t10) + length(o.t11) + length(o.t12) + length(o.t13) + length(o.t14) + length(o.t15) + length(o.t16) + length(o.t17) + length(i.pad)) FROM jqs_outer o JOIN jqs_inner i ON o.k = i.k$$, 'Shared Table') AS shared,
       join_property($$SELECT count(*), sum(length(o.t1) + length(o.t2) + length(o.t3) + length(o.t4) + length(o.t5) + length(o.t6) + length(o.t7) + length(o.t8) + length(o.t9) + length(o.t10) + length(o.t11) + length(o.t12) + length(o.t13) + length(o.t14) + length(o.t15) + length(o.t16) + length(o.t17) + length(i.pad)) FROM jqs_outer o JOIN jqs_inner i ON o.k = i.k$$, 'Spilled Chunks')::int > 0 AS spilled;
SELECT join_same($$SELECT count(*), sum(length(o.t1) + length(o.t2) + length(o.t3) + length(o.t4) + length(o.t5) + length(o.t6) + length(o.t7) + length(o.t8) + length(o.t9) + length(o.t10) + length(o.t11) + length(o.t12) + length(o.t13) + length(o.t14) + length(o.t15) + length(o.t16) + length(o.t17) + length(i.pad)) FROM jqs_outer o JOIN jqs_inner i ON o.k = i.k$$);
DROP TABLE jqs_inner, jqs_outer;
RESET work_mem;
SET enable_parallel_hash = off;
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS own_range;
SELECT join_same($$SELECT jpr.k, jpr.v, jkm.w FROM jpr JOIN jkm ON jpr.k = jkm.k$$);
SET enable_parallel_hash = on;
SET parallel_leader_participation = off;
-- Without the leader only the workers prune; EXPLAIN counts their pruning.
SELECT join_pruned($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS without_leader,
       join_unread($$SELECT count(*), sum(jpr.v) FROM jpr JOIN jkm ON jpr.k = jkm.k$$) AS unread;
SELECT join_same($$SELECT jpr.k, jpr.v, jkm.w FROM jpr JOIN jkm ON jpr.k = jkm.k$$);
SELECT join_same($$SELECT jpr.k, jpr.v, jkf.w FROM jpr JOIN jkf ON jpr.k = jkf.k$$);
RESET parallel_leader_participation;
-- With a worker's start in the partial scans' cost, the core's Append
-- gives jpr's partitions whole to one participant each. Where one
-- partition is expected to be left, fewer than the participants, the
-- join's outer side divides every partition among them instead (join_
-- divided: its Append's children are partial); where every partition is
-- left, the core's Append stays, the divided one counting a start once a
-- partition.
CREATE FUNCTION join_divided(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (FORMAT JSON, COSTS OFF) %s', query) INTO plan;
    RETURN jsonb_path_query_first(plan,
        '$[0]."Plan".** ? (@."Custom Plan Provider" == "TessHashJoin")."Plans"[0]."Plans"[0]."Parallel Aware"')::text;
END $$;
SET tessera.scan_parallel_setup_cost = 150;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jpr JOIN jkp ON jpr.k = jkp.k;
SELECT join_divided($$SELECT count(*) FROM jpr JOIN jkp ON jpr.k = jkp.k$$) AS one_left,
       join_divided($$SELECT count(*) FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE jkwide.k % 10 = 0$$) AS every_one_left,
       join_unread($$SELECT count(*) FROM jpr JOIN jkp ON jpr.k = jkp.k$$) AS unread;
SELECT join_same($$SELECT jpr.k, jpr.v FROM jpr JOIN jkp ON jpr.k = jkp.k$$);
SELECT join_same($$SELECT jpr.k, jkwide.k FROM jpr JOIN jkwide ON jpr.k = jkwide.k WHERE jkwide.k % 10 = 0$$);
SET tessera.scan_parallel_setup_cost = 0;
RESET enable_parallel_hash;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET enable_parallel_hash;

-- A text key: the table keeps the hash of the value, and the equality
-- stays a join clause, which decides the pair.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.v > jd.n AND jf.note = jd.label;
-- Keys of text, char(n) and bytea are hashed by their bytes, as their
-- types' functions hash them, and a value stored compressed by the
-- function, to the same hash: the same value compressed on one side and
-- stored plain on the other meets. char(n) without its trailing blanks;
-- under a nondeterministic collation every value by the function.
CREATE TABLE jbytes_l AS
SELECT i AS id, repeat('k', 3000) || (i % 10) AS big,
       ('c' || (i % 7) || repeat(' ', i % 3))::bpchar AS c,
       convert_to('b' || (i % 9), 'UTF8') AS b, 'Key' || (i % 5) AS t
FROM generate_series(1, 2000) AS i;
CREATE TABLE jbytes_r (id int, c char(4), b bytea, t text, big text);
ALTER TABLE jbytes_r ALTER COLUMN big SET STORAGE PLAIN;
INSERT INTO jbytes_r
SELECT i, ('c' || (i % 7))::char(4), convert_to('b' || (i % 9), 'UTF8'), 'key' || (i % 5),
       repeat('k', 3000) || (i % 10)
FROM generate_series(1, 300) AS i;
ANALYZE jbytes_l, jbytes_r;
SELECT (SELECT pg_column_compression(big) IS NOT NULL FROM jbytes_l LIMIT 1) AS left_compressed,
       (SELECT pg_column_compression(big) IS NULL FROM jbytes_r LIMIT 1) AS right_plain;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jbytes_l l JOIN jbytes_r r ON l.c = r.c;
SELECT join_same($$SELECT l.id, r.id FROM jbytes_l l JOIN jbytes_r r ON l.c = r.c$$);
SELECT join_same($$SELECT l.id, r.id FROM jbytes_l l JOIN jbytes_r r ON l.b = r.b$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jbytes_l l JOIN jbytes_r r ON l.big = r.big;
SELECT join_same($$SELECT l.id, r.id FROM jbytes_l l JOIN jbytes_r r ON l.big = r.big$$);
CREATE COLLATION join_nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jbytes_l l JOIN jbytes_r r ON l.t COLLATE join_nd = r.t;
SELECT join_same($$SELECT l.id, r.id FROM jbytes_l l JOIN jbytes_r r ON l.t COLLATE join_nd = r.t$$);
DROP COLLATION join_nd;
DROP TABLE jbytes_l, jbytes_r;
-- A key over an expression or a cast: its side's child computes it as a
-- target of its own, a word or a hashed value; on either side, both, in
-- a left and an anti join. A volatile one stays with the core.
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.note = upper(jd.label);
SELECT join_same($$SELECT jf.v, jd.id FROM jf JOIN jd ON upper(jf.note) = replace(upper(jd.label), 'D', 'F')$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id + 1;
SELECT join_same($$SELECT jf.v, jd.id FROM jf JOIN jd ON jf.fk = jd.id + 1$$);
SELECT join_same($$SELECT jf.v, jd.id FROM jf JOIN jd ON jf.fk - 1 = jd.id * 2 AND jf.fk8 = jd.id8 * 2 + 1$$);
SELECT join_same($$SELECT jf.v, jd.label FROM jf JOIN jd ON jf.fk::int8 = jd.id8 AND 'd' || jf.fk = lower(jd.label)$$);
SELECT join_same($$SELECT jf.v, jd.id FROM jf LEFT JOIN jd ON jf.fk = jd.id + 1$$);
SELECT join_same($$SELECT jf.v FROM jf WHERE NOT EXISTS (SELECT 1 FROM jd WHERE jd.id * 2 = jf.fk)$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id + (random() * 0)::int;
-- No path: the core's hash join disabled, the batch nodes off.
SET enable_hashjoin = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET enable_hashjoin;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET tessera.enable;

-- The node's path costs its children's costs as they are and its own
-- work on top: in every TessHashJoin of a plan the total is at least the
-- children's totals together, and the start at least the inner child's
-- total (the table is built before a row goes out) and at most the total.
-- A discount of the whole once compounded over nested joins (plan 8.10).
CREATE FUNCTION join_costs_carried(query text) RETURNS boolean
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || query INTO plan;
    RETURN NOT EXISTS (
        SELECT 1 FROM jsonb_path_query(plan, 'strict $.**') AS node
        WHERE jsonb_typeof(node) = 'object' AND node ->> 'Custom Plan Provider' = 'TessHashJoin'
          AND ((node ->> 'Total Cost')::numeric <
                   (node -> 'Plans' -> 0 ->> 'Total Cost')::numeric + (node -> 'Plans' -> 1 ->> 'Total Cost')::numeric
               OR (node ->> 'Startup Cost')::numeric < (node -> 'Plans' -> 1 ->> 'Total Cost')::numeric
               OR (node ->> 'Startup Cost')::numeric > (node ->> 'Total Cost')::numeric));
END $$;
SELECT join_costs_carried($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id$$) AS one,
       join_costs_carried($$SELECT count(*), sum(jd.n) FROM jf JOIN jd ON jf.fk = jd.id JOIN jdup ON jdup.k = jd.id$$) AS nested,
       join_costs_carried($$SELECT count(*) FROM jf WHERE NOT EXISTS (SELECT 1 FROM jd WHERE jd.id = jf.fk)$$) AS anti,
       join_costs_carried($$SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n$$) AS residual;
DROP FUNCTION join_costs_carried(text);

-- A serial build checks, before each batch, its chunks, its values, room
-- for two more chunks and the index its records will need, against
-- hash_mem. At 16 MiB, 420 000 keys of 32-byte records take 13 697 024
-- bytes of chunks and a 4 194 400-byte index: the build spills, and its
-- memory stays within hash_mem; 250 000 keys fit with their index.
CREATE TABLE jgbuild AS SELECT g AS k FROM generate_series(1, 420000) AS g;
CREATE TABLE jgprobe AS SELECT g % 450000 AS k FROM generate_series(1, 600000) AS g;
ANALYZE jgbuild, jgprobe;
SET work_mem = '16MB';
SET hash_mem_multiplier = 1;
SET max_parallel_workers_per_gather = 0;
-- spec: table-memory/A build whose index passes the limit
SELECT join_explain($$SELECT count(*) FROM jgprobe p JOIN jgbuild b USING (k)$$, false);
SELECT join_same($$SELECT count(*) FROM jgprobe p JOIN jgbuild b USING (k)$$);
-- spec: table-memory/A build that fits with its index
SELECT join_explain($$SELECT count(*) FROM jgprobe p JOIN jgbuild b USING (k) WHERE b.k <= 250000$$, false);
SELECT join_same($$SELECT count(*) FROM jgprobe p JOIN jgbuild b USING (k) WHERE b.k <= 250000$$);
-- At 13 MiB, 262 144 keys take 8 454 144 bytes of chunks and need a
-- 2 097 248-byte index; one key more needs a 4 194 400-byte one. A build
-- of 262 150 keys spills in its last batch; one whose filter rejects six
-- of them, in its last batches, counts the rows they select and fits.
CREATE TABLE jgbound AS SELECT g AS k FROM generate_series(1, 262150) AS g;
ANALYZE jgbound;
SET work_mem = '13MB';
-- spec: table-memory/A last batch that doubles the index
SELECT join_explain($$SELECT count(*) FROM jgprobe p JOIN jgbound b USING (k)$$, false);
SELECT join_same($$SELECT count(*) FROM jgprobe p JOIN jgbound b USING (k)$$);
-- spec: table-memory/Rows a filter rejects
SELECT join_explain($$SELECT count(*) FROM jgprobe p JOIN jgbound b USING (k) WHERE b.k <= 262140 OR b.k > 262146$$, false);
-- A table rebuilt after work_mem fell takes its chunks' length anew: at
-- 512 kB, 3000 keys take 295 008 bytes with their index and room for two
-- chunks of 64 kB, and stay in memory; with the 1 MiB chunks of 8 MiB
-- the room alone would pass the limit.
SET work_mem = '8MB';
-- spec: table-memory/A table rebuilt at a lower limit
SELECT join_explain($$SELECT s.n, set_config('work_mem', CASE s.n WHEN 1 THEN '8MB' ELSE '512kB' END, true) AS w, (SELECT count(*) FROM jgprobe p JOIN jgbound b ON p.k = b.k WHERE b.k <= 3000 + s.n - s.n) FROM generate_series(1, 2) AS s(n)$$, false);
RESET work_mem;
RESET hash_mem_multiplier;
RESET max_parallel_workers_per_gather;
DROP TABLE jgbuild, jgprobe, jgbound;

-- Gathered batches: a consumer's input copies the selected rows of sparse
-- batches whose columns are stored values into batches of 64 of its own
-- (runtime_input.h). jgo's filter f % 100 = 0 keeps a row in a hundred,
-- about one a batch; jgi pairs every other of them, and x is 0 on those
-- without a pair, so that 100 / x fails on rows a join drops.
CREATE TABLE jgo (k int, f int, x int, n numeric, t text) WITH (autovacuum_enabled = off);
INSERT INTO jgo SELECT g, g, CASE WHEN g % 200 = 100 THEN 0 ELSE 1 END, g / 4.0,
    repeat('t', g % 30) FROM generate_series(1, 20000) AS g;
CREATE TABLE jgi (k int, g int) WITH (autovacuum_enabled = off);
INSERT INTO jgi SELECT g * 200, g % 7 FROM generate_series(1, 100) AS g;
ANALYZE jgo, jgi;
-- The join's outer side through a filter: about 200 batches of a row or
-- two come to the join as 6, the first two as they are.
SELECT join_same($$SELECT count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$);
SELECT line FROM join_explain($$SELECT count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$) AS line
WHERE line ~ 'TessHashJoin|TessAgg|Gathered Rows|Probe Rows|Input Batches';
-- An aggregate above a join whose rounds keep a row in 200: it gathers
-- the join's output, stored values of the outer batch and the table.
SELECT join_same($$SELECT i.g, count(*), sum(o.n) FROM jgo o JOIN jgi i ON o.k = i.k GROUP BY i.g$$);
SELECT line FROM join_explain($$SELECT i.g, count(*), sum(o.n) FROM jgo o JOIN jgi i ON o.k = i.k GROUP BY i.g$$) AS line
WHERE line ~ 'TessHashJoin|TessAgg|Gathered Rows|Input Batches';
-- 100 / x fails on the rows the join or the aggregate's FILTER drops:
-- the copies read stored columns only, so it runs where it runs now.
SELECT join_same($$SELECT sum(100 / o.x), count(*) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$);
SELECT join_same($$SELECT sum(100 / x) FILTER (WHERE x <> 0), count(*) FROM jgo WHERE f % 100 = 0$$);
-- A volatile function in the join's output runs once a pair.
CREATE SEQUENCE jg_calls;
SELECT count(*), max(c) FROM (SELECT nextval('jg_calls') AS c FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0) AS q;
SELECT last_value FROM jg_calls;
SET tessera.enable = off;
SELECT count(*), max(c) FROM (SELECT nextval('jg_calls') AS c FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0) AS q;
SELECT last_value FROM jg_calls;
RESET tessera.enable;
DROP SEQUENCE jg_calls;
-- A limit right above the join turns its gathering off: it reads its
-- outer side a batch at a time.
SELECT line FROM join_explain($$SELECT o.k FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0 LIMIT 3$$) AS line
WHERE line ~ 'Limit|TessHashJoin|Gathered Rows';
-- A limit by a parameter, which each rescan computes again: without a
-- bound, with one and without again, the join gathers on the first and
-- the last scans only, as it does over the scans without a bound alone.
-- The outer side's first batches have no pair.
SELECT line FROM join_explain($$SELECT s.g, (SELECT count(*) FROM (SELECT o.k FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0 LIMIT CASE WHEN s.g = 2 THEN 1 END) AS q) FROM (VALUES (1), (2), (3)) AS s(g)$$) AS line
WHERE line ~ 'Limit|TessHashJoin|Gathered Rows';
SELECT line FROM join_explain($$SELECT s.g, (SELECT count(*) FROM (SELECT o.k FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0 LIMIT CASE WHEN s.g = 2 THEN 1 END) AS q) FROM (VALUES (1), (3)) AS s(g)$$) AS line
WHERE line ~ 'Limit|TessHashJoin|Gathered Rows';
SELECT join_same($$SELECT s.g, (SELECT count(*) FROM (SELECT o.k FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0 LIMIT CASE WHEN s.g = 2 THEN 1 END) AS q) FROM (VALUES (1), (2), (3)) AS s(g)$$);
-- INTERSECT and EXCEPT gather both sides apart; without sorts the
-- planner groups them in TessAgg.
SET enable_sort = off;
SELECT join_same($$SELECT k FROM jgo WHERE f % 100 = 0 INTERSECT SELECT k FROM jgo WHERE f % 300 = 0$$);
SELECT join_same($$SELECT k FROM jgo WHERE f % 100 = 0 EXCEPT ALL SELECT k FROM jgo WHERE f % 300 = 0$$);
SELECT line FROM join_explain($$SELECT k FROM jgo WHERE f % 100 = 0 INTERSECT SELECT k FROM jgo WHERE f % 300 = 0$$) AS line
WHERE line ~ 'TessAgg|Gathered Rows|Input Batches';
RESET enable_sort;
-- A partitioned table's parts, through TessAppend, gathered as one stream.
CREATE TABLE jgp (k int, f int) PARTITION BY RANGE (k);
CREATE TABLE jgp1 PARTITION OF jgp FOR VALUES FROM (1) TO (5001) WITH (autovacuum_enabled = off);
CREATE TABLE jgp2 PARTITION OF jgp FOR VALUES FROM (5001) TO (10001) WITH (autovacuum_enabled = off);
CREATE TABLE jgp3 PARTITION OF jgp FOR VALUES FROM (10001) TO (20001) WITH (autovacuum_enabled = off);
INSERT INTO jgp SELECT g, g FROM generate_series(1, 20000) AS g;
ANALYZE jgp;
SELECT join_same($$SELECT count(*), sum(k) FROM jgp WHERE f % 100 = 0$$);
SELECT line FROM join_explain($$SELECT count(*), sum(k) FROM jgp WHERE f % 100 = 0$$) AS line
WHERE line ~ 'TessAgg|TessAppend|Gathered Rows|Input Batches';
-- An index-only scan's batches, through a filter.
CREATE INDEX jgo_f ON jgo (f, k);
VACUUM jgo;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT join_same($$SELECT count(*), sum(k) FROM jgo WHERE f > 0 AND f % 100 = 0$$);
SELECT line FROM join_explain($$SELECT count(*), sum(k) FROM jgo WHERE f > 0 AND f % 100 = 0$$) AS line
WHERE line ~ 'TessAgg|Index Only Scan|Gathered Rows';
RESET enable_bitmapscan;
RESET enable_seqscan;
DROP INDEX jgo_f;
-- A join's compact output, over a table with duplicate keys: a residual
-- clause leaves a quarter of each batch of 64 pairs, and the aggregate
-- gathers them, the text column's values copied out of the join's
-- copies.
CREATE TABLE jgd (k int, v int) WITH (autovacuum_enabled = off);
INSERT INTO jgd SELECT (g % 50) * 200, g % 5 FROM generate_series(1, 2000) AS g;
ANALYZE jgd;
SET enable_sort = off;
SELECT join_same($$SELECT d.v, count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgd d ON o.k = d.k AND (o.k / 200 + d.v) % 4 = 0 GROUP BY d.v$$);
SELECT line FROM join_explain($$SELECT d.v, count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgd d ON o.k = d.k AND (o.k / 200 + d.v) % 4 = 0 GROUP BY d.v$$) AS line
WHERE line ~ 'TessAgg|TessHashJoin|Compact Batches|Gathered Rows';
RESET enable_sort;
-- An outer join's tails, and a join that spills its outer side and reads
-- it back.
SELECT join_same($$SELECT count(*), sum(i.k), sum(o.n) FROM jgo o RIGHT JOIN jgi i ON o.k = i.k AND o.f % 400 = 0$$);
SELECT line FROM join_explain($$SELECT count(*), sum(i.k), sum(o.n) FROM jgo o RIGHT JOIN jgi i ON o.k = i.k AND o.f % 400 = 0$$) AS line
WHERE line ~ 'TessAgg|TessHashJoin|Join Type|Gathered Rows';
SET work_mem = '64kB';
SELECT join_same($$SELECT count(*), sum(a.n), sum(length(b.t)) FROM jgo a JOIN jgo b ON a.k = b.k WHERE a.f % 100 = 0$$);
SELECT line FROM join_explain($$SELECT count(*), sum(a.n), sum(length(b.t)) FROM jgo a JOIN jgo b ON a.k = b.k WHERE a.f % 100 = 0$$) AS line
WHERE line ~ 'TessAgg|TessHashJoin|Batches:|Gathered Rows';
RESET work_mem;
-- Workers gather their own rows; with none launched the leader runs the
-- whole plan, its own rows handed on through TessGather as its child's.
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SELECT join_same($$SELECT count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$);
-- How the rows split among the participants varies: whether they gathered.
SELECT bool_or(line ~ 'TessGather') AS parallel, bool_or(line ~ 'Gathered Rows: [1-9]') AS gathered
FROM join_explain($$SELECT count(*), sum(o.n), sum(length(o.t)) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$) AS line;
SELECT join_same($$SELECT sum(100 / o.x), count(*) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$);
SET max_parallel_workers = 0;
SELECT join_same($$SELECT sum(100 / o.x), count(*) FROM jgo o JOIN jgi i ON o.k = i.k WHERE o.f % 100 = 0$$);
SELECT join_same($$SELECT sum(100 / x) FILTER (WHERE x <> 0), count(*) FROM jgo WHERE f % 100 = 0$$);
RESET max_parallel_workers;
RESET tessera.scan_worker_page_cost;
RESET tessera.scan_parallel_setup_cost;
RESET max_parallel_workers_per_gather;
RESET min_parallel_table_scan_size;
RESET parallel_tuple_cost;
RESET parallel_setup_cost;
DROP TABLE jgo, jgi, jgp, jgd;

DROP TABLE jd, jf, jdup, jsmall, jempty, jgrow, jbig, jpair, jbuild, jprobe, jhit, jmove, jref, jrefprobe, jrefgrow, jsb, jsp, jw70, jw1100, jsskew, jslong, jsouter, jsheavy;
DROP TABLE jpr, jpl, jph, jp2, jpk, jkf, jkm, jk8, jkn, jkl, jk2, jk19, jkw, jkx, jkwide, jkx2, jkp, jk1024, jk1025, jkr;
DROP FUNCTION jskew();
DROP FUNCTION jskewnull();
DROP FUNCTION jwide();
DROP FUNCTION jlong();
DROP FUNCTION join_property(text, text);
DROP FUNCTION join_explain(text, boolean);
DROP FUNCTION join_many();
DROP FUNCTION join_same(text, boolean);
DROP EXTENSION tessera;
