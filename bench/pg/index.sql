-- The index family: reads through indexes of bench_idx (2 M rows per
-- multiplier): a bitmap of the scattered k, whose pages hold few of the
-- rows each at 1 % and more at 5 and 15 %, BitmapAnd and BitmapOr of k and
-- w, and rows of a bitmap returned to a limit; index scans of the ordered
-- id; BRIN of the day d. The core reads the pages of the bitmap, or the
-- index, in both modes; with Tessera the rows come in batches, a filter
-- above rechecking every clause. A ratio below one is the win.
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

-- Bitmaps of k at 1, 5 and 15 % of the rows, of k and w both, of either.
SELECT pg_temp.measure_pair('bm_sparse',
    format('SELECT count(*), sum(w) FROM bench_idx WHERE k < %s', :rows / 100), :repetitions);
SELECT pg_temp.measure_pair('bm_mid',
    format('SELECT count(*), sum(w) FROM bench_idx WHERE k < %s', :rows / 20), :repetitions);
SELECT pg_temp.measure_pair('bm_dense',
    format('SELECT count(*), sum(w) FROM bench_idx WHERE k < %s', :rows * 15 / 100), :repetitions);
SELECT pg_temp.measure_pair('bm_both',
    format('SELECT count(*), sum(id) FROM bench_idx WHERE k < %s AND w = 5', :rows / 5), :repetitions);
SELECT pg_temp.measure_pair('bm_either',
    format('SELECT count(*), sum(id) FROM bench_idx WHERE k < %s OR w = 5', :rows / 100), :repetitions);
-- The rows of a bitmap of 5 %, skipped by a limit's offset.
SELECT pg_temp.measure_pair('bm_rows',
    format('SELECT id, t FROM bench_idx WHERE k < %s OFFSET %s', :rows / 20, :rows), :repetitions);
-- Index scans of the ordered id, which the core reads a row at a time in
-- both modes: 10 and 3 % of the rows aggregated, 5 % returned to a limit's
-- offset, a range in the index's order, a range with a clause on another
-- column.
SELECT pg_temp.measure_pair('ix_range',
    format('SELECT count(*), sum(w) FROM bench_idx WHERE id < %s', :rows / 10), :repetitions);
SELECT pg_temp.measure_pair('ix_short',
    format('SELECT count(*), sum(w) FROM bench_idx WHERE id < %s', :rows * 3 / 100), :repetitions);
SELECT pg_temp.measure_pair('ix_rows',
    format('SELECT id, t FROM bench_idx WHERE id < %s OFFSET %s', :rows / 20, :rows), :repetitions);
SELECT pg_temp.measure_pair('ix_order',
    format('SELECT id, t FROM bench_idx WHERE id BETWEEN %s AND %s ORDER BY id OFFSET %s',
           :rows / 4, :rows / 4 + :rows * 3 / 100, :rows), :repetitions);
SELECT pg_temp.measure_pair('ix_filter',
    format('SELECT count(*), sum(k) FROM bench_idx WHERE id < %s AND w < 50', :rows / 20), :repetitions);
-- BRIN of the day, in the order of the rows: its bitmap names whole pages,
-- every row of which the filter rechecks; a week, a month and five months
-- of days aggregated, and 20 days returned to a limit's offset.
SELECT pg_temp.measure_pair('brin_week',
    $q$SELECT count(*), sum(w) FROM bench_idx WHERE d >= '2000-05-01' AND d < '2000-05-08'$q$, :repetitions);
SELECT pg_temp.measure_pair('brin_month',
    $q$SELECT count(*), sum(w) FROM bench_idx WHERE d BETWEEN '2000-02-01' AND '2000-03-01'$q$, :repetitions);
SELECT pg_temp.measure_pair('brin_months',
    $q$SELECT count(*), sum(w) FROM bench_idx WHERE d BETWEEN '2000-02-01' AND '2000-06-30'$q$, :repetitions);
SELECT pg_temp.measure_pair('brin_rows',
    format($q$SELECT id, t FROM bench_idx WHERE d BETWEEN '2000-05-01' AND '2000-05-20' OFFSET %s$q$, :rows),
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
FROM unnest(ARRAY['bm_sparse', 'bm_mid', 'bm_dense', 'bm_both', 'bm_either', 'bm_rows',
                   'ix_range', 'ix_short', 'ix_rows', 'ix_order', 'ix_filter',
                   'brin_week', 'brin_month', 'brin_months', 'brin_rows']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['bm_sparse', 'bm_mid', 'bm_dense', 'bm_both', 'bm_either', 'bm_rows',
                   'ix_range', 'ix_short', 'ix_rows', 'ix_order', 'ix_filter',
                   'brin_week', 'brin_month', 'brin_months', 'brin_rows']) AS name \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
