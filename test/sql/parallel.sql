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
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == $p).%I', name)::jsonpath,
        jsonb_build_object('p', provider))::text;
END $$;

-- EXPLAIN ANALYZE VERBOSE without what varies from run to run: each
-- worker's rows, the rows the gather took from each participant, and the
-- output lists VERBOSE adds.
CREATE FUNCTION parallel_explain(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
        CONTINUE WHEN line ~ '^\s*(Output|Worker \d+|Messages|Rows from Workers|Rows of the Leader):';
        RETURN NEXT line;
    END LOOP;
END $$;

-- The same of the node's partial grouping under a gather, whatever groups above.
CREATE FUNCTION partial_property(query text, name text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == "TessAgg" && @."Partial Mode" == "Partial").%I',
               name)::jsonpath)::text;
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
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
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
SELECT parallel_explain($$SELECT a FROM parallel_t WHERE a > 4990$$);
-- Workers planned but not launched: the leader reads every page alone.
SET max_parallel_workers = 0;
SELECT parallel_explain($$SELECT a FROM parallel_t WHERE a > 4990$$);
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
SELECT parallel_explain($$SELECT x, n FROM (SELECT count(*) AS n FROM parallel_t WHERE a > 4997) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$);
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
-- The partial aggregate in every participant, the node's final one merging
-- their values above TessGather; without clauses, over the scan.
EXPLAIN (COSTS OFF)
SELECT count(*), count(a), sum(a), min(a), max(b) FROM parallel_t WHERE a > 100;
SELECT parallel_same($$SELECT count(*), count(a), sum(a), min(a), max(b) FROM parallel_t WHERE a > 100$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM parallel_t;
SELECT parallel_same($$SELECT count(*), sum(a) FROM parallel_t$$);
SELECT parallel_explain($$SELECT sum(a) FROM parallel_t WHERE a > 100$$);
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
SELECT partial_property($$SELECT count(*) FROM parallel_t WHERE a > 100$$, 'Input Rows') AS agg_rows;
SELECT parallel_same($$SELECT count(*), sum(a) FROM parallel_t WHERE a > 100$$);
RESET parallel_leader_participation;
-- The aggregates the node folds itself, in every participant too (plan
-- 4.23, item 4b): sum and avg of numeric and bigint go up in the node's own
-- format, their words, the count of every value and the rest (longer
-- values, NaN, infinities), which the node's final aggregation merges at
-- the larger scale, the part past the bound to the rest; the others as the
-- core's own transition values, which the core's Finalize reads too: a
-- float's sum and extremes, avg's float8[] of N, Sx and Sxx, avg of
-- integers' int8[] of the count and the sum, sum(int2)'s int8, a numeric
-- extreme. The first half of the rows has scale 1, the second scale 3;
-- rows 1 to 300 are sums near the bound at scale 0 that one value of scale
-- 18 at the end meets; floats are halves, exact in any order.
CREATE TABLE parallel_fast AS
SELECT i AS id,
       (CASE WHEN i <= 300 THEN 999999999999999999
             WHEN i = 20000 THEN 0.000000000000000001
             WHEN i <= 10000 THEN (i / 10.0)::numeric(12, 1)
             ELSE (i / 1000.0)::numeric(12, 3) END)::numeric AS n,
       (CASE WHEN i % 997 = 0 THEN 'NaN' WHEN i % 13 = 0 THEN i * 12345678901234567890.5
             ELSE (i / 4.0)::numeric(12, 2) END)::numeric AS u,
       CASE WHEN i % 2 = 0 THEN i::bigint * 1000 ELSE 9223372036854775807 - i * 1000 END AS b,
       CASE WHEN i % 7 = 0 THEN NULL ELSE i * 7919 % 100000 - 50000 END AS i4,
       (i % 60000 - 30000)::int2 AS s,
       i / 2.0::float8 AS f8, (i % 1000) / 4.0::float4 AS f4
FROM generate_series(1, 20000) AS i;
ANALYZE parallel_fast;
EXPLAIN (COSTS OFF) SELECT sum(n), avg(u), sum(b), avg(i4), sum(s), max(f8) FROM parallel_fast;
SELECT parallel_same($$SELECT sum(n), avg(n), sum(u), avg(u), min(u), max(u), sum(b), avg(b) FROM parallel_fast$$);
SELECT parallel_same($$SELECT avg(i4), avg(s), sum(s), sum(f8), avg(f8), min(f8), max(f8), sum(f4), avg(f4), min(f4), max(f4) FROM parallel_fast$$);
SELECT parallel_same($$SELECT sum(n), avg(n) FROM parallel_fast WHERE id > 300$$);
SELECT parallel_same($$SELECT sum(u), avg(u), min(u) FROM parallel_fast WHERE u <> 'NaN'$$);
-- Participants without rows, and none at all.
SELECT parallel_same($$SELECT sum(n), avg(u), sum(b), avg(i4), sum(s), avg(f8), max(f4), min(n) FROM parallel_fast WHERE id > 19995$$);
SELECT parallel_same($$SELECT sum(n), avg(u), sum(b), avg(i4), sum(s), avg(f8), max(f4), min(n) FROM parallel_fast WHERE id < 0$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT sum(n), avg(u), sum(b), avg(i4), avg(f8) FROM parallel_fast$$);
RESET parallel_leader_participation;
-- With the core's Gather its Finalize Aggregate reads the core's own
-- transition values; numeric and bigint sums then go through the core's
-- functions, serialized.
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT sum(n), avg(i4), avg(f8), max(f4) FROM parallel_fast;
SELECT parallel_same($$SELECT sum(n), avg(u), sum(b), avg(i4), avg(s), sum(s), avg(f8), max(f4), min(u) FROM parallel_fast$$);
RESET tessera.batch_gather;
DROP TABLE parallel_fast;
-- A parameter of a generic plan in an argument.
PREPARE shifted(int) AS SELECT sum(a + $1) FROM parallel_t WHERE a > 4990;
SET plan_cache_mode = force_generic_plan;
EXECUTE shifted(1);
EXECUTE shifted(1000);
RESET plan_cache_mode;
DEALLOCATE shifted;
-- GROUP BY under a Gather: a table of groups in each participant, and the
-- node's grouping over TessGather merging their partial values, counts
-- and sums added as int8, extremes compared, and applying HAVING, where
-- the core's Finalize HashAggregate would merge them row by row. A group
-- of NULL values only (sum, min and max NULL, count(a) 0), int8 extremes.
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b;
SELECT parallel_same($$SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b$$);
SELECT parallel_same($$SELECT a % 7, count(*), count(a) FROM parallel_t GROUP BY a % 7 HAVING count(a) > 100$$);
EXPLAIN (COSTS OFF) SELECT b, count(*), count(a) FROM parallel_t GROUP BY b HAVING count(a) > 428;
SELECT parallel_same($$SELECT b, count(*), count(a) FROM parallel_t GROUP BY b HAVING count(a) > 428$$);
SELECT parallel_same($$SELECT a % 7, count(*), count(a), sum(a), min(a), max(a::bigint * 1000000000000), min(-a::bigint) FROM parallel_t GROUP BY a % 7$$);
-- With the core's Gather (tessera.batch_gather off) its Finalize
-- HashAggregate merges them.
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b;
SELECT parallel_same($$SELECT b, count(*), sum(a), min(a), max(a) FROM parallel_t WHERE a > 100 GROUP BY b$$);
RESET tessera.batch_gather;
SELECT plan_property($$SELECT b, sum(a) FROM parallel_t GROUP BY b$$, 'TessAgg', 'Groups') AS groups;
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT b, sum(a) FROM parallel_t GROUP BY b$$);
RESET parallel_leader_participation;
-- Over the node's partial aggregate, always the node's final one (plan
-- 4.23, item 4b), even where the core's Gather would cost less: without
-- GROUP BY the node's stack still, with it the core's own parallel plan,
-- never the node's partial aggregate under the core's Finalize, which is
-- built only without TessGather.
SET parallel_tuple_cost = 0.1;
SET tessera.gather_tuple_share = 10;
EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM parallel_t WHERE a > 100;
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a) FROM parallel_t GROUP BY b;
RESET tessera.gather_tuple_share;
SET parallel_tuple_cost = 0;
-- Sum states under a Gather (plan 4.23, item 4b): sum and avg of numeric
-- and bigint, avg of integer and smallint fold into each participant's
-- records and go up as partial values of the node's own format (bytea: a
-- tag, the state's words, then its numeric rest; avg of integers the
-- core's int8[] of the count and the sum), which the node's grouping over
-- TessGather merges: the kernels add the sums at the larger scale and keep
-- NaN and the infinities; a state with a rest, or one past the bound of
-- 10^36 at the group's scale, merges by the core's means. Groups: 0 plain,
-- 1 with NaN, 2 with +Infinity and values of more than 18 digits (rests,
-- the flag merged with them), 3 with both infinities (NaN), 4 with such
-- values (rests), 5 whose sum at scale 0 meets a value at scale 18 (past
-- the bound), 6 of NULL values only; half the bigint values have 19
-- digits (rests).
CREATE TABLE parallel_sums AS
SELECT i AS id, i % 7 AS g,
       (CASE i % 7
            WHEN 1 THEN CASE WHEN i % 997 = 1 THEN 'NaN' ELSE (i / 4.0)::numeric(12, 2) END
            WHEN 2 THEN CASE WHEN i % 991 = 2 THEN 'Infinity'
                             WHEN i % 13 = 2 THEN i * 12345678901234567890.5
                             ELSE (i / 4.0)::numeric(12, 2) END
            WHEN 3 THEN CASE WHEN i % 983 = 3 THEN 'Infinity'
                             WHEN i % 977 = 3 THEN '-Infinity' ELSE (i / 4.0)::numeric(12, 2) END
            WHEN 4 THEN CASE WHEN i % 11 = 4 THEN i * 12345678901234567890.5
                             ELSE (i / 4.0)::numeric(12, 2) END
            WHEN 5 THEN CASE WHEN i = 19997 THEN 0.000000000000000001 ELSE 999999999999999999 END
            WHEN 6 THEN NULL
            ELSE (((i * 7919) % 2000000 - 1000000) / 100.0)::numeric(12, 2)
        END)::numeric AS n,
       CASE WHEN i % 7 = 6 THEN NULL WHEN i % 2 = 0 THEN i * 1000
            ELSE 9223372036854775807 - i * 1000 END AS b,
       CASE WHEN i % 7 = 6 THEN NULL ELSE i * 7919 % 100000 - 50000 END AS i4,
       CASE WHEN i % 7 = 6 THEN NULL ELSE (i % 60000 - 30000)::int2 END AS s
