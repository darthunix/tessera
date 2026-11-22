-- The functions a counting run of cargo clickbench reads a query through,
-- inside pg_pmu's EXPLAIN (ANALYZE, PMU), made before the run. pg_duckdb
-- runs a query under EXPLAIN only when it prints the plan, after the
-- window pg_pmu counts; read through a cursor of PL/pgSQL, whose portal
-- is a SELECT, the query runs in the executor, inside the window. Such a
-- cursor takes no parallel workers, so Tessera's queries go through an
-- EXECUTE of PL/pgSQL, which does. Both spare the query the counting of
-- rows of EXPLAIN ANALYZE; bench_exec keeps the rows in SPI's table,
-- bench_rows reads them one by one, and neither sends them to the client.
-- What they count holds the query's parsing and planning, as a timed try
-- does.
CREATE OR REPLACE FUNCTION public.bench_rows(q text) RETURNS bigint
LANGUAGE plpgsql AS $f$
DECLARE
    r record;
    n bigint := 0;
BEGIN
    FOR r IN EXECUTE q LOOP
        n := n + 1;
    END LOOP;
    RETURN n;
END
$f$;

CREATE OR REPLACE FUNCTION public.bench_exec(q text) RETURNS bigint
LANGUAGE plpgsql AS $f$
DECLARE
    n bigint;
BEGIN
    EXECUTE q;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END
$f$;
