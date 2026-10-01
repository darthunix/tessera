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
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query
    LOOP
        -- VERBOSE adds the output lists and each worker's rows: not checked here.
        CONTINUE WHEN line ~ '^\s*(Output|Worker \d+):';
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

-- Index Scan: the node takes the core's index scan's rows, in the index's
-- order, into batches of any pages; only where the index is in the
-- table's order (correlation 0.8 at least) and a thousand rows come at
-- least, else the core's scan stays.
CREATE TABLE index_o AS
SELECT g AS id, CASE WHEN g % 11 = 0 THEN NULL ELSE g % 50 END AS w, 'o' || g AS t
FROM generate_series(1, 20000) AS g;
CREATE INDEX index_o_id ON index_o (id);
CREATE INDEX index_o_desc ON index_o (id DESC, w);
VACUUM ANALYZE index_o;
SET enable_indexscan = on;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w), max(t) FROM index_o WHERE id < 5000;
SELECT index_same($$SELECT count(*), sum(w), count(w), max(t) FROM index_o WHERE id < 5000$$);
EXPLAIN (COSTS OFF) SELECT id, t FROM index_o WHERE id BETWEEN 3000 AND 6000 AND w < 10 ORDER BY id;
SELECT index_same($$SELECT string_agg(id::text, ',' ORDER BY n) FROM (SELECT id, row_number() OVER () AS n FROM (SELECT id FROM index_o WHERE id BETWEEN 3000 AND 6000 AND w < 10 ORDER BY id) AS s) AS q$$);
EXPLAIN (COSTS OFF) SELECT id, t FROM index_o WHERE id > 15000 ORDER BY id DESC;
SELECT index_same($$SELECT string_agg(t, ',' ORDER BY n) FROM (SELECT t, row_number() OVER () AS n FROM (SELECT id, t FROM index_o WHERE id > 15000 ORDER BY id DESC) AS s) AS q$$);
SELECT index_same($$SELECT id, upper(t) FROM index_o WHERE id < 2000 AND t LIKE 'o1%'$$);
-- Without clauses, in the index's order for a merge join or an ordered
-- aggregate above.
EXPLAIN (COSTS OFF) SELECT id, t FROM index_o ORDER BY id OFFSET 10;
SELECT index_same($$SELECT string_agg(t, ',' ORDER BY n) FROM (SELECT t, row_number() OVER () AS n FROM (SELECT id, t FROM index_o ORDER BY id OFFSET 10) AS s) AS q$$);
-- Short scans and a small limit stay the core's, below
-- tessera.index_min_rows, and so does an index out of the table's order,
-- below tessera.index_min_correlation; at 0 the node takes them.
EXPLAIN (COSTS OFF) SELECT * FROM index_o WHERE id = 77;
EXPLAIN (COSTS OFF) SELECT id, t FROM index_o WHERE id > 100 ORDER BY id LIMIT 5;
SET tessera.index_min_rows = 0;
SET tessera.index_min_correlation = 0;
EXPLAIN (COSTS OFF) SELECT * FROM index_o WHERE id = 77;
EXPLAIN (COSTS OFF) SELECT id, t FROM index_o WHERE id > 100 ORDER BY id LIMIT 5;
SELECT index_same($$SELECT * FROM index_o WHERE id = 77$$);
SELECT index_same($$SELECT id, t FROM index_o WHERE id > 100 ORDER BY id LIMIT 5$$);
RESET tessera.index_min_rows;
RESET tessera.index_min_correlation;
CREATE TABLE index_s AS
SELECT g AS id, (g::bigint * 7919 % 20000)::int AS k, 's' || g AS t FROM generate_series(1, 20000) AS g;
CREATE INDEX index_s_k ON index_s (k);
VACUUM ANALYZE index_s;
EXPLAIN (COSTS OFF) SELECT count(*), max(t) FROM index_s WHERE k < 5000;
SET tessera.index_min_correlation = 0;
EXPLAIN (COSTS OFF) SELECT count(*), max(t) FROM index_s WHERE k < 5000;
SELECT index_same($$SELECT count(*), max(t) FROM index_s WHERE k < 5000$$);
SELECT index_same($$SELECT string_agg(t, ',' ORDER BY n) FROM (SELECT t, row_number() OVER () AS n FROM (SELECT k, t FROM index_s WHERE k < 3000 ORDER BY k) AS s) AS q$$);
RESET tessera.index_min_correlation;
DROP TABLE index_s;
-- Updated rows, and a rescan with a parameter of the index condition.
UPDATE index_o SET t = t || 'u' WHERE id % 5 = 0;
SELECT index_same($$SELECT count(*), max(t) FROM index_o WHERE id < 6000$$);
SET tessera.index_min_rows = 0;
EXPLAIN (COSTS OFF)
SELECT x, (SELECT max(t) FROM index_o WHERE id BETWEEN x * 1000 AND x * 1000 + 3000) FROM generate_series(1, 4) AS x;
SELECT index_same($$SELECT x, (SELECT max(t) FROM index_o WHERE id BETWEEN x * 1000 AND x * 1000 + 3000) FROM generate_series(1, 4) AS x$$);
RESET tessera.index_min_rows;
-- A rescan without a parameter in the middle of the index scan: the inner
-- side of a nested loop, a limit's rows, read again from the first.
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
EXPLAIN (COSTS OFF)
SELECT x, count(t), max(t) FROM generate_series(1, 3) AS x LEFT JOIN (SELECT id, t FROM index_o WHERE id < 5000 ORDER BY id LIMIT 2000) AS s ON x > 0 GROUP BY x;
SELECT index_same($$SELECT x, count(t), max(t) FROM generate_series(1, 3) AS x LEFT JOIN (SELECT id, t FROM index_o WHERE id < 5000 ORDER BY id LIMIT 2000) AS s ON x > 0 GROUP BY x$$);
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
DROP TABLE index_o;
RESET enable_bitmapscan;