FROM generate_series(1, 20000) AS i;
ANALYZE parallel_sums;
EXPLAIN (COSTS OFF)
SELECT g, sum(n), avg(n), sum(b), avg(b), avg(i4), avg(s), count(*) FROM parallel_sums GROUP BY g;
SELECT parallel_same($$SELECT g, sum(n), avg(n), sum(b), avg(b), avg(i4), avg(s), count(*) FROM parallel_sums GROUP BY g$$);
SELECT parallel_same($$SELECT g, avg(i4), sum(n) FROM parallel_sums GROUP BY g HAVING avg(n) > 0$$);
SELECT parallel_same($$SELECT g, sum(n) FILTER (WHERE i4 > 0), avg(b) FILTER (WHERE s < 0), avg(i4) FILTER (WHERE id % 3 = 0) FROM parallel_sums GROUP BY g$$);
SELECT parallel_same($$SELECT id % 1000, sum(n), avg(i4) FROM parallel_sums WHERE g IN (0, 4) GROUP BY 1$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT g, sum(n), avg(n), sum(b), avg(b), avg(i4), avg(s), count(*) FROM parallel_sums GROUP BY g$$);
RESET parallel_leader_participation;
-- Rescanned in a join: each participant folds its share anew.
SET enable_material = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, t FROM (SELECT sum(q) AS t FROM (SELECT g, sum(n) AS q FROM parallel_sums WHERE g IN (0, 4, 5) GROUP BY g) AS q) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT parallel_same($$SELECT x, t FROM (SELECT sum(q) AS t FROM (SELECT g, sum(n) AS q FROM parallel_sums WHERE g IN (0, 4, 5) GROUP BY g) AS q) AS ss
RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
RESET enable_material;
-- Without TessGather no Finalize reads the node's format: no stack of the node's.
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF)
SELECT g, sum(n), avg(i4) FROM parallel_sums GROUP BY g;
RESET tessera.batch_gather;
DROP TABLE parallel_sums;
-- GROUP BY without aggregates and DISTINCT under a Gather: the node's
-- partial grouping in every participant, TessGather, and the node's own
-- grouping of their groups above, where the core would merge them row by
-- row. 200000 rows at the default cost of a gathered row, where the plan
-- is worth the workers, keys of few values the planner knows; NULL a group
-- of its own, two keys, an int8 and an expression, a target above the keys,
-- a count above; the workers alone.
CREATE TABLE parallel_keys AS
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i END AS a, i % 10 AS b,
       CASE WHEN i % 11 = 0 THEN NULL ELSE i % 13 END AS c
