-- The win family: filters over large tables, with count above them. With
-- Tessera on, TessFilter stands above a pack node above the sequential
-- scan and serves rows to the core aggregate; with it off, the scan
-- filters. A ratio below one is expected once a native batch scan exists;
-- until then the family records the cost that scan has to beat.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 31
\endif
SET jit = off;
SET max_parallel_workers_per_gather = 0;

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
    'SELECT count(*) FROM bench_narrow WHERE c1 <= 1000000 AND c2 > 0',
    :repetitions);
SELECT pg_temp.measure_pair('dense',
    'SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < 1500000',
    :repetitions);
-- A value chain under the predicate.
SELECT pg_temp.measure_pair('expr',
    'SELECT count(*) FROM bench_narrow WHERE (c1 + 3) * 2 < 2000000',
    :repetitions);
-- A projected column the filter does not read.
SELECT pg_temp.measure_pair('sum_sparse',
    'SELECT sum(c3) FROM bench_narrow WHERE c1 % 100 = 0', :repetitions);
-- A row-wise residual behind the batch clause.
SELECT pg_temp.measure_pair('residual',
    'SELECT count(*) FROM bench_narrow WHERE c1 > 1000000 AND c2::bigint < 1500001',
    :repetitions);
-- A wide table, where deforming the row dominates.
SELECT pg_temp.measure_pair('wide',
    'SELECT count(*) FROM bench_wide WHERE c2 + 1 < 1000', :repetitions);
-- Two int4 clauses over a table with text columns; a text residual.
SELECT pg_temp.measure_pair('mixed',
    'SELECT count(*) FROM bench_mixed WHERE a > 250000 AND d < 400000',
    :repetitions);
SELECT pg_temp.measure_pair('mixed_text',
    'SELECT count(*) FROM bench_mixed WHERE a > 250000 AND b <> ''x''',
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
\o
DEALLOCATE ALL;
