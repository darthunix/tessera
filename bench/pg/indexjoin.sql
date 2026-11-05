-- The indexjoin family: joins of a smaller outer side to bench_lookup
-- (2 M rows per multiplier) through an index of it, the joins the core
-- plans as index nested loops, one lookup an outer row, and the node
-- looks up by batches of keys. 5000 outer rows keep the core on a nested
-- loop: a key unique and in the table's order, shuffled, scattered over
-- the pages, text, date, bigint against int, four rows a key, half of
-- them missing under LEFT; then outer rows spread over every batch, ten
-- rows, keys repeated where the core may take Memoize, 20 000 rows where
-- the planner takes a hash join today, and a key one row in 100 holds;
-- then semi and anti joins, which the core's nested loop stops at the
-- first match: a unique key, half of the keys past the end, and four
-- rows a key with inner clauses that pass none, a fifth, three fifths
-- (in order and shuffled) and all of a key's rows, and a clause over both
-- sides. A ratio below one is the win.
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

-- A key unique and in the table's order.
SELECT pg_temp.measure_pair('pk',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.k WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- The same keys shuffled: each batch's keys spread over the index.
SELECT pg_temp.measure_pair('pk_shuffled',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.ks WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- An index out of the table's order: a page a row.
SELECT pg_temp.measure_pair('scattered',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.s = o.k WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- A text key.
SELECT pg_temp.measure_pair('text_key',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.tk = o.kt WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- A date key.
SELECT pg_temp.measure_pair('date_key',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.dk = o.kd WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- A bigint outer key against the int index.
SELECT pg_temp.measure_pair('cross_type',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.k8 WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- Four rows a key.
SELECT pg_temp.measure_pair('four',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.o4 = o.id WHERE o.id <= %s',
           1250 * :scale),
    :repetitions);
-- LEFT with half of the keys past the end.
SELECT pg_temp.measure_pair('left_miss',
    format('SELECT count(*), count(l.v) FROM bench_lookup_outer o LEFT JOIN bench_lookup l ON l.id = o.km WHERE o.id <= %s',
           5000 * :scale),
    :repetitions);
-- Outer rows spread over every batch, one in 100.
SELECT pg_temp.measure_pair('sparse',
    'SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.k WHERE o.sel = 7',
    :repetitions);
-- Ten outer rows.
SELECT pg_temp.measure_pair('few',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.k WHERE o.id <= %s',
           10 * :scale),
    :repetitions);
-- 1000 keys repeated over 20 000 rows, where the core may take Memoize.
SELECT pg_temp.measure_pair('repeated',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.kr WHERE o.id <= %s',
           20000 * :scale),
    :repetitions);
-- 20 000 rows, where the planner takes a hash join today.
SELECT pg_temp.measure_pair('pk_20k',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.id = o.k WHERE o.id <= %s',
           20000 * :scale),
    :repetitions);
-- A key one row in 100 holds.
SELECT pg_temp.measure_pair('hot_key',
    format('SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON l.h = o.kh WHERE o.id <= %s',
           20000 * :scale),
    :repetitions);
-- EXISTS by a unique key, and NOT EXISTS with half of the keys past the
-- end.
SELECT pg_temp.measure_pair('semi_pk',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE EXISTS (SELECT FROM bench_lookup l WHERE l.id = o.k) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('anti_miss',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE NOT EXISTS (SELECT FROM bench_lookup l WHERE l.id = o.km) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
-- Four rows a key of bench_four, an inner clause on a column whose
-- values the rows of a key take apart: none of a key's rows pass, so that
-- both read all four; a fifth, three fifths, with the keys shuffled too,
-- and all.
SELECT pg_temp.measure_pair('anti_none',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE NOT EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r < 0) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('semi_fifth',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r < 200) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('anti_fifth',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE NOT EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r < 200) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('semi_three_fifths',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r < 600) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('semi_shuffled',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE EXISTS (SELECT FROM bench_four f WHERE f.k = o.ks / 10 AND f.r < 600) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('semi_all',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r >= 0) AND o.id <= %s',
           5000 * :scale),
    :repetitions);
-- A clause over both sides that passes about half the pairs.
SELECT pg_temp.measure_pair('anti_both_sides',
    format('SELECT count(*) FROM bench_lookup_outer o WHERE NOT EXISTS (SELECT FROM bench_four f WHERE f.k = o.id AND f.r > o.sel * 10) AND o.id <= %s',
           5000 * :scale),
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
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['pk', 'pk_shuffled', 'scattered', 'text_key', 'date_key', 'cross_type', 'four', 'left_miss', 'sparse', 'few', 'repeated', 'pk_20k', 'hot_key']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['pk', 'pk_shuffled', 'scattered', 'text_key', 'date_key', 'cross_type', 'four', 'left_miss', 'sparse', 'few', 'repeated', 'pk_20k', 'hot_key']) AS name \gexec
\o
DEALLOCATE ALL;
