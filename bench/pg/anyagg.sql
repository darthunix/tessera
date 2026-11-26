-- The anyagg family: aggregates without GROUP BY that TessAgg computes
-- through the core's transition and final functions over the batches
-- (text, numeric, int8 and float8 states, bit_or), alone, over a filter and
-- three together; with GROUP BY, the generic states and the ones the node
-- keeps in its records (sum states, numeric extremes). A ratio below one is
-- the win.
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

-- The family's own table, made once: a bigint w whose every value has 19
-- digits, past a decimal's 18, of both signs and in no order, and its twin
-- s = w / 10 of 18 digits; a key r whose values come in runs of 64 rows,
-- so that a word of rows holds one group or two, and a key p whose values
-- follow one another, so that a word holds 64.
CREATE TABLE IF NOT EXISTS bench_digits AS
SELECT g / 64 % 100 AS r, g % 100 AS p, w, w / 10 AS s
FROM generate_series(1, 1000000) AS g,
     LATERAL (SELECT CASE WHEN g % 3 = 0 THEN -1 ELSE 1 END *
                     (1000000000000000000 + g::bigint * 7919 % 1000003 * 8000000000000) AS w) AS v;
VACUUM (ANALYZE) bench_digits;

\ir reading.sql

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
BEGIN
    PERFORM set_config('tessera.enable', mode, false);
    FOR warmup IN 1..5 LOOP
        PERFORM pg_temp.reading(format('EXECUTE %I', statement_name));
    END LOOP;
    FOR sample IN 1..repetitions LOOP
        INSERT INTO timings
        VALUES (test_name, mode, sample,
                pg_temp.reading(format('EXECUTE %I', statement_name)));
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
SELECT pg_temp.measure_pair('g_max_numeric', 'SELECT max(a::numeric) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_max_numeric_col', 'SELECT max(n) FROM bench_tfact', :repetitions);
SELECT pg_temp.measure_pair('g_avg_float8', 'SELECT avg(a::float8) FROM bench_mixed', :repetitions);
SELECT pg_temp.measure_pair('g_bit_or', 'SELECT bit_or(c1) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('g_filter', 'SELECT max(b) FROM bench_mixed WHERE a > 250000', :repetitions);
SELECT pg_temp.measure_pair('g_three', 'SELECT max(b), sum(c), avg(f) FROM bench_mixed', :repetitions);
-- With GROUP BY: 100 groups and 50 000 groups.
SELECT pg_temp.measure_pair('g_group_few', 'SELECT count(*), max(m) FROM (SELECT d % 100, max(b) AS m, sum(c), avg(f) FROM bench_mixed GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_many', 'SELECT count(*), max(m) FROM (SELECT a % 50000, max(b) AS m, sum(c) FROM bench_mixed GROUP BY 1) AS g', :repetitions);
-- min and max of numeric (an expression's decimals and a column's numeric
-- values), sum of smallint, with 100 groups.
SELECT pg_temp.measure_pair('g_group_numeric', 'SELECT count(*), max(m) FROM (SELECT d % 100, max(a::numeric) AS m, min(a::numeric) FROM bench_mixed GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_numeric_col', 'SELECT count(*), max(m) FROM (SELECT w, max(n) AS m, min(n) FROM bench_tfact GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_int2', 'SELECT count(*), max(s) FROM (SELECT d % 100, sum((a % 30000)::int2) AS s FROM bench_mixed GROUP BY 1) AS g', :repetitions);
-- Sums that reach the result, with 100 groups: the outer queries above
-- read no sum, and the planner drops the sums they do not read. sum and
-- avg of int8 keep sum states of their integers, sum of numeric of an
-- expression's decimals.
SELECT pg_temp.measure_pair('g_group_sum', 'SELECT count(*), sum(s), sum(v) FROM (SELECT d % 100, sum(c) AS s, avg(f) AS v FROM bench_mixed GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_sum_numeric', 'SELECT count(*), sum(s) FROM (SELECT d % 100, sum(a::numeric) AS s FROM bench_mixed GROUP BY 1) AS g', :repetitions);
-- sum and avg of bigint values of 19 digits, each beside its twin of 18:
-- without groups, by groups whose keys come in runs (every row through
-- its group's sum of the word) and by groups whose keys follow one
-- another (three rows in four into their records one by one).
SELECT pg_temp.measure_pair('g_sum_digits19', 'SELECT sum(w) FROM bench_digits', :repetitions);
SELECT pg_temp.measure_pair('g_sum_digits18', 'SELECT sum(s) FROM bench_digits', :repetitions);
SELECT pg_temp.measure_pair('g_avg_digits19', 'SELECT avg(w) FROM bench_digits', :repetitions);
SELECT pg_temp.measure_pair('g_avg_digits18', 'SELECT avg(s) FROM bench_digits', :repetitions);
SELECT pg_temp.measure_pair('g_runs_digits19', 'SELECT count(*), sum(t) FROM (SELECT r, sum(w) AS t FROM bench_digits GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_runs_digits18', 'SELECT count(*), sum(t) FROM (SELECT r, sum(s) AS t FROM bench_digits GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_spread_digits19', 'SELECT count(*), sum(t) FROM (SELECT p, sum(w) AS t FROM bench_digits GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_spread_digits18', 'SELECT count(*), sum(t) FROM (SELECT p, sum(s) AS t FROM bench_digits GROUP BY 1) AS g', :repetitions);
-- Extremes of the types the node holds as integers, beside a twin of int4
-- over the same table: a date, a timestamp, an int2, booleans (bool_and
-- the least, bool_or the greatest), a time and a money; by the 1000 groups
-- of s, both extremes read by the outer query; and the twins with
-- DISTINCT, which changes no extreme.
SELECT pg_temp.measure_pair('g_minmax_date', 'SELECT min(d), max(d) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_minmax_timestamp', 'SELECT min(ts), max(ts) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_minmax_int2', 'SELECT min(s), max(s) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_bool_and_or', 'SELECT bool_and(b), bool_or(b) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_minmax_clock', 'SELECT min(tm), max(tm), min(mo), max(mo) FROM bench_clock', :repetitions);
SELECT pg_temp.measure_pair('g_minmax_int4', 'SELECT min(v), max(v) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_minmax_int4_distinct', 'SELECT min(DISTINCT v), max(DISTINCT v) FROM bench_dates', :repetitions);
SELECT pg_temp.measure_pair('g_group_minmax_dates', 'SELECT count(*), min(a), max(b) FROM (SELECT s, min(d) AS a, max(ts) AS b FROM bench_dates GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_minmax_int4', 'SELECT count(*), min(a), max(b) FROM (SELECT s, min(v) AS a, max(v) AS b FROM bench_dates GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('g_group_minmax_int4_distinct', 'SELECT count(*), min(a), max(b) FROM (SELECT s, min(DISTINCT v) AS a, max(DISTINCT v) AS b FROM bench_dates GROUP BY 1) AS g', :repetitions);

\copy timings TO 'timings.csv' CSV HEADER

\o summary.txt
SELECT test, mode, count(*) AS runs,
       round(min(reading), 3) AS min,
       round(percentile_disc(0.5) WITHIN GROUP (ORDER BY reading), 3) AS median,
       round(percentile_disc(0.1) WITHIN GROUP (ORDER BY reading), 3) AS p10,
       round(percentile_disc(0.9) WITHIN GROUP (ORDER BY reading), 3) AS p90
FROM timings
GROUP BY test, mode
ORDER BY test, mode DESC;
\o

-- The cached plans of both modes.
\o plans.txt
SET tessera.enable = on;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['g_max_text', 'g_sum_int8', 'g_avg_int8', 'g_sum_numeric', 'g_max_numeric', 'g_max_numeric_col', 'g_avg_float8', 'g_bit_or', 'g_filter', 'g_three', 'g_group_few', 'g_group_many', 'g_group_numeric', 'g_group_numeric_col', 'g_group_int2', 'g_group_sum', 'g_group_sum_numeric', 'g_sum_digits19', 'g_sum_digits18', 'g_avg_digits19', 'g_avg_digits18', 'g_runs_digits19', 'g_runs_digits18', 'g_spread_digits19', 'g_spread_digits18', 'g_minmax_date', 'g_minmax_timestamp', 'g_minmax_int2', 'g_bool_and_or', 'g_minmax_clock', 'g_minmax_int4', 'g_minmax_int4_distinct', 'g_group_minmax_dates', 'g_group_minmax_int4', 'g_group_minmax_int4_distinct']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['g_max_text', 'g_sum_int8', 'g_avg_int8', 'g_sum_numeric', 'g_max_numeric', 'g_max_numeric_col', 'g_avg_float8', 'g_bit_or', 'g_filter', 'g_three', 'g_group_few', 'g_group_many', 'g_group_numeric', 'g_group_numeric_col', 'g_group_int2', 'g_group_sum', 'g_group_sum_numeric', 'g_sum_digits19', 'g_sum_digits18', 'g_avg_digits19', 'g_avg_digits18', 'g_runs_digits19', 'g_runs_digits18', 'g_spread_digits19', 'g_spread_digits18', 'g_minmax_date', 'g_minmax_timestamp', 'g_minmax_int2', 'g_bool_and_or', 'g_minmax_clock', 'g_minmax_int4', 'g_minmax_int4_distinct', 'g_group_minmax_dates', 'g_group_minmax_int4', 'g_group_minmax_int4_distinct']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
\o
DEALLOCATE ALL;
