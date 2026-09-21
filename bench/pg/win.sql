-- The win family: filters over large tables, with aggregates above them.
-- With Tessera on, TessAgg stands above TessFilter above TessHeapScan;
-- with it off, the scan filters and the core aggregates. A ratio below
-- one is the win; the family records it against its previous run.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 31
\endif
\if :{?workers}
\else
\set workers 0
\endif
SET jit = off;
-- Parallel workers per Gather, for both modes; none unless asked for.
SET max_parallel_workers_per_gather = :workers;
-- The data set's multiplier: the constants below keep their selectivity.
SELECT scale FROM bench_scale \gset

CREATE TEMP TABLE timings
(
    test text,
    mode text,
    run integer,
    milliseconds numeric
);

/*
 * A prepared statement without parameters is planned at its first
 * execution and cached, so the mode set here decides its plan for good:
 * every statement is prepared twice, once per mode.
 */
CREATE FUNCTION pg_temp.measure(test_name text, mode text,
                                statement_name text, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
BEGIN
    PERFORM set_config('tessera.enable', mode, false);
    FOR warmup IN 1..5 LOOP
        EXECUTE format('EXECUTE %I', statement_name);
    END LOOP;
    FOR sample IN 1..repetitions LOOP
        started_at := clock_timestamp();
        EXECUTE format('EXECUTE %I', statement_name);
        INSERT INTO timings
        VALUES (test_name, mode, sample,
                1000 * extract(epoch FROM clock_timestamp() - started_at));
    END LOOP;
END
$function$;

CREATE FUNCTION pg_temp.measure_pair(test_name text, sql text,
                                     repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    EXECUTE format('PREPARE on_%I AS %s', test_name, sql);
    EXECUTE format('PREPARE off_%I AS %s', test_name, sql);
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

-- Selectivity: nothing, one row in a thousand, half, most.
SELECT pg_temp.measure_pair('nothing',
    'SELECT count(*) FROM bench_narrow WHERE c1 < 0', :repetitions);
SELECT pg_temp.measure_pair('sparse',
    'SELECT count(*) FROM bench_narrow WHERE c1 % 1000 = 0', :repetitions);
SELECT pg_temp.measure_pair('half',
    format('SELECT count(*) FROM bench_narrow WHERE c1 <= %s AND c2 > 0',
           1000000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('dense',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < %s',
           1500000 * :scale),
    :repetitions);
-- A value chain under the predicate.
SELECT pg_temp.measure_pair('expr',
    format('SELECT count(*) FROM bench_narrow WHERE (c1 + 3) * 2 < %s',
           2000000 * :scale),
    :repetitions);
-- A projected column the filter does not read.
SELECT pg_temp.measure_pair('sum_sparse',
    'SELECT sum(c3) FROM bench_narrow WHERE c1 % 100 = 0', :repetitions);
-- A row-wise residual behind the batch clause.
SELECT pg_temp.measure_pair('residual',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > %s AND c2::bigint < %s',
           1000000 * :scale, 1500000 * :scale + 1),
    :repetitions);
-- A wide table, where deforming the row dominates.
SELECT pg_temp.measure_pair('wide',
    'SELECT count(*) FROM bench_wide WHERE c2 + 1 < 1000', :repetitions);
-- Two int4 clauses over a table with text columns; a text residual.
SELECT pg_temp.measure_pair('mixed',
    format('SELECT count(*) FROM bench_mixed WHERE a > %s AND d < %s',
           250000 * :scale, 400000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('mixed_text',
    format('SELECT count(*) FROM bench_mixed WHERE a > %s AND b <> ''x''',
           250000 * :scale),
    :repetitions);
-- Aggregates over batches: a count without a filter, and four kernels
-- over one column behind a filter that keeps most rows.
SELECT pg_temp.measure_pair('count_all',
    'SELECT count(*) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('four_dense',
    'SELECT count(*), count(c1), sum(c1), min(c1), max(c1) FROM bench_narrow WHERE c1 > 100',
    :repetitions);
-- An argument over two columns: a chain with a column operand, evaluated
-- by the aggregate node through the projection provider.
SELECT pg_temp.measure_pair('two_columns',
    format('SELECT sum(c1 + c2) FROM bench_narrow WHERE c1 > %s', 1000000 * :scale),
    :repetitions);

\copy timings TO 'timings.csv' CSV HEADER

\o summary.txt
SELECT test, mode, count(*) AS runs,
       round(min(milliseconds), 3) AS min_ms,
       round(percentile_disc(0.5) WITHIN GROUP (ORDER BY milliseconds), 3) AS median_ms,
       round(percentile_disc(0.1) WITHIN GROUP (ORDER BY milliseconds), 3) AS p10_ms,
       round(percentile_disc(0.9) WITHIN GROUP (ORDER BY milliseconds), 3) AS p90_ms
FROM timings
GROUP BY test, mode
ORDER BY test, mode DESC;
\o

-- The cached plans of both modes.
\o plans.txt
SET tessera.enable = on;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_nothing;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_half;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_expr;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_sum_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_residual;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_wide;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed_text;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_count_all;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_four_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_two_columns;
SET tessera.enable = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_nothing;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_half;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_expr;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_sum_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_residual;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_wide;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed_text;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_count_all;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_four_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_two_columns;
\o
DEALLOCATE ALL;
