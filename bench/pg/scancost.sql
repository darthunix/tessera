-- The scancost family: the times of the node's scans, from which the
-- planner's model of them is fitted (tessera.scan_page_cost and the rest,
-- docs/nodes.md): a full scan with TessFilter above over tables of other
-- widths; an index-only scan, the index mode and a bitmap at shares of
-- bench_idx's rows, the bitmap of the scattered k and of the ordered id;
-- the core's scans, as time per unit of their cost; full scans with
-- clauses past the first, for the model of the filter; and the full scans
-- again with two parallel workers, for the model of a partial scan. Each
-- sample is the minimum of its runs. The summary gives the fitted times
-- and the parameters' values, a page of the full scan being 1.
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
-- The model's parallel parameters in force, which the parallel samples set
-- to zero for a while to run in parallel at all.
SELECT set_config('bench.parallel_setup_cost', current_setting('tessera.scan_parallel_setup_cost'), false),
       set_config('bench.worker_page_cost', current_setting('tessera.scan_worker_page_cost'), false);

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

/*
 * The parallel samples: the full scans with two workers, without the
 * leader and with it, and bench_tiny's count, next to nothing to read, for
 * the workers' start and finish alone.
 */
CREATE TEMP TABLE parallel_samples
(
    method text,
    relation text,
    leader boolean,
    workers integer,
    pages float8,
    milliseconds float8
);

/*
 * The filter's samples: full scans of bench_mixed and bench_narrow, with
 * the rows reaching a batch clause past the first (batch), deforming a
 * column past a varlena (varlena), reaching a clause by rows (row_rows)
 * and those rows times the clause's operators by the core's cost
 * (operators); 'base' is each table's scan with one clause on its first
 * column, which the model of the scan counts.
 */
CREATE TEMP TABLE filter_samples
(
    name text,
    relation text,
    batch float8,
    varlena float8,
    row_rows float8,
    operators float8,
    milliseconds float8
);

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