-- Index Only Scan: the node drives the core's index-only scan, the table
-- AM filling its slot from the index tuple, and copies the rows into
-- batches of the index's columns, in the index's order; the filter above
-- evaluates every clause. Any correlation will do: an all-visible row
-- reads no page of the table.
CREATE TABLE index_i AS
SELECT g AS id, CASE WHEN g % 17 = 0 THEN NULL ELSE (g::bigint * 7919 % 20000)::int END AS k,
       g % 50 AS w, 'i' || (g % 3000) AS t, g * 2 AS x
FROM generate_series(1, 20000) AS g;
CREATE INDEX index_i_k ON index_i (k);
CREATE INDEX index_i_wt ON index_i (w, t) INCLUDE (x);
CREATE INDEX index_i_t ON index_i (t);
VACUUM ANALYZE index_i;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM index_i WHERE k < 5000;
SELECT index_explain($$SELECT count(*), sum(k) FROM index_i WHERE k < 5000$$);
SELECT index_same($$SELECT count(*), sum(k), count(k) FROM index_i WHERE k < 5000$$);
SELECT index_same($$SELECT count(*) FROM index_i WHERE k IS NULL$$);
-- In the index's order and backwards, to a limit.
SELECT index_same($$SELECT string_agg(k::text, ',' ORDER BY n) FROM (SELECT k, row_number() OVER () AS n FROM (SELECT k FROM index_i WHERE k < 3000 ORDER BY k LIMIT 1500) AS s) AS q$$);
SELECT index_same($$SELECT string_agg(k::text, ',' ORDER BY n) FROM (SELECT k, row_number() OVER () AS n FROM (SELECT k FROM index_i WHERE k > 15000 ORDER BY k DESC LIMIT 1500) AS s) AS q$$);
-- A composite index with an included column: a clause on its second
-- column, text keys copied as the scan moves from page to page.
EXPLAIN (COSTS OFF) SELECT w, t, x FROM index_i WHERE w < 20 AND t LIKE 'i1%';
SELECT index_same($$SELECT w, t, x FROM index_i WHERE w < 20 AND t LIKE 'i1%'$$);
SELECT index_same($$SELECT w, count(*), max(t), sum(x) FROM index_i WHERE w < 25 GROUP BY w$$);
SELECT index_same($$SELECT string_agg(t, ',' ORDER BY n) FROM (SELECT t, row_number() OVER () AS n FROM (SELECT t FROM index_i WHERE t > 'i2' ORDER BY t) AS s) AS q$$);
-- Rows to a row-wise parent.
SELECT index_same($$SELECT k FROM index_i WHERE k BETWEEN 1000 AND 4000$$);
-- Short scans stay the core's, below tessera.index_min_rows.
EXPLAIN (COSTS OFF) SELECT k FROM index_i WHERE k = 77;
-- Pages not all visible: the table AM reads them for the rows' visibility.
UPDATE index_i SET x = x + 1 WHERE id % 7 = 0;
DELETE FROM index_i WHERE id % 11 = 0;
SELECT index_explain($$SELECT count(*), sum(k) FROM index_i WHERE k < 5000$$);
SELECT index_same($$SELECT count(*), sum(k) FROM index_i WHERE k < 5000$$);
SELECT index_same($$SELECT w, count(*), sum(x) FROM index_i WHERE w < 25 GROUP BY w$$);
-- Rescan with a parameter of the index condition for every outer row, and
-- an initplan's value as a key, whose rows the planner guesses few.
SET tessera.index_min_rows = 0;
EXPLAIN (COSTS OFF)
SELECT y, (SELECT count(*) FROM index_i WHERE k BETWEEN y * 1000 AND y * 1000 + 4000) FROM generate_series(1, 4) AS y;
SELECT index_same($$SELECT y, (SELECT count(*) FROM index_i WHERE k BETWEEN y * 1000 AND y * 1000 + 4000) FROM generate_series(1, 4) AS y$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM index_i WHERE k < (SELECT 6000);
SELECT index_same($$SELECT count(*), sum(k) FROM index_i WHERE k < (SELECT 6000)$$);
RESET tessera.index_min_rows;
-- Nothing found.
SELECT index_same($$SELECT count(*), max(k) FROM index_i WHERE k < 0$$);
RESET enable_bitmapscan;
DROP TABLE index_i;

-- BRIN: its bitmap names whole ranges of pages (lossy), every row of which
-- the filter rechecks in batches, dates too. minmax, minmax-multi and bloom
-- classes; NULL days and IS NULL; rows added after the index's summary.
CREATE TABLE index_b AS
SELECT g AS id, CASE WHEN g % 50 = 0 THEN NULL ELSE date '2020-01-01' + g / 100 END AS d,
       timestamp '2020-01-01' + g * interval '1 minute' AS ts, g % 7 AS w
FROM generate_series(1, 30000) AS g;
CREATE INDEX index_b_d ON index_b USING brin (d) WITH (pages_per_range = 1);
CREATE INDEX index_b_ts ON index_b USING brin (ts timestamp_minmax_multi_ops) WITH (pages_per_range = 4);
CREATE INDEX index_b_w ON index_b USING brin (w int4_bloom_ops);
VACUUM ANALYZE index_b;
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_bitmapscan = on;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_b WHERE d BETWEEN '2020-02-01' AND '2020-02-10';
SELECT line FROM index_explain($$SELECT count(*), sum(w) FROM index_b WHERE d BETWEEN '2020-02-01' AND '2020-02-10'$$) AS line
WHERE line ~ 'Heap Blocks: [1-9]|Exact Heap Blocks|Rows Removed by Batch Filter: [1-9]';
SELECT index_same($$SELECT count(*), sum(w) FROM index_b WHERE d BETWEEN '2020-02-01' AND '2020-02-10'$$);
-- A day of 100 rows, which the core's estimate spreads over a page each:
-- the ranges' pages give all their rows all the same.
EXPLAIN (COSTS OFF) SELECT id, d FROM index_b WHERE d = '2020-03-05';
SELECT index_same($$SELECT id, d FROM index_b WHERE d = '2020-03-05'$$);
SELECT index_same($$SELECT count(*), max(id) FROM index_b WHERE d IS NULL AND id < 3000$$);
SELECT index_same($$SELECT count(*), sum(id) FROM index_b WHERE ts >= '2020-01-05 10:00' AND ts < '2020-01-06'$$);
SELECT index_same($$SELECT count(*), sum(id) FROM index_b WHERE w = 3 AND d < '2020-01-20'$$);
INSERT INTO index_b SELECT g, date '2020-01-01' + g / 100, timestamp '2020-01-01' + g * interval '1 minute', g % 7
FROM generate_series(30001, 31000) AS g;
SELECT index_same($$SELECT count(*), sum(w) FROM index_b WHERE d BETWEEN '2020-02-01' AND '2020-02-10' OR d > '2020-10-20'$$);
RESET enable_bitmapscan;
DROP TABLE index_b;

RESET enable_seqscan;
RESET enable_indexscan;

-- The scans ranked by the node's times (tessera.scan_page_cost and the
-- rest): the node's full scan is faster than its index scans far below
-- the share of rows where the core's are equal, so past that share it
-- costs just below them. k scattered, id in the table's order, both
-- indexed; a few rows keep the index, many take the full scan, an
-- index-only scan, an index scan and a bitmap alike; an order with a
-- limit keeps the index. A bitmap of the ordered id reads the pages its
-- rows fill in order, fewer than the core estimates for rows at random,
-- and is faster than the index scan: the node's bitmap takes its place,
-- though the core's add_path had dropped the core's.
CREATE TABLE index_r AS
SELECT g AS id, (g::bigint * 7919 % 60000)::int AS k, g % 97 AS w FROM generate_series(1, 60000) AS g;
CREATE INDEX index_r_id ON index_r (id);
CREATE INDEX index_r_k ON index_r (k);
VACUUM ANALYZE index_r;
SET tessera.index_min_rows = 0;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 3000;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 36000;
SELECT index_same($$SELECT count(*), sum(k) FROM index_r WHERE k < 36000$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 3000;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 36000;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 36000$$);
SET enable_indexscan = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE k < 3000;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE k < 36000;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE k < 36000$$);
RESET enable_indexscan;
EXPLAIN (COSTS OFF) SELECT k FROM index_r WHERE k < 36000 ORDER BY k LIMIT 10;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 3000$$);
-- A clause by rows past the index's sees the rows the index gives, as it
-- sees those the full scan's first clause leaves: it keeps the full scan
-- at a quarter of the rows.
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 15000 AND (w # 0) > -1;
-- With workers, past that share the node's partial full scan in place of
-- the core's parallel index scans (the core dropped its serial and its
-- partial sequential scans for them). The workers start at no cost in the
-- node's model either, which would keep so small a table serial.
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 21000;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 21000$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 21000;
SELECT index_same($$SELECT count(*), sum(k) FROM index_r WHERE k < 21000$$);
-- Below it, the node over the core's parallel index-only and index scans,
-- which divide the work among the participants through their own shared
-- memory: the node's chunk holds only its counters. Without the leader,
-- and rescanned under the gather in a join. Only where the model finds
-- the participant's time and the workers' start below the fastest serial
-- scan: at the default start the node takes no parallel scan of so small
-- a table, and the core's stands under TessPack (parallel_setup_cost is 0
-- here).
SET enable_bitmapscan = off;
RESET tessera.scan_parallel_setup_cost;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 9000;
-- A worker at the leader's pace too, which keeps these index scans of a
-- small table clear of the partial full scan.
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.index_worker_share = 1;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 9000;
SELECT index_same($$SELECT count(*), sum(k) FROM index_r WHERE k < 9000$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 3000;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 3000$$);
-- A worker of a parallel index scan reads at tessera.index_worker_share of
-- the leader's pace, the leader alone while the workers start: at a start
-- of 380 the node's parallel index-only scan of 9000 rows is taken where a
-- worker keeps the leader's pace, the leader's head start deciding it; at
-- a tenth of it the partial full scan takes its place.
SET tessera.scan_parallel_setup_cost = 380;
SET tessera.index_worker_share = 1;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 9000;
SET tessera.index_worker_share = 0.1;
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 9000;
SET tessera.index_worker_share = 1;
SET tessera.scan_parallel_setup_cost = 0;
SET parallel_leader_participation = off;
SELECT index_same($$SELECT count(*), sum(k) FROM index_r WHERE k < 9000$$);
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 3000$$);
RESET parallel_leader_participation;
SET enable_material = off;
EXPLAIN (COSTS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM index_r WHERE k < 9000) AS ss RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT index_same($$SELECT x, n, s FROM (SELECT count(*) AS n, sum(k) AS s FROM index_r WHERE k < 9000) AS ss
                    RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
RESET enable_material;
-- The node's parallel bitmap: one participant builds the bitmap into the
-- query's shared memory, the others waiting, and all take its pages
-- through a shared iterator. The ordered id's is made where the core's
-- parallel index scan pushed the core's partial bitmap out; the scattered
-- k's (the full scan off) without the leader too, and rescanned under the
-- gather in a join, built anew.
RESET enable_bitmapscan;
SET tessera.bitmap_build_cost = 0;
SET tessera.bitmap_build_scatter_cost = 0;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE id < 9000;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 9000$$);
SET enable_indexscan = off;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_r WHERE k < 9000;
SELECT index_same($$SELECT count(*), sum(w), min(id) FROM index_r WHERE k < 9000$$);
SET parallel_leader_participation = off;
SELECT index_same($$SELECT count(*), sum(w) FROM index_r WHERE id < 9000$$);
SELECT index_same($$SELECT count(*), sum(w), min(id) FROM index_r WHERE k < 9000$$);
RESET parallel_leader_participation;
SET enable_material = off;
EXPLAIN (COSTS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM index_r WHERE k < 9000) AS ss RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT index_same($$SELECT x, n, s FROM (SELECT count(*) AS n, sum(w) AS s FROM index_r WHERE k < 9000) AS ss
                    RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
RESET enable_material;
RESET enable_indexscan;
RESET enable_seqscan;
-- BRIN: its pages lie as its column's correlation lays them, as a btree's
-- do: serially the node's BRIN bitmap of 15 % of the rows beats the full
-- scan, which the correlation of 0 of an index not btree took for all
-- the pages; with workers, the parallel BRIN bitmap.
CREATE TABLE index_rb AS SELECT g AS id, g % 97 AS w FROM generate_series(1, 60000) AS g;
CREATE INDEX index_rb_id ON index_rb USING brin (id) WITH (pages_per_range = 4);
VACUUM ANALYZE index_rb;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_rb WHERE id BETWEEN 30000 AND 38999;
SELECT index_same($$SELECT count(*), sum(w) FROM index_rb WHERE id BETWEEN 30000 AND 38999$$);
SET max_parallel_workers_per_gather = 0;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM index_rb WHERE id BETWEEN 30000 AND 38999;
SET max_parallel_workers_per_gather = 2;
DROP TABLE index_rb;
RESET tessera.bitmap_build_cost;
RESET tessera.bitmap_build_scatter_cost;
RESET tessera.index_worker_share;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET enable_bitmapscan;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET min_parallel_index_scan_size;
SET max_parallel_workers_per_gather = 0;
-- A table past effective_cache_size keeps the core's costs: its pages
-- may come from the disk, where reading few of them counts.
SET effective_cache_size = '64kB';
EXPLAIN (COSTS OFF) SELECT count(*) FROM index_r WHERE k < 36000;
RESET effective_cache_size;
RESET tessera.index_min_rows;
DROP TABLE index_r;
RESET max_parallel_workers_per_gather;
DROP TABLE index_t;
DROP FUNCTION index_explain(text);
DROP FUNCTION index_same(text);
DROP EXTENSION tessera;