FROM generate_series(1, 200000) AS i;
ANALYZE parallel_keys;
SET parallel_tuple_cost = 0.1;
EXPLAIN (COSTS OFF) SELECT b FROM parallel_keys WHERE a > 100 GROUP BY b;
SELECT parallel_same($$SELECT b FROM parallel_keys WHERE a > 100 GROUP BY b$$);
EXPLAIN (COSTS OFF) SELECT DISTINCT c % 5, b FROM parallel_keys;
SELECT parallel_same($$SELECT DISTINCT c % 5, b FROM parallel_keys$$);
SELECT parallel_same($$SELECT c % 7 + 1, b::bigint * 10000000000 FROM parallel_keys GROUP BY c % 7, b::bigint * 10000000000$$);
SELECT parallel_same($$SELECT count(*), sum(x) FROM (SELECT DISTINCT c * 1000 + b AS x FROM parallel_keys) AS s$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT DISTINCT c % 5, b FROM parallel_keys$$);
SELECT parallel_same($$SELECT b FROM parallel_keys WHERE a > 100 GROUP BY b$$);
RESET parallel_leader_participation;
-- Rescanned in a join: each participant groups its share anew.
SET enable_material = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM (SELECT DISTINCT c % 5 FROM parallel_keys) AS d) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT parallel_same($$SELECT x, n FROM (SELECT count(*) AS n FROM (SELECT DISTINCT c % 5 FROM parallel_keys) AS d) AS ss
RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
RESET enable_material;
SET parallel_tuple_cost = 0;
DROP TABLE parallel_keys;
-- Partial groups past hash_mem go out early, and the table starts anew:
-- the grouping above merges a group's partials, and the partial writes
-- nothing (the grouping above, at that hash_mem, spills as a serial one).
-- A group's rows come together, so a table folds many before it fills.
CREATE TABLE parallel_groups AS
SELECT g / 60 AS k, g AS v FROM generate_series(1, 300000) AS g;
ANALYZE parallel_groups;
SET work_mem = '64kB';
-- The core's sorted grouping would leave no partial hash aggregate to take.
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_groups GROUP BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_groups GROUP BY k) AS q$$);
SELECT partial_property($$SELECT k, count(*) FROM parallel_groups GROUP BY k$$, 'Early Emits')::int > 0 AS early,
       partial_property($$SELECT k, count(*) FROM parallel_groups GROUP BY k$$, 'Disk Usage') AS disk;
