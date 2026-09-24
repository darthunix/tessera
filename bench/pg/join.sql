-- The join family: equi-joins over one integer key. With Tessera on, a
-- TessHashJoin builds the inner side into the hash table and probes it
-- with batches of the outer side, under TessAgg where the query
-- aggregates; with it off, the core's Hash Join. A ratio below one is the
-- win; plan_time measures planning alone, where the join hook must cost
-- next to nothing.
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
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

/*
 * Planning alone: EXPLAIN without ANALYZE plans the statement and runs
 * nothing, so its time is the planner's, hooks included. One plan takes
 * tens of microseconds, so a sample is the mean over 100 of them.
 */
CREATE FUNCTION pg_temp.measure_plan(test_name text, sql text,
                                     repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
    mode text;
BEGIN
    FOREACH mode IN ARRAY ARRAY['on', 'off'] LOOP
        PERFORM set_config('tessera.enable', mode, false);
        FOR warmup IN 1..5 LOOP
            EXECUTE 'EXPLAIN (COSTS OFF) ' || sql;
        END LOOP;
        FOR sample IN 1..repetitions LOOP
            started_at := clock_timestamp();
            FOR plan IN 1..100 LOOP
                EXECUTE 'EXPLAIN (COSTS OFF) ' || sql;
            END LOOP;
            INSERT INTO timings
            VALUES (test_name, mode, sample,
                    10 * extract(epoch FROM clock_timestamp() - started_at));
        END LOOP;
    END LOOP;
END
$function$;

-- A foreign key into the dimension: the probe and its mask alone, a column
-- of the inner side through the table's payload, one of the outer side.
SELECT pg_temp.measure_pair('fk_count',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id',
    :repetitions);
SELECT pg_temp.measure_pair('fk_inner_col',
    'SELECT sum(d.d1) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id',
    :repetitions);
SELECT pg_temp.measure_pair('fk_outer_col',
    'SELECT sum(f.f1) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id',
    :repetitions);
-- int8 keys past the int4 range, and an int4 key against an int8 one.
SELECT pg_temp.measure_pair('int8',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_big = d.big',
    :repetitions);
SELECT pg_temp.measure_pair('mixed',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id8',
    :repetitions);
-- Misses: a dimension filtered to one key in a hundred, then no match.
SELECT pg_temp.measure_pair('selective',
    format('SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE d.d1 <= %s',
           1000 * :scale),
    :repetitions);
SELECT pg_temp.measure_pair('miss',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_miss = d.id',
    :repetitions);
-- Four records per key on the build side.
SELECT pg_temp.measure_pair('dup',
    'SELECT sum(u.v) FROM bench_fact f JOIN bench_dup u ON f.fk = u.k',
    :repetitions);
-- Two keys, and a residual clause over columns of both sides.
SELECT pg_temp.measure_pair('two_keys',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id AND f.fk8 = d.id8',
    :repetitions);
SELECT pg_temp.measure_pair('residual',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id AND f.f1 > d.d1 * 10',
    :repetitions);
-- A row-wise parent: a top-N sort reads every joined row.
SELECT pg_temp.measure_pair('rows_parent',
    'SELECT f.f1, d.d1 FROM bench_fact f JOIN bench_dim d ON f.fk = d.id ORDER BY f.f1 DESC LIMIT 10',
    :repetitions);
-- A join over a join.
SELECT pg_temp.measure_pair('chain',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id JOIN bench_dup u ON d.id = u.k',
    :repetitions);
-- The planner over four relations, where the hook sees every join order.
SELECT pg_temp.measure_plan('plan_time',
    'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id JOIN bench_dup u ON d.id = u.k JOIN bench_dim e ON u.v = e.id',
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
FROM unnest(ARRAY['fk_count', 'fk_inner_col', 'fk_outer_col', 'int8', 'mixed',
                  'selective', 'miss', 'dup', 'two_keys', 'residual', 'rows_parent',
                  'chain']) AS name \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['fk_count', 'fk_inner_col', 'fk_outer_col', 'int8', 'mixed',
                  'selective', 'miss', 'dup', 'two_keys', 'residual', 'rows_parent',
                  'chain']) AS name \gexec
\o
DEALLOCATE ALL;
