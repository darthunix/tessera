-- The tax family: queries where a batch plan cannot win. With Tessera on,
-- TessLimit stands above a pack node above the core plan; with it off,
-- the core Limit runs. The ratio is the cost of the batch boundary.
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

SELECT pg_temp.measure_pair('limit_1',
    'SELECT c1 FROM bench_narrow LIMIT 1', :repetitions);
SELECT pg_temp.measure_pair('limit_1000',
    'SELECT count(*) FROM (SELECT c1 FROM bench_narrow LIMIT 1000) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('limit_1m',
    'SELECT count(*) FROM (SELECT c1 FROM bench_narrow LIMIT 1000000) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('offset_1m',
    'SELECT count(*) FROM (SELECT c1 FROM bench_narrow OFFSET 1000000 LIMIT 10) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('sort_limit',
    'SELECT c1 FROM bench_narrow ORDER BY c1 DESC LIMIT 10', :repetitions);
SELECT pg_temp.measure_pair('wide_limit',
    'SELECT count(*) FROM (SELECT c60 FROM bench_wide LIMIT 200000) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('mixed_limit',
    'SELECT count(b) FROM (SELECT b FROM bench_mixed LIMIT 400000) AS s',
    :repetitions);
-- Computed targets under a limit: two int4 chains the filter computes for
-- the rows served, and a text expression computed row by row.
SELECT pg_temp.measure_pair('limit_expr',
    'SELECT sum(x + y) FROM (SELECT c1 + 1 AS x, c2 * 2 AS y FROM bench_narrow WHERE c1 > 100 LIMIT 1000000) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('limit_text',
    'SELECT count(x) FROM (SELECT b || ''x'' AS x FROM bench_mixed WHERE a > 250000 LIMIT 100000) AS s',
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
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_limit_1;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_limit_1000;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_limit_1m;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_offset_1m;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_sort_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_wide_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_limit_expr;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_limit_text;
SET tessera.enable = off;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_limit_1;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_limit_1000;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_limit_1m;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_offset_1m;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_sort_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_wide_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed_limit;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_limit_expr;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_limit_text;
\o
DEALLOCATE ALL;
