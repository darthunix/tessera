-- The anykey family: groupings, DISTINCT and UNION keyed by types a word
-- does not hold (text, numeric), whose values get numbers through a
-- dictionary, and hash joins by such keys, whose table keeps the values'
-- hashes. A ratio below one is the win.
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

-- Grouping by a text expression (99 groups), a text column (450 000
-- groups), with aggregates, by numeric (1000 groups), DISTINCT of text and
-- UNION of text.
SELECT pg_temp.measure_pair('any_text_few',
    'SELECT count(*), sum(c) FROM (SELECT substr(b, 1, 4) AS k, count(*) AS c FROM bench_mixed GROUP BY 1) AS q', :repetitions);
SELECT pg_temp.measure_pair('any_text_col',
    'SELECT count(*), sum(c) FROM (SELECT e, count(*) AS c FROM bench_mixed GROUP BY 1) AS q', :repetitions);
SELECT pg_temp.measure_pair('any_text_agg',
    'SELECT count(*), sum(s) FROM (SELECT substr(b, 1, 4) AS k, sum(a) AS s, max(d) FROM bench_mixed GROUP BY 1) AS q', :repetitions);
SELECT pg_temp.measure_pair('any_numeric',
    'SELECT count(*), sum(c) FROM (SELECT (a % 1000)::numeric AS k, count(*) AS c FROM bench_mixed GROUP BY 1) AS q', :repetitions);
SELECT pg_temp.measure_pair('any_distinct',
    'SELECT count(*) FROM (SELECT DISTINCT substr(e, 1, 5) FROM bench_mixed) AS q', :repetitions);
SELECT pg_temp.measure_pair('any_union',
    'SELECT count(*) FROM (SELECT substr(b, 1, 5) FROM bench_mixed UNION SELECT substr(e, 1, 5) FROM bench_mixed) AS q', :repetitions);
-- Joins of 2 000 000 rows with 100 000 by text, by numeric, by text and
-- an integer, and a semi-join by text.
SELECT pg_temp.measure_pair('any_join_text',
    'SELECT count(*), sum(d.w) FROM bench_tfact AS f JOIN bench_tdim AS d ON f.k = d.k', :repetitions);
SELECT pg_temp.measure_pair('any_join_numeric',
    'SELECT count(*), sum(d.w) FROM bench_tfact AS f JOIN bench_tdim AS d ON f.n = d.n', :repetitions);
SELECT pg_temp.measure_pair('any_join_two',
    'SELECT count(*), sum(d.w) FROM bench_tfact AS f JOIN bench_tdim AS d ON f.k = d.k AND f.w = d.w', :repetitions);
SELECT pg_temp.measure_pair('any_join_semi',
    'SELECT count(*) FROM bench_tfact AS f WHERE EXISTS (SELECT FROM bench_tdim AS d WHERE d.k = f.k AND d.w < 10)', :repetitions);

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
FROM unnest(ARRAY['any_text_few', 'any_text_col', 'any_text_agg', 'any_numeric', 'any_distinct', 'any_union', 'any_join_text', 'any_join_numeric', 'any_join_two', 'any_join_semi']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['any_text_few', 'any_text_col', 'any_text_agg', 'any_numeric', 'any_distinct', 'any_union', 'any_join_text', 'any_join_numeric', 'any_join_two', 'any_join_semi']) AS name \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
