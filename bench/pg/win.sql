-- The win family: filters over large tables, with aggregates above them.
-- With Tessera on, TessAgg stands above TessFilter above TessHeapScan;
-- with it off, the scan filters and the core aggregates. A ratio below
-- one is the win; the family records it against its previous run.
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
-- The data set's multiplier: the constants below keep their selectivity.
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

-- Selectivity: nothing, one row in a thousand, half, most.
SELECT pg_temp.measure_pair('nothing',
    'SELECT count(*) FROM bench_narrow WHERE c1 < 0', :repetitions);
SELECT pg_temp.measure_pair('sparse',
    'SELECT count(*) FROM bench_narrow WHERE c1 % 1000 = 0', :repetitions);
SELECT pg_temp.measure_pair('half',
    format('SELECT count(*) FROM bench_narrow WHERE c1 <= %s AND c2 > 0',
           1000000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('dense',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > 100 AND c2 < %s',
           1500000 * :scale),
    :repetitions);
-- A value chain under the predicate.
SELECT pg_temp.measure_pair('expr',
    format('SELECT count(*) FROM bench_narrow WHERE (c1 + 3) * 2 < %s',
           2000000 * :scale),
    :repetitions);
-- A projected column the filter does not read.
SELECT pg_temp.measure_pair('sum_sparse',
    'SELECT sum(c3) FROM bench_narrow WHERE c1 % 100 = 0', :repetitions);
-- A row-wise residual behind the batch clause.
SELECT pg_temp.measure_pair('residual',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > %s AND c2::bigint < %s',
           1000000 * :scale, 1500000 * :scale + 1),
    :repetitions);
-- A wide table, where deforming the row dominates.
SELECT pg_temp.measure_pair('wide',
    'SELECT count(*) FROM bench_wide WHERE c2 + 1 < 1000', :repetitions);
-- Two int4 clauses over a table with text columns; a text residual.
SELECT pg_temp.measure_pair('mixed',
    format('SELECT count(*) FROM bench_mixed WHERE a > %s AND d < %s',
           250000 * :scale, 400000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('mixed_text',
    format('SELECT count(*) FROM bench_mixed WHERE a > %s AND b <> ''x''',
           250000 * :scale),
    :repetitions);
-- Aggregates over batches: a count without a filter, and four kernels
-- over one column behind a filter that keeps most rows.
SELECT pg_temp.measure_pair('count_all',
    'SELECT count(*) FROM bench_narrow', :repetitions);
SELECT pg_temp.measure_pair('four_dense',
    'SELECT count(*), count(c1), sum(c1), min(c1), max(c1) FROM bench_narrow WHERE c1 > 100',
    :repetitions);
-- An argument over two columns: a chain with a column operand, evaluated
-- by the aggregate node through the projection provider.
SELECT pg_temp.measure_pair('two_columns',
    format('SELECT sum(c1 + c2) FROM bench_narrow WHERE c1 > %s', 1000000 * :scale),
    :repetitions);
-- Two computed sides: the comparison's operand and the argument's are
-- expressions of their own, computed over the batch; half the rows pass.
SELECT pg_temp.measure_pair('tree',
    format('SELECT count(*) FROM bench_narrow WHERE (c1 + 1) * 2 < (c2 + 3) * 3 - %s',
           1000000 * :scale + 10),
    :repetitions);
SELECT pg_temp.measure_pair('tree_sum',
    'SELECT sum(((c1 % 1000) + 1) * ((c2 % 1000) + 1)) FROM bench_narrow',
    :repetitions);
-- An OR of two comparisons, the right one over the rows the left one did
-- not keep; half the rows pass. A short IN list over a chain: four of ten.
SELECT pg_temp.measure_pair('either',
    format('SELECT count(*) FROM bench_narrow WHERE c1 < %s OR c2 > %s',
           500000 * :scale, 1500000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('in_list',
    'SELECT count(*) FROM bench_narrow WHERE c1 % 10 IN (1, 3, 5, 7)',
    :repetitions);
-- Conditional values: a CASE as an aggregate's argument, a third of the
-- rows in its first branch; a CASE under a filter, half the rows each way.
SELECT pg_temp.measure_pair('case_sum',
    'SELECT sum(CASE WHEN c1 % 3 = 0 THEN c2 ELSE 0 END) FROM bench_narrow',
    :repetitions);
SELECT pg_temp.measure_pair('case_filter',
    format('SELECT count(*) FROM bench_narrow WHERE CASE WHEN c1 < %s THEN c2 ELSE c3 END %% 2 = 0',
           1000000 * :scale),
    :repetitions);
-- GROUP BY over a table of groups: ten groups of an expression with
-- statistics (setup.sql), a hundred thousand of a foreign key column, and
-- a thousand over a filter that keeps half the rows.
SELECT pg_temp.measure_pair('group_few',
    'SELECT c1 % 10, count(*), sum(c2) FROM bench_narrow GROUP BY 1',
    :repetitions);
SELECT pg_temp.measure_pair('group_many',
    'SELECT fk, count(*), max(f1) FROM bench_fact GROUP BY fk',
    :repetitions);
SELECT pg_temp.measure_pair('group_filter',
    format('SELECT c2 %% 1000, count(*), sum(c3) FROM bench_narrow WHERE c1 > %s GROUP BY 1',
           1000000 * :scale),
    :repetitions);
-- An integer column against a bigint constant: a cross-type comparison;
-- half the rows pass.
SELECT pg_temp.measure_pair('int4_bigint',
    format('SELECT count(*) FROM bench_narrow WHERE c1 > %s::bigint', 1000000 * :scale),
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
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_nothing;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_half;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_expr;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_sum_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_residual;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_wide;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_mixed_text;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_count_all;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_four_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_two_columns;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_tree;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_tree_sum;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_int4_bigint;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_either;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_in_list;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_case_sum;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_case_filter;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_group_few;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_group_many;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_group_filter;
SET tessera.enable = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_nothing;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_half;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_expr;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_sum_sparse;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_residual;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_wide;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_mixed_text;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_count_all;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_four_dense;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_two_columns;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_tree;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_tree_sum;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_int4_bigint;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_either;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_in_list;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_case_sum;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_case_filter;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_group_few;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_group_many;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_group_filter;
\o
DEALLOCATE ALL;
