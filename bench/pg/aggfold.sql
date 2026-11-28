-- The aggfold family: aggregates of one expression under constants, which
-- TessAgg folds into states of the expression it shares, each case beside
-- a twin that keeps those states and derives nothing; and guards that
-- fold nothing. A ratio below one is the win.
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

-- The family's own table, made once: 2 million rows of a smallint s, an
-- integer i, a bigint b and a numeric(12,2) n of both signs and in no
-- order, a key k of 37 groups and a key m of a million, two rows each.
CREATE TABLE IF NOT EXISTS bench_fold AS
SELECT (v % 60001 - 30000)::smallint AS s,
       (v % 2000003 - 1000001)::integer AS i,
       (v % 2000003 - 1000001)::bigint * 1000003 AS b,
       ((v % 2000003 - 1000001) / 100.0)::numeric(12,2) AS n,
       g % 37 AS k, g % 1000000 AS m
FROM generate_series(1, 2000000) AS g,
     LATERAL (SELECT g::bigint * 7919 % 4000037 AS v) AS r;
VACUUM (ANALYZE) bench_fold;

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

-- The sums of ten forms of a column, k from 0 to 9, as a statement and as
-- the select list of a grouping whose outer query reads every sum.
CREATE FUNCTION pg_temp.ten(form text) RETURNS text LANGUAGE sql AS $$
    SELECT string_agg(format('sum(%s)', replace(form, 'K', k::text)), ', ' ORDER BY k)
    FROM generate_series(0, 9) AS k
$$;
CREATE FUNCTION pg_temp.ten_read(form text) RETURNS text LANGUAGE sql AS $$
    SELECT string_agg(format('sum(%s)', replace(form, 'K', k::text)), ' + ' ORDER BY k)
    FROM generate_series(0, 9) AS k
$$;

