-- The setop family: UNION ALL and UNION over batch scans. The core's
-- Append takes rows from its children, so a batch parent above it packs
-- them again; the cases measure that break and, with TessAppend, its
-- removal: an aggregate and a hash join over UNION ALL, an aggregate over
-- a partitioned table, UNION ALL returned as rows, and UNION with few and
-- many distinct values. A ratio below one is the win.
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

-- An aggregate over UNION ALL of two filtered scans, a million rows each.
SELECT pg_temp.measure_pair('setop_count',
    format('SELECT count(*), sum(c) FROM (SELECT c2 AS c FROM bench_narrow WHERE c1 > %s '
           'UNION ALL SELECT f1 FROM bench_fact WHERE f1 > %s) AS s',
           1000000 * :scale, 1000000 * :scale),
    :repetitions);
-- UNION ALL as the outer side of a hash join, one branch a plain scan.
SELECT pg_temp.measure_pair('setop_join',
    format('SELECT count(*), sum(d.d1) FROM (SELECT fk FROM bench_fact WHERE f1 > %s '
           'UNION ALL SELECT k FROM bench_dup) AS s JOIN bench_dim AS d ON d.id = s.fk',
           1000000 * :scale),
    :repetitions);
-- An aggregate over the four partitions of bench_part, one row in ten kept.
SELECT pg_temp.measure_pair('setop_part',
    'SELECT count(*), sum(k) FROM bench_part WHERE v < 100',
    :repetitions);
-- UNION ALL returned as rows: the offset skips all of them.
SELECT pg_temp.measure_pair('setop_rows',
    format('SELECT c1 FROM bench_narrow WHERE c1 > %s UNION ALL '
           'SELECT f1 FROM bench_fact WHERE f1 > %s OFFSET %s',
           1000000 * :scale, 1000000 * :scale, 2000000 * :scale),
    :repetitions);
-- UNION of 2.1 M rows with 1000 distinct values.
SELECT pg_temp.measure_pair('setop_few',
    'SELECT count(*) FROM (SELECT c1 % 1000 FROM bench_narrow UNION SELECT k % 1000 FROM bench_dup) AS s',
    :repetitions);
-- UNION of two filtered scans, 2.5 M rows with 2 M distinct values.
SELECT pg_temp.measure_pair('setop_many',
    format('SELECT count(*) FROM (SELECT c1 FROM bench_narrow WHERE c1 <= %s '
           'UNION SELECT c2 FROM bench_narrow WHERE c2 > %s) AS s',
           1500000 * :scale, 1000000 * :scale),
    :repetitions);
-- UNION of 2.5 M rows with 1000 distinct values the planner knows (the
-- statistics of c2 % 1000): with workers each groups its share.
SELECT pg_temp.measure_pair('setop_known',
    format('SELECT count(*) FROM (SELECT c2 %% 1000 FROM bench_narrow '
           'UNION SELECT c2 %% 1000 FROM bench_narrow WHERE c1 < %s) AS s',
           500000 * :scale),
    :repetitions);
-- A set operation within another: UNION of 2.1 M rows with 1000 values
-- as the left side of EXCEPT.
SELECT pg_temp.measure_pair('setop_nested',
    format('SELECT count(*) FROM ((SELECT c1 %% 1000 FROM bench_narrow UNION SELECT k %% 1000 FROM bench_dup) '
           'EXCEPT SELECT c2 %% 100 FROM bench_narrow WHERE c1 < %s) AS s',
           100000 * :scale),
    :repetitions);
-- UNION of branches whose clauses Tessera does not evaluate in batches.
SELECT pg_temp.measure_pair('setop_like',
    $q$SELECT count(*) FROM (SELECT a FROM bench_mixed WHERE b LIKE 'b-1%' UNION SELECT d FROM bench_mixed WHERE e LIKE 'e-2%') AS s$q$,
    :repetitions);
-- INTERSECT and EXCEPT, grouping both sides by every column: EXCEPT of
-- 500 000 integers and a third of them, EXCEPT ALL of 1000 values with
-- many copies each, INTERSECT of 500 000 texts and computed ones through a
-- dictionary, INTERSECT ALL of integers of 50 000 and 70 000 values.
SELECT pg_temp.measure_pair('setop_except',
    'SELECT count(*) FROM (SELECT a FROM bench_mixed EXCEPT SELECT d FROM bench_mixed WHERE d % 3 = 0) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('setop_except_all',
    'SELECT count(*) FROM (SELECT c1 % 1000 FROM bench_narrow EXCEPT ALL SELECT k % 1000 FROM bench_dup) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('setop_intersect_text',
    $q$SELECT count(*) FROM (SELECT b FROM bench_mixed INTERSECT SELECT 'b-' || (c * 2) FROM bench_mixed) AS s$q$,
    :repetitions);
SELECT pg_temp.measure_pair('setop_intersect_all',
    'SELECT count(*) FROM (SELECT c1 % 50000 FROM bench_narrow INTERSECT ALL SELECT c2 % 70000 FROM bench_narrow) AS s',
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
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['setop_count', 'setop_join', 'setop_part', 'setop_rows',
                  'setop_few', 'setop_many', 'setop_known', 'setop_nested', 'setop_like', 'setop_except', 'setop_except_all',
                  'setop_intersect_text', 'setop_intersect_all']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['setop_count', 'setop_join', 'setop_part', 'setop_rows',
                  'setop_few', 'setop_many', 'setop_known', 'setop_nested', 'setop_like', 'setop_except', 'setop_except_all',
                  'setop_intersect_text', 'setop_intersect_all']) AS name \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
