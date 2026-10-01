-- The sort family: ORDER BY over bench_sort, whose keys lie in no order of
-- the rows. Each query skips every row with OFFSET, so the Limit above the
-- sort reads all of it and returns nothing: the time is the scan, the
-- sort and the reading of its output. work_mem is large enough that the
-- core sorts in memory. A ratio below one is the win.
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
SET work_mem = '512MB';
-- The data set's multiplier: the offset below skips every row.
SELECT 2000000 * scale AS rows FROM bench_scale \gset

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

-- One key: a unique int4, an int8 past the int4 range, 1000 values.
SELECT pg_temp.measure_pair('order_int',
    format('SELECT k4, v FROM bench_sort ORDER BY k4 OFFSET %s', :rows),
    :repetitions);
SELECT pg_temp.measure_pair('order_big',
    format('SELECT k8, v FROM bench_sort ORDER BY k8 OFFSET %s', :rows),
    :repetitions);
SELECT pg_temp.measure_pair('order_few',
    format('SELECT few, v FROM bench_sort ORDER BY few OFFSET %s', :rows),
    :repetitions);
-- Two int4 keys, the first with 1000 values.
SELECT pg_temp.measure_pair('order_multi',
    format('SELECT few, k4, v FROM bench_sort ORDER BY few, k4 OFFSET %s', :rows),
    :repetitions);
-- Descending with NULLs (every 11th row) last.
SELECT pg_temp.measure_pair('order_desc',
    format('SELECT nul, v FROM bench_sort ORDER BY nul DESC NULLS LAST OFFSET %s', :rows),
    :repetitions);
-- A key already in the order of the rows.
SELECT pg_temp.measure_pair('order_sorted',
    format('SELECT sorted, v FROM bench_sort ORDER BY sorted OFFSET %s', :rows),
    :repetitions);
-- A text column the sort carries.
SELECT pg_temp.measure_pair('order_wide',
    format('SELECT k4, t, v FROM bench_sort ORDER BY k4 OFFSET %s', :rows),
    :repetitions);
-- The sorted rows read by a parent: an int column and a text column
-- gathered from the sort's records, counted by an aggregate above.
SELECT pg_temp.measure_pair('order_out',
    'SELECT count(v) FROM (SELECT k4, v FROM bench_sort ORDER BY k4) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('order_text_out',
    'SELECT count(t) FROM (SELECT k4, t FROM bench_sort ORDER BY k4) AS s',
    :repetitions);
-- Top-N: the first rows of a sort under LIMIT, few and many, keys in no
-- order, keys in the reverse of the rows' order, where every row is better
-- than the ones kept so far, and an offset before the rows returned.
SELECT pg_temp.measure_pair('topn_few',
    'SELECT k4, v FROM bench_sort ORDER BY k4 LIMIT 10', :repetitions);
SELECT pg_temp.measure_pair('topn_many',
    'SELECT count(v) FROM (SELECT k4, v FROM bench_sort ORDER BY k4 LIMIT 100000) AS s',
    :repetitions);
SELECT pg_temp.measure_pair('topn_reverse',
    'SELECT sorted, v FROM bench_sort ORDER BY sorted DESC LIMIT 10', :repetitions);
SELECT pg_temp.measure_pair('topn_offset',
    'SELECT k4, v FROM bench_sort ORDER BY k4 OFFSET 1000 LIMIT 10', :repetitions);
-- Distinct: the distinct values of a key of 1000 values and of a unique
-- one, counted above; count(DISTINCT) over the whole table and per group.
SELECT pg_temp.measure_pair('distinct_few',
    'SELECT count(*) FROM (SELECT DISTINCT few FROM bench_sort) AS s', :repetitions);
SELECT pg_temp.measure_pair('distinct_many',
    'SELECT count(*) FROM (SELECT DISTINCT k4 FROM bench_sort) AS s', :repetitions);
SELECT pg_temp.measure_pair('distinct_agg',
    'SELECT count(DISTINCT few) FROM bench_sort', :repetitions);
SELECT pg_temp.measure_pair('distinct_group',
    'SELECT count(*), sum(n) FROM (SELECT few, count(DISTINCT k4 % 100) AS n FROM bench_sort GROUP BY few) AS s',
    :repetitions);
-- A sort over a filter that keeps one row in ten.
SELECT pg_temp.measure_pair('order_filter',
    format('SELECT k4, v FROM bench_sort WHERE few < 100 ORDER BY k4 OFFSET %s', :rows),
    :repetitions);
-- Keys of other types: numeric and text under "C", whose words are their
-- abbreviated keys, also under LIMIT (top-N), and text of the default
-- collation after an int4 of 1000 values.
SELECT pg_temp.measure_pair('order_numeric',
    format('SELECT k4::numeric * 1.5 AS n, v FROM bench_sort ORDER BY n OFFSET %s', :rows),
    :repetitions);
SELECT pg_temp.measure_pair('order_text_c',
    format('SELECT t, v FROM bench_sort ORDER BY t COLLATE "C" OFFSET %s', :rows),
    :repetitions);
SELECT pg_temp.measure_pair('topn_numeric',
    'SELECT k4::numeric * 1.5 AS n, v FROM bench_sort ORDER BY n LIMIT 10', :repetitions);
SELECT pg_temp.measure_pair('topn_text_c',
    'SELECT t, v FROM bench_sort ORDER BY t COLLATE "C" LIMIT 10', :repetitions);
SELECT pg_temp.measure_pair('order_few_text',
    format('SELECT few, t, v FROM bench_sort ORDER BY few, t OFFSET %s', :rows),
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

-- The cached plans of both modes, with the sort method and memory of each.
\o plans.txt
SET tessera.enable = on;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['order_int', 'order_big', 'order_few', 'order_multi', 'order_desc',
                  'order_sorted', 'order_wide', 'order_out', 'order_text_out',
                  'topn_few', 'topn_many', 'topn_reverse', 'topn_offset',
                  'distinct_few', 'distinct_many', 'distinct_agg', 'distinct_group',
                  'order_filter', 'order_numeric', 'order_text_c', 'order_few_text', 'topn_numeric', 'topn_text_c']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['order_int', 'order_big', 'order_few', 'order_multi', 'order_desc',
                  'order_sorted', 'order_wide', 'order_out', 'order_text_out',
                  'topn_few', 'topn_many', 'topn_reverse', 'topn_offset',
                  'distinct_few', 'distinct_many', 'distinct_agg', 'distinct_group',
                  'order_filter', 'order_numeric', 'order_text_c', 'order_few_text', 'topn_numeric', 'topn_text_c']) AS name \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