/* A parallel sample: the query timed, and the workers its plan launched. */
CREATE FUNCTION pg_temp.parallel_sample(method text, relation text, leader boolean, query text,
                                        repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    plan json;
    node json;
    launched integer;
BEGIN
    PERFORM set_config('parallel_leader_participation', leader::text, false);
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query INTO plan;
    node := plan -> 0 -> 'Plan';
    WHILE node IS NOT NULL AND launched IS NULL LOOP
        launched := (node ->> 'Workers Launched')::integer;
        node := node -> 'Plans' -> 0;
    END LOOP;
    INSERT INTO parallel_samples
    SELECT method, relation, leader, coalesce(launched, 0), c.relpages,
           pg_temp.fastest(query, repetitions)
    FROM pg_class AS c
    WHERE c.relname = relation;
    PERFORM set_config('parallel_leader_participation', 'on', false);
END
$function$;

/* A filter sample: the query timed, with its rows reaching each kind of work. */
CREATE FUNCTION pg_temp.filter_sample(name text, relation text, query text, batch float8,
                                      varlena float8, row_rows float8, operators float8,
                                      repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    INSERT INTO filter_samples
    VALUES (name, relation, batch, varlena, row_rows, operators,
            pg_temp.fastest(query, repetitions));
END
$function$;

DO $do$
DECLARE
    rows bigint := current_setting('bench.rows')::bigint;
    repetitions integer := current_setting('bench.repetitions')::integer;
    share numeric;
    mode text;
    relation text;
    leader boolean;
    mixed float8;
    narrow float8;
    half float8;
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
        -- The filter: later clauses in batches, a column past a varlena
        -- (bench_mixed's b, a text, precedes c, d and e), clauses by rows,
        -- on every row and on the half the first clause leaves.
        IF mode = 'on' THEN
            PERFORM pg_temp.only('seq');
            SELECT count(*) INTO mixed FROM bench_mixed;
            SELECT count(*) INTO half FROM bench_mixed WHERE a > (SELECT count(*) / 2 FROM bench_mixed);
            PERFORM pg_temp.filter_sample('base', 'bench_mixed',
                'SELECT count(*) FROM bench_mixed WHERE a > -1', 0, 0, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('varlena first', 'bench_mixed',
                'SELECT count(*) FROM bench_mixed WHERE d > -1', 0, mixed, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch varlena', 'bench_mixed',
                'SELECT count(*) FROM bench_mixed WHERE a > -1 AND d > -1', mixed, mixed, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch varlena half', 'bench_mixed',
                format('SELECT count(*) FROM bench_mixed WHERE a > %s AND d > -1', (SELECT count(*) / 2 FROM bench_mixed)),
                half, half, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch int8 varlena', 'bench_mixed',
                'SELECT count(*) FROM bench_mixed WHERE a > -1 AND c > -1', mixed, mixed, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('row text', 'bench_mixed',
                $q$SELECT count(*) FROM bench_mixed WHERE a > -1 AND b <> 'x'$q$, 0, 0, mixed, mixed, repetitions);
            PERFORM pg_temp.filter_sample('row text half', 'bench_mixed',
                format($q$SELECT count(*) FROM bench_mixed WHERE a > %s AND b <> 'x'$q$, (SELECT count(*) / 2 FROM bench_mixed)),
                0, 0, half, half, repetitions);
            PERFORM pg_temp.filter_sample('row text varlena', 'bench_mixed',
                $q$SELECT count(*) FROM bench_mixed WHERE a > -1 AND e <> 'x'$q$, 0, mixed, mixed, mixed, repetitions);
            SELECT count(*) INTO narrow FROM bench_narrow;
            SELECT count(*) INTO half FROM bench_narrow WHERE c1 > (SELECT count(*) / 2 FROM bench_narrow);
            PERFORM pg_temp.filter_sample('base', 'bench_narrow',
                'SELECT count(*) FROM bench_narrow WHERE c1 > -1', 0, 0, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch', 'bench_narrow',
                'SELECT count(*) FROM bench_narrow WHERE c1 > -1 AND c2 > -1', narrow, 0, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch half', 'bench_narrow',
                format('SELECT count(*) FROM bench_narrow WHERE c1 > %s AND c2 > -1', (SELECT count(*) / 2 FROM bench_narrow)),
                half, 0, 0, 0, repetitions);
            PERFORM pg_temp.filter_sample('batch two', 'bench_narrow',
                'SELECT count(*) FROM bench_narrow WHERE c1 > -1 AND c2 > -1 AND c3 > -1', 2 * narrow, 0, 0, 0,
                repetitions);
            PERFORM pg_temp.filter_sample('row numeric', 'bench_narrow',
                'SELECT count(*) FROM bench_narrow WHERE c1 > -1 AND c2::numeric > -1', 0, 0, narrow, 2 * narrow,
                repetitions);
        END IF;
        -- The full scans again with two workers, which the core's costs
        -- and the node's model are set to allow.
        PERFORM set_config('max_parallel_workers_per_gather', '2', false);
        PERFORM set_config('parallel_setup_cost', '0', false);
        PERFORM set_config('parallel_tuple_cost', '0', false);
        PERFORM set_config('min_parallel_table_scan_size', '0', false);
        PERFORM set_config('tessera.scan_parallel_setup_cost', '0', false);
        PERFORM set_config('tessera.scan_worker_page_cost', '0', false);
        PERFORM pg_temp.only('seq');
        FOREACH relation IN ARRAY ARRAY['bench_narrow', 'bench_fact', 'bench_sort', 'bench_idx',
                                        'bench_mixed', 'bench_wide'] LOOP
            FOREACH leader IN ARRAY ARRAY[false, true] LOOP
                PERFORM pg_temp.parallel_sample(mode || ' seq', relation, leader,
                                                format('SELECT count(*) FROM %I WHERE %I > -1', relation,
                                                       (SELECT attname FROM pg_attribute
                                                        WHERE attrelid = relation::regclass AND attnum = 1)),
                                                repetitions);
            END LOOP;
        END LOOP;
        PERFORM pg_temp.parallel_sample(mode || ' start', 'bench_tiny', true,
                                        'SELECT count(*) FROM bench_tiny', repetitions);
        PERFORM set_config('max_parallel_workers_per_gather', '0', false);
        PERFORM set_config('parallel_setup_cost', '1000', false);
        PERFORM set_config('parallel_tuple_cost', '0.1', false);
        PERFORM set_config('min_parallel_table_scan_size', '8MB', false);
        PERFORM set_config('tessera.scan_parallel_setup_cost',
                           current_setting('bench.parallel_setup_cost'), false);
        PERFORM set_config('tessera.scan_worker_page_cost',
                           current_setting('bench.worker_page_cost'), false);
    END LOOP;
    PERFORM set_config('tessera.enable', 'on', false);
END
$do$;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET enable_seqscan;
RESET enable_indexonlyscan;
RESET enable_indexscan;
RESET enable_bitmapscan;

\copy samples TO 'timings.csv' CSV HEADER
\copy parallel_samples TO 'parallel.csv' CSV HEADER
\copy filter_samples TO 'filter.csv' CSV HEADER

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
-- The core's time per unit of its cost, by scan, which the model does not
-- use: it varies too much over the bitmaps.
SELECT regexp_replace(method, ' (ordered|scattered)$', '') AS core_scan,
       round((sum(milliseconds) / sum(cost) * 1000)::numeric, 4) AS us_per_cost_unit
FROM samples WHERE method LIKE 'off %' GROUP BY 1 ORDER BY 1;
SELECT 1.0 AS "tessera.scan_page_cost",
       round((scan_tuple_ms / scan_page_ms)::numeric, 4) AS "tessera.scan_tuple_cost",
       round((ios_row_ms / scan_page_ms)::numeric, 4) AS "tessera.index_only_tuple_cost",
       round((index_row_ms / scan_page_ms)::numeric, 4) AS "tessera.index_tuple_cost",
       round((bitmap_page_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_page_cost",
       round((bitmap_row_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_tuple_cost",
       round((bitmap_scatter_ms / scan_page_ms)::numeric, 4) AS "tessera.bitmap_scatter_cost"
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

-- The model of a partial scan, by mode (docs/nodes.md): L, the time of
-- bench_tiny's parallel count, the workers' start and finish; phi, the
-- toll a page a worker reads, fitted over the samples without the leader,
-- whose n workers read P / n pages each at c + phi for the serial time a
-- page c = S / P (T - L - S / n = phi P / n, least squares through zero);
-- h, what the leader reads before the workers come, from the samples
-- with it: T = L + (S - h) / D, D = 1 + n c / (c + phi), averaged.
SELECT method, relation, leader, workers, pages, round(milliseconds::numeric, 3) AS ms
FROM parallel_samples ORDER BY method, relation, leader;

CREATE TEMP VIEW parallel_fit AS
WITH serial AS (
    SELECT split_part(method, ' ', 1) AS mode, relation, milliseconds AS s
    FROM samples WHERE method LIKE '% seq'
), parallel AS (
    SELECT split_part(p.method, ' ', 1) AS mode, p.*, se.s
    FROM parallel_samples AS p
    JOIN serial AS se ON se.mode = split_part(p.method, ' ', 1) AND se.relation = p.relation
    WHERE p.method LIKE '% seq' AND p.workers > 0
), start AS (
    SELECT split_part(method, ' ', 1) AS mode, milliseconds AS l
    FROM parallel_samples WHERE method LIKE '% start'
), toll AS (
    SELECT p.mode,
           sum((p.pages / p.workers) * (p.milliseconds - st.l - p.s / p.workers)) /
           sum((p.pages / p.workers) ^ 2) AS phi_ms
    FROM parallel AS p JOIN start AS st USING (mode)
    WHERE NOT p.leader
    GROUP BY p.mode
), head AS (
    SELECT p.mode,
           avg(p.s - (p.milliseconds - st.l) *
               (1 + p.workers * (p.s / p.pages) / (p.s / p.pages + t.phi_ms))) AS h_ms
    FROM parallel AS p JOIN start AS st USING (mode) JOIN toll AS t USING (mode)
    WHERE p.leader
    GROUP BY p.mode
)
SELECT st.mode, st.l AS l_ms, t.phi_ms, h.h_ms
FROM start AS st JOIN toll AS t USING (mode) JOIN head AS h USING (mode);

SELECT mode, round(l_ms::numeric, 3) AS start_ms, round((phi_ms * 1000)::numeric, 4) AS page_toll_us,
       round(h_ms::numeric, 3) AS head_start_ms
FROM parallel_fit ORDER BY mode DESC;
-- In the units of the model in force: the time of a full scan's page as
-- the current tessera.scan_page_cost and scan_tuple_cost give it, the
-- median over the serial full scans (the unit a fit of this run gives
-- moves from run to run, and with it the parameters above).
CREATE TEMP VIEW model_unit AS
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY milliseconds /
           (pages * current_setting('tessera.scan_page_cost')::float8 +
            tuples * current_setting('tessera.scan_tuple_cost')::float8)) AS unit_ms
FROM samples WHERE method = 'on seq';
SELECT round((p.l_ms / u.unit_ms)::numeric) AS "tessera.scan_parallel_setup_cost",
       round((p.phi_ms / u.unit_ms)::numeric, 2) AS "tessera.scan_worker_page_cost",
       round((p.h_ms / u.unit_ms)::numeric) AS head_start,
       round((u.unit_ms * 1000)::numeric, 4) AS unit_us
FROM parallel_fit AS p, model_unit AS u
WHERE p.mode = 'on';
-- How well each parallel sample of the node fits, predicted against measured.
SELECT p.relation, p.leader, round(p.milliseconds::numeric, 3) AS ms,
       round((f.l_ms + CASE
           WHEN p.leader THEN (se.milliseconds - f.h_ms) /
               (1 + p.workers * (se.milliseconds / p.pages) / (se.milliseconds / p.pages + f.phi_ms))
           ELSE (se.milliseconds + p.pages * f.phi_ms) / p.workers
       END)::numeric, 3) AS predicted_ms,
       round(se.milliseconds::numeric, 3) AS serial_ms
FROM parallel_samples AS p
JOIN samples AS se ON se.method = 'on seq' AND se.relation = p.relation
JOIN parallel_fit AS f ON f.mode = 'on'
WHERE p.method = 'on seq'
ORDER BY p.relation, p.leader;

-- The model of the filter: each sample's time over its table's base,
-- fitted by its rows: a column past a varlena from the samples of that
-- alone, a later batch clause from those of batch clauses alone, then a
-- clause by rows and its operators from the samples by rows, less their
-- deforming (least squares through zero).
CREATE TEMP VIEW filter_excess AS
SELECT f.*, f.milliseconds - b.milliseconds AS excess
FROM filter_samples AS f
JOIN filter_samples AS b ON b.relation = f.relation AND b.name = 'base'
WHERE f.name <> 'base';
CREATE TEMP VIEW filter_fit AS
WITH v AS (
    SELECT sum(varlena * excess) / sum(varlena ^ 2) AS varlena_ms
    FROM filter_excess WHERE batch = 0 AND row_rows = 0
), b AS (
    SELECT sum(batch * excess) / sum(batch ^ 2) AS batch_ms
    FROM filter_excess WHERE varlena = 0 AND row_rows = 0
), r AS (
    SELECT sum(row_rows * row_rows) AS a11, sum(row_rows * operators) AS a12,
           sum(operators * operators) AS a22,
           sum(row_rows * (excess - v.varlena_ms * varlena)) AS b1,
           sum(operators * (excess - v.varlena_ms * varlena)) AS b2
    FROM filter_excess, v WHERE row_rows > 0 AND batch = 0
)
SELECT v.varlena_ms, b.batch_ms,
       (r.b1 * r.a22 - r.a12 * r.b2) / (r.a11 * r.a22 - r.a12 * r.a12) AS row_ms,
       (r.a11 * r.b2 - r.a12 * r.b1) / (r.a11 * r.a22 - r.a12 * r.a12) AS operator_ms
FROM v, b, r;
SELECT round((batch_ms * 1e6)::numeric, 3) AS batch_clause_ns,
       round((varlena_ms * 1e6)::numeric, 3) AS varlena_ns,
       round((row_ms * 1e6)::numeric, 3) AS row_clause_ns,
       round((operator_ms * 1e6)::numeric, 3) AS row_operator_ns
FROM filter_fit;
SELECT round((f.batch_ms / u.unit_ms)::numeric, 5) AS "tessera.filter_clause_cost",
       round((f.row_ms / u.unit_ms)::numeric, 5) AS "tessera.filter_row_clause_cost",
       round((f.operator_ms / u.unit_ms)::numeric, 5) AS "tessera.filter_row_operator_cost",
       round((f.varlena_ms / u.unit_ms)::numeric, 5) AS "tessera.deform_varlena_cost"
FROM filter_fit AS f, model_unit AS u;
-- How well each filter sample fits, its time over the base against the model's.
SELECT e.relation, e.name, round(e.excess::numeric, 3) AS excess_ms,
       round((e.batch * f.batch_ms + e.varlena * f.varlena_ms + e.row_rows * f.row_ms +
              e.operators * f.operator_ms)::numeric, 3) AS predicted_ms
FROM filter_excess AS e, filter_fit AS f
ORDER BY e.relation, e.name;
\o
