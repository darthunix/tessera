-- The rowwise family: tables without clauses read under a parent of the
-- core that takes rows one at a time: aggregates Tessera does not compute,
-- a window function, a limit of one row. With Tessera on, the native scan
-- serves the rows; with it off, the core's sequential scan. A ratio below
-- one is the win.
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
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

-- bit_or, which TessAgg does not compute, of a column and of an expression.
SELECT pg_temp.measure_pair('row_column',
    'SELECT bit_or(c1) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('row_expr',
    'SELECT bit_or(c1 % 1000) FROM bench_narrow', :repetitions);
-- One column of sixty, and a text column.
SELECT pg_temp.measure_pair('row_wide',
    'SELECT bit_or(c60) FROM bench_wide', :repetitions);
SELECT pg_temp.measure_pair('row_text',
    'SELECT max(b) FROM bench_mixed', :repetitions);
-- A window function over every row.
SELECT pg_temp.measure_pair('row_window',
    'SELECT max(n) FROM (SELECT c1, row_number() OVER () AS n FROM bench_narrow) AS s',
    :repetitions);
-- The first row only.
SELECT pg_temp.measure_pair('row_first',
    'SELECT c1 FROM bench_narrow LIMIT 1', :repetitions);

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
FROM unnest(ARRAY['row_column', 'row_expr', 'row_wide', 'row_text', 'row_window',
                  'row_first']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['row_column', 'row_expr', 'row_wide', 'row_text', 'row_window',
                  'row_first']) AS name \gexec
\o
DEALLOCATE ALL;
