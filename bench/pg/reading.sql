-- One reading of a case, the same for every family that times its
-- cases, so that what a reading is lives in one place. A family includes
-- this file and records what these functions return in the table
-- `timings` it creates: a time in milliseconds or, when run.sh measures
-- with METRIC=instructions (the setting bench.metric), the instructions
-- the processor spent, which pg_pmu counts in EXPLAIN ANALYZE, parallel
-- workers included.

-- The readings of a run, one row a reading of a case in a mode, which the
-- family writes to timings.csv.
CREATE TEMP TABLE timings
(
    test text,
    mode text,
    run integer,
    reading numeric
);

-- Whether the run counts instructions instead of timing.
CREATE FUNCTION pg_temp.counting()
RETURNS boolean
LANGUAGE sql
AS $function$
SELECT current_setting('bench.metric', true) IS NOT DISTINCT FROM 'instructions'
$function$;

-- A count pg_pmu prints for a statement, by its name in EXPLAIN's JSON,
-- such as 'Execution Instructions'. The EXPLAIN that counts has no timing
-- and no buffers, whose instrumentation would add instructions of its own
-- to every node; the count of rows per node remains in it, the same in
-- both revisions of an A/B. A count that is missing is an error, never
-- an empty reading.
CREATE FUNCTION pg_temp.counted(sql text, name text)
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    plan json;
    count numeric;
BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, PMU, TIMING off, BUFFERS off, SUMMARY off, '
            'FORMAT JSON) ' || sql
       INTO plan;
    count := (plan -> 0 ->> name)::numeric;
    IF count IS NULL THEN
        RAISE EXCEPTION 'pg_pmu printed no "%" for %', name, sql;
    END IF;
    RETURN count;
END
$function$;

-- A statement's reading: its time in milliseconds, or the instructions of
-- its execution. `command` is what the family runs, such as EXECUTE of a
-- prepared statement.
CREATE FUNCTION pg_temp.reading(command text)
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
BEGIN
    IF pg_temp.counting() THEN
        RETURN pg_temp.counted(command, 'Execution Instructions');
    END IF;
    started_at := clock_timestamp();
    EXECUTE command;
    RETURN 1000 * extract(epoch FROM clock_timestamp() - started_at);
END
$function$;

-- The reading of a query's planning alone: the mean time in milliseconds
-- of `plans` EXPLAINs with `options`, such as '(COSTS OFF) ', which plan
-- the query and run nothing; or the instructions of one planning, which
-- pg_pmu counts only under ANALYZE, so that the query then runs as well.
CREATE FUNCTION pg_temp.plan_reading(sql text, plans integer,
                                     options text DEFAULT '')
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
BEGIN
    IF pg_temp.counting() THEN
        RETURN pg_temp.counted(sql, 'Planning Instructions');
    END IF;
    started_at := clock_timestamp();
    FOR plan IN 1..plans LOOP
        EXECUTE 'EXPLAIN ' || options || sql;
    END LOOP;
    RETURN 1000 * extract(epoch FROM clock_timestamp() - started_at) / plans;
END
$function$;
