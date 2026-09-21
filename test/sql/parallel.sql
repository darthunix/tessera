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
-- An aggregate the node does not compute: the core's partial aggregate over the rows.
EXPLAIN (COSTS OFF) SELECT count(c), count(*) FROM parallel_t WHERE a > 100;
SELECT parallel_same($$SELECT count(c), count(*) FROM parallel_t WHERE a > 100$$);
-- The switch off.
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a, b FROM parallel_t WHERE a > 4990;
RESET tessera.enable;

DROP TABLE parallel_t;
DROP FUNCTION parallel_same(text);
DROP FUNCTION plan_property(text, text, text);
DROP EXTENSION tessera;
