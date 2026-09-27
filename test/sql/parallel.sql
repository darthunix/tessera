CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

-- The same result with Tessera on and off, as text; each side plans afresh.
CREATE FUNCTION parallel_same(query text) RETURNS text
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
    RETURN with_tessera::text;
END $$;

-- A property of one node of the plan that ran, as the leader reports it.
CREATE FUNCTION plan_property(query text, provider text, name text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == $p).%I', name)::jsonpath,
        jsonb_build_object('p', provider))::text;
END $$;

CREATE TABLE parallel_t (a int, b int, c text);
INSERT INTO parallel_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i END, i % 10, 'r' || i
FROM generate_series(1, 5000) AS i;
ANALYZE parallel_t;
SELECT pg_relation_size('parallel_t') / current_setting('block_size')::int AS pages \gset

-- Two workers even for a small table, in both modes.
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;

-- The partial path: the node in every participant over its share of the pages.
EXPLAIN (COSTS OFF) SELECT a, b FROM parallel_t WHERE a > 4990;
SELECT parallel_same($$SELECT a, b FROM parallel_t WHERE a > 4990$$);
-- A row-wise residual clause and computed targets, in the workers too.
SELECT parallel_same($$SELECT a, c FROM parallel_t WHERE a > 4990 AND c <> 'r4995'$$);
EXPLAIN (COSTS OFF) SELECT a + 1 AS next, c || '!' AS shout FROM parallel_t WHERE a > 4990;
SELECT parallel_same($$SELECT a + 1 AS next, c || '!' AS shout FROM parallel_t WHERE a > 4990$$);
-- Every page is read once across the participants: the leader sums their pages.
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 0$$, 'TessHeapScan', 'Pages')::int = :pages AS all_pages;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM parallel_t WHERE a > 4990;
-- Workers planned but not launched: the leader reads every page alone.
SET max_parallel_workers = 0;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM parallel_t WHERE a > 4990;
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 0$$, 'TessHeapScan', 'Pages')::int = :pages AS all_pages;
SELECT parallel_same($$SELECT a, b FROM parallel_t WHERE a > 4990$$);
RESET max_parallel_workers;
-- The leader does not take part: the workers' counters alone make the totals.
SET parallel_leader_participation = off;
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 0$$, 'TessHeapScan', 'Pages')::int = :pages AS all_pages;
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 0$$, 'TessHeapScan', 'Batch Size') AS batch_size;
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 0$$, 'TessFilter', 'Input Rows') AS filter_rows;
SELECT parallel_same($$SELECT a, b FROM parallel_t WHERE a > 4990$$);
RESET parallel_leader_participation;
-- A Gather rescanned in a join: the shared page handout starts over.
SET enable_material = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM parallel_t WHERE a > 4997) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT parallel_same($$SELECT x, n FROM (SELECT count(*) AS n FROM parallel_t WHERE a > 4997) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$);
-- A limit above the Gather stops it early, and a rescan sets it up anew.
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT a FROM parallel_t WHERE a > 100 LIMIT 3) AS s;
SELECT count(*) FROM (SELECT a FROM parallel_t WHERE a > 100 LIMIT 3) AS s;
SELECT plan_property($$SELECT a FROM parallel_t WHERE a > 100 LIMIT 3$$, 'TessHeapScan', 'Pages')::int
    BETWEEN 1 AND :pages AS some_pages;
