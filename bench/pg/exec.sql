-- The exec family (plan item 4.27, review section G): the costs of
-- execution the review named. Rows a sort and a grouping serve one at a
-- time to a parent of the core; text keys whose abbreviated keys
-- tell nothing apart, under ICU and under "C", and a control whose do; a
-- join whose outer keys are half one key, spilling, and a control without
-- the skew; a sort and a grouping past work_mem whose rows carry text; the
-- planning of a query of many expressions. A ratio of Tessera on to off
-- below one is the win.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 11
\endif
\if :{?workers}
\else
\set workers 0
\endif
SET jit = off;
-- Parallel workers per Gather, for both modes; none unless asked for.
SET max_parallel_workers_per_gather = :workers;

-- The family's own tables, made once: text keys of a common prefix of nine
-- bytes (and the same digits first, for the control), in no order; outer
-- keys of a join, every second row key 1, the others spread over bench_dim.
CREATE TABLE IF NOT EXISTS bench_prefix AS
SELECT 'customer#' || lpad(k::text, 9, '0') AS p,
       lpad(k::text, 9, '0') || '#customer' AS s, g AS v
FROM generate_series(1, 1000000) AS g,
     LATERAL (SELECT (g::bigint * 7919 % 1000000)::int AS k) AS key;
CREATE TABLE IF NOT EXISTS bench_skew AS
SELECT CASE WHEN g % 2 = 0 THEN 1 ELSE (g::bigint * 7919 % 100000)::int + 1 END AS k,
       g AS v
FROM generate_series(1, 2000000) AS g;
VACUUM (ANALYZE) bench_prefix;
VACUUM (ANALYZE) bench_skew;

CREATE TEMP TABLE timings
(
    test text,
    mode text,
    run integer,
    milliseconds numeric
);

CREATE TEMP TABLE cases
(
    test text,
    work_mem text,
    prepared boolean
);

/*
 * A prepared statement without parameters is planned at its first
 * execution and cached, so the mode set here decides its plan for good:
 * every statement is prepared twice, once per mode. An unprepared case
 * runs its text each time, planning included.
 */
CREATE FUNCTION pg_temp.measure(test_name text, mode text, statement text,
                                prepared boolean, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
    command text := CASE WHEN prepared THEN format('EXECUTE %I', statement)
                         ELSE statement END;
BEGIN
    PERFORM set_config('tessera.enable', mode, false);
    FOR warmup IN 1..5 LOOP
        EXECUTE command;
    END LOOP;
    FOR sample IN 1..repetitions LOOP
        started_at := clock_timestamp();
        EXECUTE command;
        INSERT INTO timings
        VALUES (test_name, mode, sample,
                1000 * extract(epoch FROM clock_timestamp() - started_at));
    END LOOP;
END
$function$;

CREATE FUNCTION pg_temp.measure_pair(test_name text, sql text, memory text,
                                     prepared boolean, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM set_config('work_mem', memory, false);
    INSERT INTO cases VALUES (test_name, memory, prepared);
    IF prepared THEN
        EXECUTE format('PREPARE on_%I AS %s', test_name, sql);
        EXECUTE format('PREPARE off_%I AS %s', test_name, sql);
    END IF;
    -- run.sh measure with CASES: only the cases it matches are timed.
    IF test_name !~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') THEN
        RETURN;
    END IF;
    IF prepared THEN
        PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, true, repetitions);
        PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, true, repetitions);
    ELSE
        PERFORM pg_temp.measure(test_name, 'on', sql, false, repetitions);
        PERFORM pg_temp.measure(test_name, 'off', sql, false, repetitions);
    END IF;
END
$function$;

-- Rows served one at a time to a parent of the core, a window function:
-- eight columns of a sort in memory and the two of a grouping's groups.
-- (Under such a parent an append is the core's.)
SELECT pg_temp.measure_pair('out_sort',
    'SELECT max(n + (c1 # c2 # c3 # c4 # c5 # c6 # c7 # c8)) FROM (SELECT *, row_number() OVER () AS n FROM (SELECT * FROM bench_narrow ORDER BY c2 DESC) AS s) AS w',
    '256MB', true, :repetitions);
SELECT pg_temp.measure_pair('out_group',
    'SELECT max(w + (k # n)) FROM (SELECT k, n, row_number() OVER () AS w FROM (SELECT c1 / 4 AS k, count(*) AS n FROM bench_narrow GROUP BY 1) AS s) AS x',
    '256MB', true, :repetitions);
-- Text keys whose first nine bytes are the same: ICU's sort keys and the
-- bytes under "C" tell no row apart; the control's do.
SELECT pg_temp.measure_pair('prefix_icu',
    'SELECT p, v FROM bench_prefix ORDER BY p COLLATE "en-US-x-icu" OFFSET 1000000',
    '256MB', true, :repetitions);
SELECT pg_temp.measure_pair('prefix_c',
    'SELECT p, v FROM bench_prefix ORDER BY p COLLATE "C" OFFSET 1000000',
    '256MB', true, :repetitions);
SELECT pg_temp.measure_pair('noprefix_icu',
    'SELECT s, v FROM bench_prefix ORDER BY s COLLATE "en-US-x-icu" OFFSET 1000000',
    '256MB', true, :repetitions);
-- A join past work_mem: half of the outer rows one key, and a control.
SELECT pg_temp.measure_pair('skew_join',
    'SELECT count(*), sum(d.d1) FROM bench_skew AS o JOIN bench_dim AS d ON o.k = d.id',
    '1MB', true, :repetitions);
SELECT pg_temp.measure_pair('even_join',
    'SELECT count(*), sum(d.d1) FROM bench_fact AS f JOIN bench_dim AS d ON f.fk = d.id',
    '1MB', true, :repetitions);
-- Text past work_mem: a sort whose runs carry a text column that a parent
-- reads back from them, and a grouping by a text key whose rows wait on
-- disk for their partition.
SELECT pg_temp.measure_pair('runs_text',
    'SELECT count(t) FROM (SELECT k4, t FROM bench_sort ORDER BY k4) AS s',
    '4MB', true, :repetitions);
SELECT pg_temp.measure_pair('rows_text',
    'SELECT count(*), sum(s) FROM (SELECT b, sum(d) AS s FROM bench_mixed GROUP BY b) AS g',
    '4MB', true, :repetitions);
-- Planning alone: EXPLAIN of thirty aggregates of expressions and a
-- filter, planned every time.
SELECT pg_temp.measure_pair('plan_exprs',
    'EXPLAIN SELECT ' || string_agg(format('sum(a * %s + %s), max(a - %s)', i, i, i), ', ') ||
    ' FROM bench_tiny WHERE a % 7 <> 3 AND a + 1 > 2',
    '4MB', false, :repetitions)
FROM generate_series(1, 15) AS i;
RESET work_mem;

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

-- The plans of both modes at each case's work_mem.
\o plans.txt
SELECT format('SET work_mem = %L', work_mem),
       'SET tessera.enable = on',
       CASE WHEN prepared
            THEN format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', test)
       END,
       'SET tessera.enable = off',
       CASE WHEN prepared
            THEN format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', test)
       END
FROM cases \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
