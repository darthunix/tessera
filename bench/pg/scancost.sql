-- The scancost family: the times of the node's scans, from which the
-- planner's model of them is fitted (tessera.scan_page_cost and the rest,
-- docs/nodes.md): a full scan with TessFilter above over tables of other
-- widths; an index-only scan, the index mode and a bitmap at shares of
-- bench_idx's rows, the bitmap of the scattered k and of the ordered id;
-- and the core's scans, as time per unit of their cost. Each sample is
-- the minimum of its runs. The summary gives the fitted times and the
-- parameters' values, a page of the full scan being 1.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 7
\endif
SET jit = off;
SET max_parallel_workers_per_gather = 0;
SET work_mem = '256MB';
-- The DO block below reads them as settings: psql substitutes no
-- variable within its dollar quotes.
SELECT set_config('bench.rows', (2000000 * scale)::text, false) FROM bench_scale;
SELECT set_config('bench.repetitions', :'repetitions', false);

-- One sample a query: its plan's scan node, its cost, pages and rows.
CREATE TEMP TABLE samples
(
    method text,
    relation text,
    share numeric,
    pages float8,
    tuples float8,
    rows float8,
    cost float8,
    milliseconds float8
);

/* The minimum time of repetitions runs, after a warm-up. */
CREATE FUNCTION pg_temp.fastest(query text, repetitions integer)
RETURNS float8
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
    best float8;
BEGIN
    EXECUTE query;
    EXECUTE query;
    FOR sample IN 1..repetitions LOOP
        started_at := clock_timestamp();
        EXECUTE query;
        best := least(best, 1000 * extract(epoch FROM clock_timestamp() - started_at));
    END LOOP;
    RETURN best;
END
$function$;

/* Only the scans of a kind: seq, index-only, index, bitmap, as enable_* allow them. */
CREATE FUNCTION pg_temp.only(kind text)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM set_config('enable_seqscan', (kind = 'seq')::text, false);
    PERFORM set_config('enable_indexonlyscan', (kind = 'ios')::text, false);
    PERFORM set_config('enable_indexscan', (kind IN ('ios', 'index'))::text, false);
    PERFORM set_config('enable_bitmapscan', (kind = 'bitmap')::text, false);
END
$function$;

/*
 * A sample: the query timed, and from its EXPLAIN ANALYZE the scan's cost
 * and rows (the plan's first node with a relation: the core's scan, or
 * the node's filter over it), and the heap pages a bitmap read (the
 * node's or the core's Heap Blocks).
 */
