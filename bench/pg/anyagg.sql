-- The anyagg family: aggregates without GROUP BY that TessAgg computes
-- through the core's transition and final functions over the batches
-- (text, numeric, int8 and float8 states, bit_or), alone, over a filter and
-- three together. A ratio below one is the win.
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
    -- run.sh measure with CASES: only the cases it matches are timed.
    IF test_name !~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') THEN
        RETURN;
    END IF;
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

SELECT pg_temp.measure_pair('g_max_text', 'SELECT max(b) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_sum_int8', 'SELECT sum(c) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_avg_int8', 'SELECT avg(f) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_sum_numeric', 'SELECT sum(a::numeric) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_avg_float8', 'SELECT avg(a::float8) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_bit_or', 'SELECT bit_or(c1) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('g_filter', 'SELECT max(b) FROM bench_mixed WHERE a > 250000', :repetitions);
SELECT pg_temp.measure_pair('g_three', 'SELECT max(b), sum(c), avg(f) FROM bench_mixed', :repetitions);
-- With GROUP BY: 100 groups and 50 000 groups.
SELECT pg_temp.measure_pair('g_group_few', 'SELECT count(*), max(m) FROM (SELECT d % 100, max(b) AS m, sum(c), avg(f) FROM bench_mixed GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_many', 'SELECT count(*), max(m) FROM (SELECT a % 50000, max(b) AS m, sum(c) FROM bench_mixed GROUP BY 1) AS g', :repetitions);

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
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['g_max_text', 'g_sum_int8', 'g_avg_int8', 'g_sum_numeric', 'g_avg_float8', 'g_bit_or', 'g_filter', 'g_three', 'g_group_few', 'g_group_many']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['g_max_text', 'g_sum_int8', 'g_avg_int8', 'g_sum_numeric', 'g_avg_float8', 'g_bit_or', 'g_filter', 'g_three', 'g_group_few', 'g_group_many']) AS name \gexec
\o
DEALLOCATE ALL;