EXPLAIN (COSTS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM (SELECT a FROM parallel_t WHERE a > 4997 LIMIT 2) AS l) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT parallel_same($$SELECT x, n FROM (SELECT count(*) AS n FROM (SELECT a FROM parallel_t WHERE a > 4997 LIMIT 2) AS l) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$);
RESET enable_material;
-- A parameter of a generic plan reaches the workers.
PREPARE above(int) AS SELECT count(*), min(a) FROM parallel_t WHERE a > $1;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE above(4990);
EXECUTE above(4990);
EXECUTE above(10);
RESET plan_cache_mode;
DEALLOCATE above;
-- An order above the Gather.
EXPLAIN (COSTS OFF) SELECT a FROM parallel_t WHERE a > 4990 ORDER BY a;
SELECT parallel_same($$SELECT a FROM parallel_t WHERE a > 4990 ORDER BY a$$);
-- An error raised in a worker reaches the client.
SELECT a + 2147483647 FROM parallel_t WHERE a > 4990;
-- The partial aggregate in every participant, the core's Finalize Aggregate
-- combining their values above the Gather; without clauses, over the scan.
EXPLAIN (COSTS OFF)
SELECT count(*), count(a), sum(a), min(a), max(b) FROM parallel_t WHERE a > 100;
SELECT parallel_same($$SELECT count(*), count(a), sum(a), min(a), max(b) FROM parallel_t WHERE a > 100$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM parallel_t;
SELECT parallel_same($$SELECT count(*), sum(a) FROM parallel_t$$);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sum(a) FROM parallel_t WHERE a > 100;
-- Chains and row-wise expressions as arguments, expressions above, HAVING.
SELECT parallel_same($$SELECT sum(a + b), max(a * 2), min(-a), sum(CASE WHEN b > 5 THEN a ELSE 0 END)
    FROM parallel_t WHERE a > 100$$);
SELECT parallel_same($$SELECT sum(a) / count(*) AS mean, count(*) + 1 FROM parallel_t WHERE a > 100$$);
SELECT parallel_same($$SELECT sum(a) FROM parallel_t WHERE a > 100 HAVING count(*) > 100$$);
SELECT parallel_same($$SELECT sum(a) FROM parallel_t WHERE a > 100 HAVING count(*) > 100000$$);
-- Participants without rows: a count of zero and NULL values to combine.
SELECT parallel_same($$SELECT count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a < 5$$);
SELECT parallel_same($$SELECT count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 1000000$$);
-- The leader does not take part: the aggregate's rows are the workers' alone.
SET parallel_leader_participation = off;
SELECT plan_property($$SELECT count(*) FROM parallel_t WHERE a > 100$$, 'TessAgg', 'Input Rows') AS agg_rows;
SELECT parallel_same($$SELECT count(*), sum(a) FROM parallel_t WHERE a > 100$$);
RESET parallel_leader_participation;
-- A parameter of a generic plan in an argument.
PREPARE shifted(int) AS SELECT sum(a + $1) FROM parallel_t WHERE a > 4990;
SET plan_cache_mode = force_generic_plan;
EXECUTE shifted(1);
EXECUTE shifted(1000);
RESET plan_cache_mode;
DEALLOCATE shifted;
-- GROUP BY under a Gather: a table of groups in each participant, the
-- core's Finalize HashAggregate merging them and applying HAVING.
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b;
SELECT parallel_same($$SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b$$);
SELECT parallel_same($$SELECT a % 7, count(*), count(a) FROM parallel_t GROUP BY a % 7 HAVING count(*) > 100$$);
SELECT plan_property($$SELECT b, sum(a) FROM parallel_t GROUP BY b$$, 'TessAgg', 'Groups') AS groups;
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT b, sum(a) FROM parallel_t GROUP BY b$$);
RESET parallel_leader_participation;
-- Partial groups past hash_mem go out early, and the table starts anew:
-- the Finalize Aggregate merges a group's partials, and nothing is written.
-- A group's rows come together, so a table folds many before it fills.
CREATE TABLE parallel_groups AS
SELECT g / 60 AS k, g AS v FROM generate_series(1, 300000) AS g;
ANALYZE parallel_groups;
SET work_mem = '64kB';
-- The core's sorted grouping would leave no partial hash aggregate to take.
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_groups GROUP BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_groups GROUP BY k) AS q$$);
SELECT plan_property($$SELECT k, count(*) FROM parallel_groups GROUP BY k$$, 'TessAgg', 'Early Emits')::int > 0 AS early,
       plan_property($$SELECT k, count(*) FROM parallel_groups GROUP BY k$$, 'TessAgg', 'Disk Usage') AS disk;
-- Groups spread over the input fold nothing before the table fills, a
-- group per row read: sending the table up would hand the Finalize
-- Aggregate every row. The groups go to disk instead, as a serial node's,
-- and out as partials once the input is done.
CREATE TABLE parallel_spread AS
SELECT g % 5000 AS k, g AS v FROM generate_series(1, 300000) AS g;
ANALYZE parallel_spread;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_spread GROUP BY k) AS q$$);
SELECT plan_property($$SELECT k, count(*) FROM parallel_spread GROUP BY k$$, 'TessAgg', 'Early Emits') IS NULL AS no_early,
       plan_property($$SELECT k, count(*) FROM parallel_spread GROUP BY k$$, 'TessAgg', 'Disk Usage')::int > 0 AS spilled;
