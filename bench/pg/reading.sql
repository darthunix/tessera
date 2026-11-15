-- One reading of a case, the same for every family that times its
-- cases, so that what a reading is lives in one place. A family includes
-- this file and records what these functions return.

-- The time of a statement in milliseconds. `command` is what the family
-- runs, such as EXECUTE of a prepared statement.
CREATE FUNCTION pg_temp.reading(command text)
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz := clock_timestamp();
BEGIN
    EXECUTE command;
    RETURN 1000 * extract(epoch FROM clock_timestamp() - started_at);
END
$function$;

-- The time of a query's planning alone in milliseconds: the mean of
-- `plans` EXPLAINs with `options`, such as '(COSTS OFF) ', which plan the
-- query and run nothing.
CREATE FUNCTION pg_temp.plan_reading(sql text, plans integer,
                                     options text DEFAULT '')
RETURNS numeric
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz := clock_timestamp();
BEGIN
    FOR plan IN 1..plans LOOP
        EXECUTE 'EXPLAIN ' || options || sql;
    END LOOP;
    RETURN 1000 * extract(epoch FROM clock_timestamp() - started_at) / plans;
END
$function$;