-- Sum states go out early the same, their rests with them.
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, sum(v::numeric / 3), avg(v::bigint * 10000000000000), avg(v) FROM parallel_groups GROUP BY k) AS q$$);
SELECT partial_property($$SELECT k, sum(v::numeric / 3) FROM parallel_groups GROUP BY k$$, 'Early Emits')::int > 0 AS early;
-- A state's rest counts in the memory that sends the groups up: a hundred
-- groups of sums of 2000 digits outgrow hash_mem in their rests alone.
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k % 100, sum(v * (10::numeric ^ 2000 - 1)) FROM parallel_groups GROUP BY 1) AS q$$);
SELECT partial_property($$SELECT k % 100, sum(v * (10::numeric ^ 2000 - 1)) FROM parallel_groups GROUP BY 1$$, 'Early Emits')::int > 0 AS early;
-- Groups spread over the input fold nothing before the table fills, a
-- group per row read: sending the table up would hand the grouping above
-- every row. The groups go to disk instead, as a serial node's, and out as
-- partials once the input is done.
CREATE TABLE parallel_spread AS
SELECT g % 5000 AS k, g AS v FROM generate_series(1, 300000) AS g;
ANALYZE parallel_spread;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM parallel_spread GROUP BY k) AS q$$);
SELECT partial_property($$SELECT k, count(*) FROM parallel_spread GROUP BY k$$, 'Early Emits') IS NULL AS no_early,
       partial_property($$SELECT k, count(*) FROM parallel_spread GROUP BY k$$, 'Disk Usage')::int > 0 AS spilled;