CREATE FUNCTION pg_temp.sample(method text, relation text, share numeric, query text,
                               repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    plan json;
    node json;
    scan json;
    pages float8;
BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query INTO plan;
    node := plan -> 0 -> 'Plan';
    WHILE node IS NOT NULL LOOP
        IF scan IS NULL AND node ->> 'Relation Name' IS NOT NULL THEN
            scan := node;
        END IF;
        IF node ->> 'Exact Heap Blocks' IS NOT NULL THEN
            pages := (node ->> 'Exact Heap Blocks')::float8 + (node ->> 'Lossy Heap Blocks')::float8;
        END IF;
        node := node -> 'Plans' -> 0;
    END LOOP;
    INSERT INTO samples
    SELECT method, relation, share,
           CASE WHEN method LIKE '%bitmap%' THEN pages ELSE c.relpages END,
           c.reltuples, (scan ->> 'Actual Rows')::float8, (scan ->> 'Total Cost')::float8,
           pg_temp.fastest(query, repetitions)
    FROM pg_class AS c
    WHERE c.relname = relation;
END
$function$;

DO $do$
DECLARE
    rows bigint := current_setting('bench.rows')::bigint;
    repetitions integer := current_setting('bench.repetitions')::integer;
    share numeric;
    mode text;
    relation text;
BEGIN
    FOREACH mode IN ARRAY ARRAY['on', 'off'] LOOP
        PERFORM set_config('tessera.enable', mode, false);
        -- The full scan with a clause every row passes, over every width.
        PERFORM pg_temp.only('seq');
        FOREACH relation IN ARRAY ARRAY['bench_narrow', 'bench_fact', 'bench_sort', 'bench_idx',
                                        'bench_mixed', 'bench_wide'] LOOP
            PERFORM pg_temp.sample(mode || ' seq', relation, 1,
                                   format('SELECT count(*) FROM %I WHERE %I > -1', relation,
                                          (SELECT attname FROM pg_attribute
                                           WHERE attrelid = relation::regclass AND attnum = 1)),
                                   repetitions);
        END LOOP;
        FOREACH share IN ARRAY ARRAY[0.01, 0.05, 0.1, 0.2, 0.3, 0.5] LOOP
            PERFORM pg_temp.only('ios');
            PERFORM pg_temp.sample(mode || ' ios', 'bench_idx', share,
                                   format('SELECT count(*) FROM bench_idx WHERE k < %s', round(rows * share)),
                                   repetitions);
            PERFORM pg_temp.only('index');
            PERFORM pg_temp.sample(mode || ' index', 'bench_idx', share,
                                   format('SELECT count(*), sum(w) FROM bench_idx WHERE id < %s', round(rows * share)),
                                   repetitions);
            PERFORM pg_temp.only('bitmap');
            PERFORM pg_temp.sample(mode || ' bitmap scattered', 'bench_idx', share,
                                   format('SELECT count(*), sum(w) FROM bench_idx WHERE k < %s', round(rows * share)),
                                   repetitions);
            PERFORM pg_temp.sample(mode || ' bitmap ordered', 'bench_idx', share,
                                   format('SELECT count(*), sum(w) FROM bench_idx WHERE id < %s', round(rows * share)),
                                   repetitions);
        END LOOP;
    END LOOP;
    PERFORM set_config('tessera.enable', 'on', false);
END
$do$;
RESET enable_seqscan;
RESET enable_indexonlyscan;
RESET enable_indexscan;
RESET enable_bitmapscan;

\copy samples TO 'timings.csv' CSV HEADER

\o summary.txt
-- The samples, then the fits: the full scan by pages and tuples, the
-- bitmaps by pages, rows and rows times 1 - c² for the column's
-- correlation c (least squares without an intercept; the ordered id alone
-- has as many pages as rows by a constant, the scattered k tells them
-- apart), the index scans by rows; the core's by their cost. A scattered
-- column's bitmap is built from rows in no order of their pages, which
-- its rows pay for.
SELECT method, relation, share, pages, tuples, rows, round(cost::numeric, 1) AS cost,
       round(milliseconds::numeric, 3) AS ms
FROM samples ORDER BY method, relation, share;

CREATE TEMP VIEW fits AS
WITH seq AS (
    SELECT sum(pages * pages) AS pp, sum(pages * tuples) AS pt, sum(tuples * tuples) AS tt,
           sum(pages * milliseconds) AS pm, sum(tuples * milliseconds) AS tm
    FROM samples WHERE method = 'on seq'
), bitmap_samples AS (
    SELECT s.pages AS x1, s.rows AS x2,
           s.rows * (1 - coalesce(st.correlation, 0) ^ 2) AS x3, s.milliseconds AS y
    FROM samples AS s
    LEFT JOIN pg_stats AS st
        ON st.tablename = 'bench_idx'
       AND st.attname = CASE WHEN s.method LIKE '%ordered' THEN 'id' ELSE 'k' END
    WHERE s.method LIKE 'on bitmap%'
), bitmap AS (
    SELECT sum(x1 * x1) AS a11, sum(x1 * x2) AS a12, sum(x1 * x3) AS a13,
           sum(x2 * x2) AS a22, sum(x2 * x3) AS a23, sum(x3 * x3) AS a33,
           sum(x1 * y) AS b1, sum(x2 * y) AS b2, sum(x3 * y) AS b3
    FROM bitmap_samples
), bitmap_fit AS (
    -- Cramer's rule over the normal equations.
    SELECT (b1 * (a22 * a33 - a23 * a23) - a12 * (b2 * a33 - a23 * b3) + a13 * (b2 * a23 - a22 * b3)) / d AS page_ms,
           (a11 * (b2 * a33 - a23 * b3) - b1 * (a12 * a33 - a23 * a13) + a13 * (a12 * b3 - b2 * a13)) / d AS row_ms,
           (a11 * (a22 * b3 - b2 * a23) - a12 * (a12 * b3 - b2 * a13) + b1 * (a12 * a23 - a22 * a13)) / d AS extra_ms
    FROM bitmap,
         LATERAL (SELECT a11 * (a22 * a33 - a23 * a23) - a12 * (a12 * a33 - a23 * a13) +
                         a13 * (a12 * a23 - a22 * a13) AS d) AS det
), scattered AS (
    SELECT extra_ms FROM bitmap_fit
)
SELECT
    (SELECT (pm * tt - tm * pt) / (pp * tt - pt * pt) FROM seq) AS scan_page_ms,
    (SELECT (tm * pp - pm * pt) / (pp * tt - pt * pt) FROM seq) AS scan_tuple_ms,
    (SELECT regr_slope(milliseconds, rows) FROM samples WHERE method = 'on ios') AS ios_row_ms,
    (SELECT regr_slope(milliseconds, rows) FROM samples WHERE method = 'on index') AS index_row_ms,
    (SELECT page_ms FROM bitmap_fit) AS bitmap_page_ms,
    (SELECT row_ms FROM bitmap_fit) AS bitmap_row_ms,
    (SELECT extra_ms FROM scattered) AS bitmap_scatter_ms,
    (SELECT regr_slope(milliseconds, cost) FROM samples WHERE method LIKE 'off %') AS core_cost_ms;

-- The times in microseconds, and the parameters: a full scan's page is 1.
SELECT round((scan_page_ms * 1000)::numeric, 4) AS scan_page_us,
       round((scan_tuple_ms * 1000)::numeric, 5) AS scan_tuple_us,
       round((ios_row_ms * 1000)::numeric, 5) AS ios_row_us,
       round((index_row_ms * 1000)::numeric, 5) AS index_row_us,
       round((bitmap_page_ms * 1000)::numeric, 4) AS bitmap_page_us,
       round((bitmap_row_ms * 1000)::numeric, 5) AS bitmap_row_us,
       round((bitmap_scatter_ms * 1000)::numeric, 5) AS bitmap_scatter_us,
       round((core_cost_ms * 1000)::numeric, 4) AS core_cost_us
FROM fits;
-- The core's time per unit of its cost, by scan: one value serves them all.
SELECT regexp_replace(method, ' (ordered|scattered)$', '') AS core_scan,
       round((sum(milliseconds) / sum(cost) * 1000)::numeric, 4) AS us_per_cost_unit
FROM samples WHERE method LIKE 'off %' GROUP BY 1 ORDER BY 1;
SELECT 1.0 AS "tessera.scan_page_cost",
       round((scan_tuple_ms / scan_page_ms)::numeric, 4) AS "tessera.scan_tuple_cost",
       round((ios_row_ms / scan_page_ms)::numeric, 4) AS "tessera.index_only_tuple_cost",
       round((index_row_ms / scan_page_ms)::numeric, 4) AS "tessera.index_tuple_cost",
       round((bitmap_page_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_page_cost",
       round((bitmap_row_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_tuple_cost",
       round((bitmap_scatter_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_scatter_cost",
       round((core_cost_ms / scan_page_ms)::numeric, 4) AS "tessera.core_scan_cost"
FROM fits;
-- How well each sample of the node fits, predicted against measured.
SELECT s.method, s.relation, s.share, round(s.milliseconds::numeric, 3) AS ms,
       round((CASE
           WHEN s.method = 'on seq' THEN f.scan_page_ms * s.pages + f.scan_tuple_ms * s.tuples
           WHEN s.method = 'on ios' THEN f.ios_row_ms * s.rows
           WHEN s.method = 'on index' THEN f.index_row_ms * s.rows
           WHEN s.method LIKE 'on bitmap%' THEN f.bitmap_page_ms * s.pages + f.bitmap_row_ms * s.rows +
               f.bitmap_scatter_ms * s.rows *
               (1 - coalesce((SELECT correlation FROM pg_stats WHERE tablename = 'bench_idx' AND
                              attname = CASE WHEN s.method LIKE '%ordered' THEN 'id' ELSE 'k' END), 0) ^ 2)
       END)::numeric, 3) AS predicted_ms
FROM samples AS s, fits AS f
WHERE s.method LIKE 'on %'
ORDER BY s.method, s.relation, s.share;
\o
