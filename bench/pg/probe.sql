-- The probe family: where the time of a parallel query goes. A table of
-- 28 pages, where all the time is the fixed cost of launching and finishing
-- the workers; the win family's count_all and dense with one, two and four
-- workers; and the plans of both modes with the time and rows of every
-- worker (EXPLAIN VERBOSE), for the same cases. Both modes throughout.
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

-- The small table: any difference between the modes is the batch nodes'
-- own fixed cost, counters and callbacks included.
DROP TABLE IF EXISTS bench_tiny;
CREATE TABLE bench_tiny AS SELECT g AS a FROM generate_series(1, 5000) AS g;
VACUUM (ANALYZE) bench_tiny;
SET tessera.enable = off;
SELECT count(*) FROM bench_tiny;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 0;
SELECT pg_temp.measure_pair('tiny_serial',
    'SELECT count(*) FROM bench_tiny WHERE a > 0', :repetitions);
SET max_parallel_workers_per_gather = 1;
SELECT pg_temp.measure_pair('tiny_w1',
    'SELECT count(*) FROM bench_tiny WHERE a > 0', :repetitions);
SET max_parallel_workers_per_gather = 2;
SELECT pg_temp.measure_pair('tiny_w2',
    'SELECT count(*) FROM bench_tiny WHERE a > 0', :repetitions);
SET parallel_leader_participation = off;
SELECT pg_temp.measure_pair('tiny_w2_noleader',
    'SELECT count(*) FROM bench_tiny WHERE a > 0', :repetitions);
RESET parallel_leader_participation;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;

-- The big tables at the core's own costs, with one, two and four workers;
-- the narrow table is told to offer four whatever its size.
ALTER TABLE bench_narrow SET (parallel_workers = 4);
SET max_parallel_workers_per_gather = 1;
SELECT pg_temp.measure_pair('count_all_w1',
    'SELECT count(*) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('dense_w1',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < %s',
           1500000 * :scale),
    :repetitions);
SET max_parallel_workers_per_gather = 2;
SELECT pg_temp.measure_pair('count_all_w2',
    'SELECT count(*) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('dense_w2',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < %s',
           1500000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('mixed_w2',
    format('SELECT count(*) FROM bench_mixed WHERE a > %s AND d < %s',
           250000 * :scale, 400000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('wide_w2',
    'SELECT count(*) FROM bench_wide WHERE c2 + 1 < 1000', :repetitions);
SET max_parallel_workers_per_gather = 4;
SELECT pg_temp.measure_pair('count_all_w4',
    'SELECT count(*) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('dense_w4',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < %s',
           1500000 * :scale),
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

-- The plans of both modes with every worker's time and rows. A cached
-- plan is replanned when its table changed, so the settings of each case
-- are in effect again.
\o plans.txt
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SET tessera.enable = on;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_tiny_w2;
SET tessera.enable = off;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_tiny_w2;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET tessera.enable = on;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_count_all_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_dense_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_wide_w2;
SET tessera.enable = off;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_count_all_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_dense_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed_w2;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_wide_w2;
SET max_parallel_workers_per_gather = 4;
SET tessera.enable = on;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_count_all_w4;
SET tessera.enable = off;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_count_all_w4;
\o

ALTER TABLE bench_narrow RESET (parallel_workers);
DROP TABLE bench_tiny;