-- Sum states spill no state (their records merge a word an aggregate):
-- such groups go up early instead, every time the table fills.
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, sum(v::numeric), avg(v) FROM parallel_spread GROUP BY k) AS q$$);
SELECT partial_property($$SELECT k, sum(v::numeric) FROM parallel_spread GROUP BY k$$, 'Early Emits')::int > 0 AS early,
       partial_property($$SELECT k, sum(v::numeric) FROM parallel_spread GROUP BY k$$, 'Disk Usage') IS NULL AS no_disk;
-- Without aggregates the same: the groups of the tables emptied early, and
-- those spilled, come to the node's grouping above more than once each;
-- the workers alone too.
EXPLAIN (COSTS OFF) SELECT k FROM parallel_groups GROUP BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k FROM parallel_groups GROUP BY k) AS q$$);
SELECT partial_property($$SELECT k FROM parallel_groups GROUP BY k$$, 'Early Emits')::int > 0 AS early;
SET parallel_tuple_cost = 0.1;
EXPLAIN (COSTS OFF) SELECT DISTINCT k FROM parallel_spread;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT DISTINCT k FROM parallel_spread) AS q$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k FROM parallel_groups GROUP BY k) AS q$$);
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT DISTINCT k FROM parallel_spread) AS q$$);
RESET parallel_leader_participation;
SET parallel_tuple_cost = 0;
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
-- A hundred columns: two lanes of NULL bits in a message, a column from
-- the 65th on taking its bit in the second.
DO $$
BEGIN
    EXECUTE format('CREATE TABLE parallel_many AS SELECT g AS k, %s, '
                   'CASE WHEN g %% 5 = 0 THEN NULL ELSE ''x'' || g END AS t '
                   'FROM generate_series(1, 20000) AS g',
                   (SELECT string_agg(format('CASE WHEN g %% %s = 0 THEN NULL ELSE g + %s END AS c%s',
                                             i % 7 + 2, i, i), ', ')
                    FROM generate_series(1, 98) AS i));
