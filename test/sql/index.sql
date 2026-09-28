CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
LOAD 'tessera_limit';

-- The same rows with Tessera on and off, as text, in the same multiset.
CREATE FUNCTION index_same(query text) RETURNS text
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
    PERFORM set_config('tessera.enable', 'on', true);
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('on: %s off: %s', with_tessera, without_tessera);
    END IF;
    RETURN 'same';
END
$$;

-- EXPLAIN ANALYZE with the counters that vary from run to run as N.
CREATE FUNCTION index_explain(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query
    LOOP
        RETURN NEXT regexp_replace(line, 'Index Searches: \d+', 'Index Searches: N');
    END LOOP;
END
$$;

-- k scattered over the pages, NULL in every 13th row; w of 97 values; t
-- text. The bitmap scans are the planner's only choice here.
CREATE TABLE index_t AS
SELECT g AS id,
       CASE WHEN g % 13 = 0 THEN NULL ELSE (g::bigint * 7919 % 30000)::int END AS k,
       g % 97 AS w, 'v' || (g % 1000) AS t
FROM generate_series(1, 30000) AS g;
CREATE INDEX index_t_k ON index_t (k);
CREATE INDEX index_t_w ON index_t (w);
VACUUM ANALYZE index_t;
SET enable_seqscan = off;
SET enable_indexscan = off;
SET max_parallel_workers_per_gather = 0;

-- Bitmap Heap Scan: the node reads the bitmap's pages in batches of rows
-- from page after page, the filter above evaluates every clause, the
-- index's recheck among them.
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_t WHERE k < 500;
SELECT index_explain($$SELECT count(*), sum(w) FROM index_t WHERE k < 500$$);
SELECT index_same($$SELECT count(*), sum(w) FROM index_t WHERE k < 500$$);
SELECT index_same($$SELECT id, k, t FROM index_t WHERE k < 300$$);
SELECT index_same($$SELECT id, k + w, upper(t) FROM index_t WHERE k BETWEEN 1000 AND 1300$$);
SELECT index_same($$SELECT count(*), max(id) FROM index_t WHERE k IS NULL$$);
SELECT index_same($$SELECT count(*) FROM index_t WHERE k < 0$$);
SELECT index_same($$SELECT id FROM index_t WHERE k < 2000 AND t LIKE 'v1%'$$);
-- BitmapAnd and BitmapOr, whose pages give few rows: the node reads them
-- all the same at tessera.bitmap_page_rows 0.
SET tessera.bitmap_page_rows = 0;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_t WHERE k < 3000 AND w = 5;
SELECT index_same($$SELECT count(*), sum(id) FROM index_t WHERE k < 3000 AND w = 5$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_t WHERE k < 300 OR w = 5;
SELECT index_same($$SELECT count(*), sum(id) FROM index_t WHERE k < 300 OR w = 5$$);
SELECT index_same($$SELECT id, w FROM index_t WHERE (k < 300 OR w = 5) AND id % 2 = 0$$);
RESET tessera.bitmap_page_rows;
-- Below 2 rows a page by the planner's estimate, the core's bitmap heap
-- scan, which the aggregate above reads through the pack node.
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_t WHERE k < 3000 AND w = 5;
-- A bitmap past work_mem keeps whole pages (lossy), every row of them
-- rechecked by the filter: 60000 wide rows on about 1700 pages.
CREATE TABLE index_wide AS
SELECT g AS id, (g::bigint * 7919 % 60000)::int AS k, g % 97 AS w, repeat('x', 200) AS pad
FROM generate_series(1, 60000) AS g;
CREATE INDEX index_wide_k ON index_wide (k);
VACUUM ANALYZE index_wide;
SET work_mem = '64kB';
SELECT line FROM index_explain($$SELECT count(*), sum(w) FROM index_wide WHERE k < 30000$$) AS line
WHERE line ~ 'Lossy Heap Blocks: [1-9]|Rows Removed by Batch Filter: [1-9]';
SELECT index_same($$SELECT count(*), sum(w) FROM index_wide WHERE k < 30000$$);
SELECT index_same($$SELECT id, k FROM index_wide WHERE k < 20000 AND w < 3$$);
RESET work_mem;
DROP TABLE index_wide;
-- Updated and deleted rows: the pages' visible tuples, HOT chains
-- followed, are the ones read.
UPDATE index_t SET t = t || 'x' WHERE id % 7 = 0;
UPDATE index_t SET k = k + 1 WHERE id % 11 = 0;
DELETE FROM index_t WHERE id % 17 = 0;
SELECT index_same($$SELECT id, k, t FROM index_t WHERE k < 400$$);
SELECT index_same($$SELECT count(*), sum(k) FROM index_t WHERE k < 5000 AND w = 7$$);
-- Rescan: a parameter of the index condition changes for every outer row,
-- and the bitmap is built again.
EXPLAIN (COSTS OFF)
SELECT x, (SELECT count(*) FROM index_t WHERE w < x + 20 AND k < 20000) FROM generate_series(1, 3) AS x;
SELECT index_same($$SELECT x, (SELECT count(*) FROM index_t WHERE w < x + 20 AND k < 20000),
                          (SELECT sum(k) FROM index_t WHERE w < x + 10 AND k < 20000) FROM generate_series(1, 5) AS x$$);
-- A limit above stops the reads.
SELECT count(*) FROM (SELECT id FROM index_t WHERE k < 5000 LIMIT 10) AS q;

RESET enable_seqscan;
RESET enable_indexscan;
RESET max_parallel_workers_per_gather;
DROP TABLE index_t;
DROP FUNCTION index_explain(text);
DROP FUNCTION index_same(text);
DROP EXTENSION tessera;