RESET enable_sort;
RESET work_mem;
DROP TABLE parallel_groups, parallel_spread;
-- A batch node above a Gather: with the core's Gather (tessera.batch_gather
-- off) the pack under it takes the leader's rows in the Gather's child's
-- slot and the workers' in the Gather's own, two descriptors of one
-- layout, and deforms both by the first; TessGather gives batches, and
-- the pack goes.
CREATE TABLE parallel_pack AS
SELECT g % 5000 AS k, g AS v FROM generate_series(1, 300000) AS g;
ANALYZE parallel_pack;
SET work_mem = '256kB';
SET enable_sort = off;
SET cpu_tuple_cost = 0.05;
EXPLAIN (COSTS OFF) SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k) AS q$$);
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k) AS q$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k) AS q$$);
RESET tessera.batch_gather;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_pack GROUP BY k) AS q$$);
RESET parallel_leader_participation;
RESET cpu_tuple_cost;
RESET enable_sort;
RESET work_mem;
DROP TABLE parallel_pack;
-- An aggregate the node does not compute: the core's partial aggregate over the rows.
EXPLAIN (COSTS OFF) SELECT count(c), count(*) FROM parallel_t WHERE a > 100;
SELECT parallel_same($$SELECT count(c), count(*) FROM parallel_t WHERE a > 100$$);
-- TessGather: the workers send batches of rows through queues of their
-- own; 200000 rows with NULLs and text, values of up to 2000 bytes that
-- fill a message's half of its queue before its rows do, twenty columns
-- that narrow a message's rows, the workers alone, a limit that stops them
-- while they send, and the core's Gather with tessera.batch_gather off.
CREATE TABLE parallel_wide AS
SELECT g AS k, CASE WHEN g % 9 = 0 THEN NULL ELSE g % 1000 END AS a,
       CASE WHEN g % 11 = 0 THEN NULL ELSE repeat('t', g % 40) || g END AS t,
       CASE WHEN g % 997 = 0 THEN repeat('w', 2000) END AS w,
       g + 1 AS c1, g + 2 AS c2, g + 3 AS c3, g + 4 AS c4, g + 5 AS c5, g + 6 AS c6,
       g + 7 AS c7, g + 8 AS c8, g + 9 AS c9, g + 10 AS c10, g + 11 AS c11,
       g + 12 AS c12, g + 13 AS c13, g + 14 AS c14, g + 15 AS c15, g + 16 AS c16
FROM generate_series(1, 200000) AS g;
ANALYZE parallel_wide;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k % 3 <> 0;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, a, t, w FROM parallel_wide WHERE k % 3 <> 0 OFFSET 0) AS q$$);
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT * FROM parallel_wide WHERE k % 2 = 0 OFFSET 0) AS q$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, a, t, w FROM parallel_wide WHERE k % 3 <> 0 OFFSET 0) AS q$$);
SELECT count(*) FROM (SELECT k, t FROM parallel_wide WHERE a > 5 LIMIT 10) AS q;
RESET parallel_leader_participation;
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k % 3 <> 0;
RESET tessera.batch_gather;
-- At the core's costs of parallel work the node's own paths cost a row a
-- quarter of parallel_tuple_cost: a scan returning 8000 rows and a sort of
-- every row go parallel through TessGather and TessGatherMerge, and stay
-- serial with tessera.batch_gather off, where a row costs the core's
-- Gather the whole.
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
EXPLAIN (COSTS OFF) SELECT k, a FROM parallel_wide ORDER BY a, k;
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
EXPLAIN (COSTS OFF) SELECT k, a FROM parallel_wide ORDER BY a, k;
RESET tessera.batch_gather;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
DROP TABLE parallel_wide;
-- The switch off.
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a, b FROM parallel_t WHERE a > 4990;
RESET tessera.enable;

DROP TABLE parallel_t;
DROP FUNCTION parallel_same(text);
DROP FUNCTION plan_property(text, text, text);
DROP EXTENSION tessera;