END $$;
ANALYZE parallel_many;
EXPLAIN (COSTS OFF) SELECT * FROM parallel_many WHERE k % 3 <> 0;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT * FROM parallel_many WHERE k % 3 <> 0 OFFSET 0) AS q$$);
SET parallel_leader_participation = off;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT t, c98, c64, * FROM parallel_many WHERE k % 3 <> 0 OFFSET 0) AS q$$);
RESET parallel_leader_participation;
DROP TABLE parallel_many;
-- A projection the workers may compute goes below the gather, and the
-- batch node there takes it; one of a parallel-restricted function stays
-- in the leader, above TessGather.
CREATE FUNCTION parallel_restricted(int) RETURNS int
LANGUAGE plpgsql PARALLEL RESTRICTED AS $$ BEGIN RETURN $1 * 2; END $$;
EXPLAIN (COSTS OFF, VERBOSE) SELECT k + 1, a FROM parallel_wide WHERE k % 3 <> 0;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k + 1, a FROM parallel_wide WHERE k % 3 <> 0 OFFSET 0) AS q$$);
EXPLAIN (COSTS OFF) SELECT parallel_restricted(k), a FROM parallel_wide WHERE k % 3 <> 0;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT parallel_restricted(k), a FROM parallel_wide WHERE k % 3 <> 0 OFFSET 0) AS q$$);
DROP FUNCTION parallel_restricted(int);
-- Executor parameters: finalize_plan wants one of the core's Gathers over
-- every parallel-aware node, and the nodes under TessGather hold their
-- flag back while it runs. An initplan, a subplan run for every outer
-- row, INTERSECT and a recursive CTE.
EXPLAIN (COSTS OFF) SELECT k, a FROM parallel_wide WHERE k < (SELECT 1000) ORDER BY k;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, a FROM parallel_wide WHERE k < (SELECT 1000) OFFSET 0) AS q$$);
SELECT parallel_same($$SELECT x, (SELECT count(*) FROM parallel_wide WHERE k < x * 1000 AND a >= 0) FROM generate_series(1, 3) AS x$$);
EXPLAIN (VERBOSE, COSTS OFF) SELECT k FROM parallel_wide WHERE k < 100 INTERSECT SELECT a FROM parallel_wide WHERE a < 500;
SELECT parallel_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k FROM parallel_wide WHERE k < 100 INTERSECT SELECT a FROM parallel_wide WHERE a < 500) AS q$$);
SELECT parallel_same($$WITH RECURSIVE r(n) AS (SELECT k FROM parallel_wide WHERE k < 10 UNION SELECT n + 1 FROM r WHERE n < 20) SELECT count(*) FROM r$$);
-- At the core's costs of parallel work the node's own paths cost a row a
-- quarter of parallel_tuple_cost: a sort of every row goes parallel
-- through TessGatherMerge, and stays serial with tessera.batch_gather off,
-- where a row costs the core's Gather Merge the whole; a scan returning
-- 8000 rows goes parallel through TessGather, and through the core's
-- Gather too while the node's model of a partial scan starts the workers
-- at no cost. At the model's default start the scan of so small a table
-- stays serial.
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
EXPLAIN (COSTS OFF) SELECT k, a FROM parallel_wide ORDER BY a, k;
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
EXPLAIN (COSTS OFF) SELECT k, a FROM parallel_wide ORDER BY a, k;
RESET tessera.batch_gather;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
-- A worker's toll a page it reads first counts against the workers too:
-- at a lower start the scan goes parallel without the toll, and stays
-- serial at a toll of 10 a page.
SET tessera.scan_parallel_setup_cost = 1500;
SET tessera.scan_worker_page_cost = 0;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
SET tessera.scan_worker_page_cost = 10;
EXPLAIN (COSTS OFF) SELECT k, a, t FROM parallel_wide WHERE k < 8000;
-- The filter's work counts in the scan's time: at the model's defaults a
-- clause by rows past the first makes the workers worth their start,
-- where a batch clause does not; and at a lower start so does a first
-- clause on a column past a varlena (t precedes c16).
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
EXPLAIN (COSTS OFF) SELECT count(*) FROM parallel_wide WHERE k > 0 AND c1 > 0;
EXPLAIN (COSTS OFF) SELECT count(*) FROM parallel_wide WHERE k > 0 AND (c1 # 0) > 0;
SET tessera.scan_parallel_setup_cost = 2500;
EXPLAIN (COSTS OFF) SELECT count(*) FROM parallel_wide WHERE k > 0;
EXPLAIN (COSTS OFF) SELECT count(*) FROM parallel_wide WHERE c16 > 0;
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
DROP TABLE parallel_wide;
-- A parallel-aware node of the core below that allocates in the query's
-- dynamic shared memory, the shared bitmap of a Parallel Bitmap Heap
-- Scan: the leader's own part runs with it installed, as under the core's
-- Gather and Gather Merge (it found none and crashed).
CREATE TABLE parallel_bitmap AS
SELECT g AS id, (g::bigint * 7919 % 200000)::int AS k, g % 97 AS w FROM generate_series(1, 200000) AS g;
CREATE INDEX parallel_bitmap_k ON parallel_bitmap (k);
VACUUM ANALYZE parallel_bitmap;
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(w) FROM parallel_bitmap WHERE k < 60000;
SELECT parallel_same($$SELECT count(*), sum(w) FROM parallel_bitmap WHERE k < 60000$$);
EXPLAIN (COSTS OFF) SELECT k, w FROM parallel_bitmap WHERE k < 60000 ORDER BY k;
SELECT parallel_same($$SELECT md5(string_agg(k || ':' || w, ',' ORDER BY n)) FROM (SELECT k, w, row_number() OVER () AS n FROM (SELECT k, w FROM parallel_bitmap WHERE k < 60000 ORDER BY k, w) AS s) AS q$$);
RESET enable_seqscan;
RESET enable_indexscan;
RESET enable_indexonlyscan;
DROP TABLE parallel_bitmap;
-- A gather run again, in a subquery that runs once for each x: the
-- leader's own part rescans with the workers, which share the scans anew,
-- as under the core's Gather and its rescan parameter, and an initplan's
-- value that changed with x reaches it. Before, a shared table built for
-- the first x stayed (0 rows past it), and a sort kept the leader's share
-- of the first scan (the sum of its rows twice).
CREATE TABLE parallel_rescan AS SELECT g AS k, g % 1000 AS v FROM generate_series(1, 200000) AS g;
CREATE TABLE parallel_rescan_keys AS SELECT g AS k, g % 7 AS w FROM generate_series(1, 100000) AS g;
ANALYZE parallel_rescan;
ANALYZE parallel_rescan_keys;
EXPLAIN (COSTS OFF)
SELECT x, (SELECT count(*) FROM parallel_rescan AS b JOIN parallel_rescan_keys AS m ON b.k = m.k WHERE m.w = (SELECT x)) FROM generate_series(1, 4) AS x;
SELECT parallel_same($$SELECT x, (SELECT count(*) FROM parallel_rescan AS b JOIN parallel_rescan_keys AS m ON b.k = m.k WHERE m.w = (SELECT x)) FROM generate_series(1, 4) AS x$$);
EXPLAIN (COSTS OFF)
SELECT x, (SELECT sum(k) FROM (SELECT k FROM parallel_rescan WHERE v < (SELECT x * 100) ORDER BY k DESC OFFSET 10) AS q) FROM generate_series(1, 4) AS x;
SELECT parallel_same($$SELECT x, (SELECT sum(k) FROM (SELECT k FROM parallel_rescan WHERE v < (SELECT x * 100) ORDER BY k DESC OFFSET 10) AS q) FROM generate_series(1, 4) AS x$$);
EXPLAIN (COSTS OFF)
SELECT x, (SELECT sum(k) + x FROM (SELECT k FROM parallel_rescan ORDER BY k DESC OFFSET 10) AS q) FROM generate_series(1, 6) AS x;
SELECT parallel_same($$SELECT x, (SELECT sum(k) + x FROM (SELECT k FROM parallel_rescan ORDER BY k DESC OFFSET 10) AS q) FROM generate_series(1, 6) AS x$$);
DROP TABLE parallel_rescan, parallel_rescan_keys;
-- A join's TessGather reads a copy of the partial path it gathers. The
-- join hook runs once a pair of inputs; here a later pair, dim over fact
-- for the right anti join, adds the core's partial path, which drops and
-- frees the node's. Before, the gather still read the freed path, a
-- projection over the gather took its memory, and planning went round
-- the loop until the stack ran out.
CREATE TABLE parallel_fact AS
    SELECT g AS id, (g % 101 - 50)::int4 AS a, (g * 7919 % 200001 - 100000)::int4 AS b,
           (g % 101 - 50)::int8 AS c, (g * 37 % 2001 - 1000)::int2 AS s,
           date '2000-01-01' + (g * 13 % 4001 - 2000) AS d
    FROM generate_series(1, 2000) AS g;
CREATE TABLE parallel_dim AS
    SELECT g AS id, CASE WHEN g % 10 = 0 THEN NULL ELSE (g % 101 - 50)::int4 END AS a
    FROM generate_series(1, 2000) AS g;
ANALYZE parallel_fact;
ANALYZE parallel_dim;
EXPLAIN (COSTS OFF)
SELECT b FROM parallel_fact EXCEPT ALL SELECT f.b - 1 FROM parallel_fact AS f WHERE f.d IN (date '2000-01-01' - 849, date '2000-01-01' + 145, date '2000-01-01' - 1283) AND NOT EXISTS (SELECT 1 FROM parallel_dim AS m WHERE m.a = f.s);
SELECT parallel_same($$SELECT count(*), sum(b) FROM (SELECT b FROM parallel_fact EXCEPT ALL SELECT f.b - 1 FROM parallel_fact AS f WHERE f.d IN (date '2000-01-01' - 849, date '2000-01-01' + 145, date '2000-01-01' - 1283) AND NOT EXISTS (SELECT 1 FROM parallel_dim AS m WHERE m.a = f.s)) AS q$$);
DROP TABLE parallel_fact, parallel_dim;
-- The switch off.
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a, b FROM parallel_t WHERE a > 4990;
RESET tessera.enable;

DROP TABLE parallel_t;
DROP FUNCTION parallel_same(text);
DROP FUNCTION plan_property(text, text, text);
DROP FUNCTION partial_property(text, text);
DROP FUNCTION parallel_explain(text);
DROP EXTENSION tessera;