-- Without groups: two forms, the least a fold takes, and ten, of each
-- type, beside their twins, the states the fold keeps.
SELECT pg_temp.measure_pair('fold_two_int4', 'SELECT sum(i), sum(i + 1) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('twin_two_int4', 'SELECT sum(i), count(i) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_int2', format('SELECT %s FROM bench_fold', pg_temp.ten('s + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int2', 'SELECT sum(s::integer), count(s) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_int4', format('SELECT %s FROM bench_fold', pg_temp.ten('i * 2 + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int4', 'SELECT sum(i), count(i), min(i), max(i) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_int8', format('SELECT %s FROM bench_fold', pg_temp.ten('b + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int8', 'SELECT avg(b), min(b), max(b) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_numeric', format('SELECT %s FROM bench_fold', pg_temp.ten('n + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_numeric', 'SELECT avg(n), count(n) FROM bench_fold', :repetitions);
-- Every kind of aggregate over one column, and an expression dearer than a
-- column.
SELECT pg_temp.measure_pair('fold_kinds_int4', 'SELECT sum(i), sum(i + 1), avg(i * 2), count(i - 3), min(i + 4), max(-i), sum(5 - i), avg(i + 6), min(i * 3), max(i - 7) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('twin_kinds_int4', 'SELECT sum(i), count(i), min(i), max(i) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_abs', format('SELECT %s FROM bench_fold', pg_temp.ten('abs(i) + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_abs', 'SELECT sum(abs(i)), count(abs(i)) FROM bench_fold', :repetitions);
-- With 37 groups and with a million, the outer query reading every sum.
SELECT pg_temp.measure_pair('fold_ten_int4_k37', format('SELECT count(*), sum(t) FROM (SELECT k, %s AS t FROM bench_fold GROUP BY 1) AS g', pg_temp.ten_read('i * 2 + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int4_k37', 'SELECT count(*), sum(t) FROM (SELECT k, sum(i) + count(i) + min(i) + max(i) AS t FROM bench_fold GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_int4_m1m', format('SELECT count(*), sum(t) FROM (SELECT m, %s AS t FROM bench_fold GROUP BY 1) AS g', pg_temp.ten_read('i * 2 + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int4_m1m', 'SELECT count(*), sum(t) FROM (SELECT m, sum(i) + count(i) + min(i) + max(i) AS t FROM bench_fold GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('fold_ten_numeric_m1m', format('SELECT count(*), sum(t) FROM (SELECT m, %s AS t FROM bench_fold GROUP BY 1) AS g', pg_temp.ten_read('n + K')), :repetitions);
-- The twin of numerics computes the same ten values from the same states
-- above the aggregates, sum(n) + k * count(n), as the case's reading adds
-- ten numerics a group: what differs is who derives them, the fold or the
-- core's executor.
SELECT pg_temp.measure_pair('twin_ten_numeric_m1m', format('SELECT count(*), sum(t) FROM (SELECT m, %s AS t FROM bench_fold GROUP BY 1) AS g', (SELECT string_agg(format('(sum(n) + %s * count(n))', k), ' + ' ORDER BY k) FROM generate_series(0, 9) AS k)), :repetitions);
-- DISTINCT, with 37 groups.
SELECT pg_temp.measure_pair('fold_distinct_int4_k37', 'SELECT count(*), sum(a + b + c) FROM (SELECT k, count(DISTINCT i + 1) AS a, sum(DISTINCT i * 2) AS b, avg(DISTINCT 10 - i) AS c FROM bench_fold GROUP BY 1) AS g', :repetitions);
SELECT pg_temp.measure_pair('twin_distinct_int4_k37', 'SELECT count(*), sum(a) FROM (SELECT k, avg(DISTINCT i) AS a FROM bench_fold GROUP BY 1) AS g', :repetitions);
-- Folded and unfolded aggregates together.
SELECT pg_temp.measure_pair('fold_mixed', 'SELECT sum(i), sum(i + 1), sum(i + 2), sum(b), max(n), sum(s::float8 + 0.5) FROM bench_fold', :repetitions);
-- Guards that fold nothing: a form alone, aggregates of a column alone, a
-- form of double precision.
SELECT pg_temp.measure_pair('guard_lone', 'SELECT sum(i + 1) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('guard_tie', 'SELECT sum(i), avg(i), count(i) FROM bench_fold', :repetitions);
SELECT pg_temp.measure_pair('guard_float', format('SELECT %s FROM bench_fold', pg_temp.ten('i::float8 + K')), :repetitions);
-- Groups past work_mem: a million groups spilled.
SET work_mem = '4MB';
SELECT pg_temp.measure_pair('fold_ten_int4_spill', format('SELECT count(*), sum(t) FROM (SELECT m, %s AS t FROM bench_fold GROUP BY 1) AS g', pg_temp.ten_read('i * 2 + K')), :repetitions);
SELECT pg_temp.measure_pair('twin_ten_int4_spill', 'SELECT count(*), sum(t) FROM (SELECT m, sum(i) + count(i) + min(i) + max(i) AS t FROM bench_fold GROUP BY 1) AS g', :repetitions);
RESET work_mem;
-- A parallel plan, which folds nothing.
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SELECT pg_temp.measure_pair('guard_parallel', format('SELECT %s FROM bench_fold', pg_temp.ten('i * 2 + K')), :repetitions);
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET max_parallel_workers_per_gather = :workers;

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
FROM unnest(ARRAY['fold_two_int4', 'twin_two_int4', 'fold_ten_int2', 'twin_ten_int2', 'fold_ten_int4', 'twin_ten_int4', 'fold_ten_int8', 'twin_ten_int8', 'fold_ten_numeric', 'twin_ten_numeric', 'fold_kinds_int4', 'twin_kinds_int4', 'fold_ten_abs', 'twin_ten_abs', 'fold_ten_int4_k37', 'twin_ten_int4_k37', 'fold_ten_int4_m1m', 'twin_ten_int4_m1m', 'fold_ten_numeric_m1m', 'twin_ten_numeric_m1m', 'fold_distinct_int4_k37', 'twin_distinct_int4_k37', 'fold_mixed', 'guard_lone', 'guard_tie', 'guard_float', 'fold_ten_int4_spill', 'twin_ten_int4_spill', 'guard_parallel']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['fold_two_int4', 'twin_two_int4', 'fold_ten_int2', 'twin_ten_int2', 'fold_ten_int4', 'twin_ten_int4', 'fold_ten_int8', 'twin_ten_int8', 'fold_ten_numeric', 'twin_ten_numeric', 'fold_kinds_int4', 'twin_kinds_int4', 'fold_ten_abs', 'twin_ten_abs', 'fold_ten_int4_k37', 'twin_ten_int4_k37', 'fold_ten_int4_m1m', 'twin_ten_int4_m1m', 'fold_ten_numeric_m1m', 'twin_ten_numeric_m1m', 'fold_distinct_int4_k37', 'twin_distinct_int4_k37', 'fold_mixed', 'guard_lone', 'guard_tie', 'guard_float', 'fold_ten_int4_spill', 'twin_ten_int4_spill', 'guard_parallel']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
\o
DEALLOCATE ALL;
