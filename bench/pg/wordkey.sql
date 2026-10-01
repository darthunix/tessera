-- The wordkey family: joins, groupings, DISTINCT and sorts keyed by types
-- the table keeps in a word besides int4 and int8 (date, timestamp, int2,
-- bool). A ratio below one is the win.
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
SET work_mem = '256MB';
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
    -- run.sh measure with CASES: only the cases it matches are timed.
    IF test_name !~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') THEN
        RETURN;
    END IF;
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

-- Grouping by a date (1826 groups), a timestamp (43824) and an int2 (1000).
SELECT pg_temp.measure_pair('key_date_group',
    'SELECT count(*), sum(n) FROM (SELECT d, count(*) AS n, sum(v) FROM bench_dates GROUP BY d) AS g', :repetitions);
SELECT pg_temp.measure_pair('key_ts_group',
    'SELECT count(*), sum(n) FROM (SELECT ts, count(*) AS n FROM bench_dates GROUP BY ts) AS g', :repetitions);
SELECT pg_temp.measure_pair('key_int2_group',
    'SELECT count(*), sum(n) FROM (SELECT s, b, count(*) AS n FROM bench_dates GROUP BY s, b) AS g', :repetitions);
-- A join on the date, DISTINCT of the timestamp, a sort by the date.
SELECT pg_temp.measure_pair('key_date_join',
    'SELECT count(*), sum(w) FROM bench_dates JOIN bench_days USING (d)', :repetitions);
SELECT pg_temp.measure_pair('key_ts_distinct',
    'SELECT count(*) FROM (SELECT DISTINCT ts FROM bench_dates) AS s', :repetitions);
SELECT pg_temp.measure_pair('key_date_order',
    format('SELECT d, v FROM bench_dates ORDER BY d OFFSET %s', 2000000 * :scale), :repetitions);

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
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['key_date_group', 'key_ts_group', 'key_int2_group', 'key_date_join', 'key_ts_distinct', 'key_date_order']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['key_date_group', 'key_ts_group', 'key_int2_group', 'key_date_join', 'key_ts_distinct', 'key_date_order']) AS name \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
