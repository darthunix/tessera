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

-- A dimension with a unique key, int8 copies of it and values past the
-- int4 range; a fact table whose keys repeat, miss and are NULL; a table
-- with three rows per key and NULL keys; a small one and an empty one.
CREATE TABLE jd (id int PRIMARY KEY, id8 bigint, big bigint, label text, n int);
INSERT INTO jd
SELECT g, g, g::bigint << 33, 'd' || g, CASE WHEN g % 5 = 0 THEN NULL ELSE g * 10 END
FROM generate_series(1, 300) AS g;
CREATE TABLE jf (fk int, fk8 bigint, fk_big bigint, v int, note text);
INSERT INTO jf
SELECT fk, fk, fk::bigint << 33, g, 'f' || g
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

-- A side of fewer than 64 rows, an empty side, a join over a join.
SELECT join_same($$SELECT jsmall.s, jd.label FROM jsmall JOIN jd ON jsmall.k = jd.id$$);
SELECT join_same($$SELECT jf.v, jempty.e FROM jf JOIN jempty ON jf.fk = jempty.k$$);
SELECT join_same($$SELECT jempty.e, jd.label FROM jempty JOIN jd ON jempty.k = jd.id$$);
EXPLAIN (COSTS OFF)
SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id JOIN jdup ON jd.id = jdup.k;
SELECT join_same($$SELECT jf.v, jd.label, jdup.w FROM jf JOIN jd ON jf.fk = jd.id JOIN jdup ON jd.id = jdup.k$$);

-- A build side larger than the planner thinks: the table grows.
CREATE TABLE jgrow (k int, g int);
ANALYZE jgrow;
INSERT INTO jgrow SELECT g % 400, g FROM generate_series(1, 20000) AS g;
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jsmall JOIN jgrow ON jsmall.k = jgrow.k$$);
SELECT join_same($$SELECT count(*), sum(jgrow.g) FROM jd JOIN jgrow ON jd.id = jgrow.k$$);

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

-- No path: another join type, a second clause, another key type, the
-- core's hash join disabled, the batch nodes off.
EXPLAIN (COSTS OFF) SELECT count(jd.id) FROM jf LEFT JOIN jd ON jf.fk = jd.id;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id AND jf.v > jd.n;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.note = jd.label;
SET enable_hashjoin = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET enable_hashjoin;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jf JOIN jd ON jf.fk = jd.id;
RESET tessera.enable;

DROP TABLE jd, jf, jdup, jsmall, jempty, jgrow;
DROP FUNCTION join_explain(text);
DROP FUNCTION join_many();
DROP FUNCTION join_same(text);
DROP EXTENSION tessera;
