-- The spill family: joins and groupings whose table outgrows hash_mem at
-- the data set's multiplier 10, measured at a work_mem of 4, 16 and 64 MB
-- (hash_mem twice that), Tessera on against the core. A ratio below one is
-- the win. Each case runs for seconds, so it is warmed up twice and timed
-- 11 times unless asked otherwise; the plans record the disk each wrote.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 11
\endif
\if :{?warmups}
\else
\set warmups 2
\endif
\if :{?workers}
\else
\set workers 0
\endif
SET jit = off;
SET max_parallel_workers_per_gather = :workers;
SELECT scale FROM bench_scale \gset

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
    work_mem text
);

CREATE FUNCTION pg_temp.measure(test_name text, mode text, statement_name text,
                                warmups integer, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
BEGIN
    PERFORM set_config('tessera.enable', mode, false);
    FOR warmup IN 1..warmups LOOP
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

/*
 * A case at a work_mem: set while each mode plans it (at its first
 * execution, which the warm-up does) and runs it.
 */
CREATE FUNCTION pg_temp.measure_pair(test_name text, sql text, memory text,
                                     warmups integer, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM set_config('work_mem', memory, false);
    INSERT INTO cases VALUES (test_name, memory);
    EXECUTE format('PREPARE on_%I AS %s', test_name, sql);
    EXECUTE format('PREPARE off_%I AS %s', test_name, sql);
    -- run.sh measure with CASES: only the cases it matches are timed.
    IF test_name !~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') THEN
        RETURN;
    END IF;
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, warmups, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, warmups, repetitions);
END
$function$;

/*
 * The cases, at each work_mem:
 * - dim: the fact table's foreign key into the dimension (1 M rows per
 *   multiplier 10), a column of the inner side;
 * - dim_left and dim_anti: the same as a left join and NOT EXISTS;
 * - mixed_text: the dimension's keys against bench_mixed (5 M rows), whose
 *   text column the inner side carries;
 * - group_fk: the fact table grouped by its foreign key, 1 M groups;
 * - group_mixed: bench_mixed grouped by its unique key, 5 M groups.
 */
SELECT pg_temp.measure_pair(format('%s_%s', name, lower(memory)), sql, memory,
                            :warmups, :repetitions)
FROM unnest(ARRAY['4MB', '16MB', '64MB']) WITH ORDINALITY AS m(memory, m_order),
     unnest(ARRAY['dim', 'dim_left', 'dim_anti', 'mixed_text', 'group_fk', 'group_mixed'],
            ARRAY['SELECT sum(d.d1) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id',
                  'SELECT count(*), count(d.d1) FROM bench_fact f LEFT JOIN bench_dim d ON f.fk = d.id',
                  'SELECT count(*) FROM bench_fact f WHERE NOT EXISTS (SELECT 1 FROM bench_dim d WHERE d.id = f.fk)',
                  'SELECT count(m.e) FROM bench_fact f JOIN bench_mixed m ON f.fk = m.a',
                  'SELECT count(*), sum(s) FROM (SELECT fk, sum(f1) AS s FROM bench_fact GROUP BY fk) AS g',
                  'SELECT count(*), sum(s) FROM (SELECT a, sum(d) AS s FROM bench_mixed GROUP BY a) AS g'])
         WITH ORDINALITY AS c(name, sql, c_order)
ORDER BY m_order, c_order;
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

-- The cached plans of both modes at their work_mem, with the disk each wrote.
\o plans.txt
SELECT format('SET work_mem = %L', work_mem),
       'SET tessera.enable = on',
       format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', test),
       'SET tessera.enable = off',
       format('EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', test)
FROM cases \gexec
\o
RESET work_mem;
DEALLOCATE ALL;
