-- One reading of a case, the same for every family that times its
-- cases, so that what a reading is lives in one place. A family includes
-- this file and records what these functions return in the table
-- `timings` it creates: a time in milliseconds or, when run.sh measures
-- with METRIC=instructions (the setting bench.metric), the instructions
-- the processor spent, which pg_pmu counts in EXPLAIN ANALYZE, parallel
-- workers included.

-- The readings of a run, one row a reading of a case in a mode, which the
-- family writes to timings.csv. A count of instructions also gives its
-- row, by the trigger below, what the EXPLAIN that counted it showed:
-- `disk`, the kB its nodes report written to disk (Disk Usage), and
-- `plan`, its shape. A time leaves them empty.
CREATE TEMP TABLE timings
(
    test text,
    mode text,
    run integer,
    reading numeric,
    disk numeric,
    plan text
);

-- Whether the run counts instructions instead of timing.
CREATE FUNCTION pg_temp.counting()
RETURNS boolean
LANGUAGE sql
AS $function$
SELECT current_setting('bench.metric', true) IS NOT DISTINCT FROM 'instructions'
$function$;

-- The shape of a plan node and of the nodes under it, from EXPLAIN's
-- JSON: each node's type, or its provider for a custom scan, with its
-- strategy, partial mode, join type and planned workers, the relation it
-- scans and the index it uses, such as
-- TessHashJoin[Semi](TessHeapScan on fact; TessHeapScan on dim). A change of
-- costs that swaps a join's sides, takes another index or another number
-- of workers, or puts a core node for a batch node, changes the shape.
-- What the shape leaves out, such as a filter or a sort's keys, is not
-- compared.
CREATE FUNCTION pg_temp.shape(node jsonb)
RETURNS text
LANGUAGE plpgsql
AS $function$
DECLARE
    children text;
    traits text;
BEGIN
    SELECT string_agg(pg_temp.shape(child), '; ' ORDER BY position)
      INTO children
      FROM jsonb_array_elements(node -> 'Plans')
           WITH ORDINALITY AS c(child, position);
    traits := concat_ws('/', node ->> 'Strategy', node ->> 'Partial Mode',
                        node ->> 'Join Type',
                        (node ->> 'Workers Planned') || ' workers');
    RETURN coalesce(node ->> 'Custom Plan Provider', node ->> 'Node Type')
           || coalesce('[' || nullif(traits, '') || ']', '')
           || coalesce(' on ' || (node ->> 'Relation Name'), '')
           || coalesce(' using ' || (node ->> 'Index Name'), '')
           || coalesce('(' || children || ')', '');
END
$function$;

-- A count pg_pmu prints for a statement, by its name in EXPLAIN's JSON,
-- such as 'Execution Instructions'. The EXPLAIN that counts has no timing
-- and no buffers, whose instrumentation would add instructions of its own
-- to every node; the count of rows per node remains in it, the same in
-- both revisions of an A/B while their plans are the same. A count that
-- is missing is an error, never an empty reading. The disk and the shape
-- of its plan stay in the session's settings bench.disk and bench.plan,
-- for the row of `timings` that records the count. A shape that the CSV
-- of timings.csv would quote, or none, is an error too: tessera-ab reads
-- the file split at its commas.
CREATE FUNCTION pg_temp.counted(sql text, name text)
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    explained jsonb;
    count numeric;
    shape text;
BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, PMU, TIMING off, BUFFERS off, SUMMARY off, '
            'FORMAT JSON) ' || sql
       INTO explained;
    count := (explained -> 0 ->> name)::numeric;
    IF count IS NULL THEN
        RAISE EXCEPTION 'pg_pmu printed no "%" for %', name, sql;
    END IF;
    PERFORM set_config('bench.disk',
        (SELECT coalesce(sum((kb #>> '{}')::numeric), 0)
           FROM jsonb_path_query(explained, 'strict $.**."Disk Usage"') AS kb)::text,
        false);
    shape := pg_temp.shape(explained -> 0 -> 'Plan');
    IF shape IS NULL OR shape ~ '[,"]' THEN
        RAISE EXCEPTION 'the plan of % has no shape for timings.csv: %',
            sql, coalesce(shape, 'none');
    END IF;
    PERFORM set_config('bench.plan', shape, false);
    RETURN count;
END
$function$;

-- The row of a count takes the disk and the plan of the EXPLAIN that
-- counted it: the last one, since a family computes the reading in the
-- INSERT itself, or just before it. A row inserted before any count
-- fails, as bench.disk is then unset.
CREATE FUNCTION pg_temp.counted_row()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    IF pg_temp.counting() THEN
        NEW.disk := current_setting('bench.disk')::numeric;
        NEW.plan := current_setting('bench.plan');
    END IF;
    RETURN NEW;
END
$function$;

CREATE TRIGGER counted_row BEFORE INSERT ON timings
    FOR EACH ROW EXECUTE FUNCTION pg_temp.counted_row();

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
