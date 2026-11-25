-- EXPLAIN in the same words on every core Tessera builds against,
-- PostgreSQL 18, 19 and master. The cores name some parts of a plan
-- differently, and the names are no part of what Tessera plans:
-- - 19 names a subplan by its kind and number (SubPlan expr_1,
--   exists_to_any_2), 18 by its number alone (SubPlan 1), and they
--   number in other orders: both become SubPlan and InitPlan;
-- - 19 names the branches of a set operation unnamed_subquery,
--   unnamed_subquery_1 and so on, 18 "*SELECT* 1", "*SELECT* 2", and
--   they number nested ones in other orders; 19 names the query of an
--   INSERT ... SELECT unnamed_subquery too, 18 "*SELECT*": all become
--   subquery;
-- - 19 says which scan a Result replaces (Replaces: Scan on t), 18 does
--   not: the line goes;
-- - master since 2026-11-10 says what a Function Scan's store of rows
--   took (Storage: Memory  Maximum Storage: 17kB), 19 and 18 do not: the
--   line goes, and so would the same line of a Material node, which no
--   suite prints.
-- A plan that prints a subplan, a branch of a set operation, a Result
-- in place of a scan or, under ANALYZE, a Function Scan goes through
-- explain_neutral, or a helper of the suite that passes its lines
-- through core_neutral. A suite includes
-- this file with ECHO none, so that the text is not repeated in each
-- expected output, and drops the functions at its end. The path comes
-- from pg_regress (PG_ABS_SRCDIR): a suite run by psql alone does not
-- find the file.
-- Names the cores may differ in that no suite prints yet are left as
-- they are, such as two CTEs of one name in two scopes (19: x and x_1).

-- A line of EXPLAIN in those words; NULL for a line to leave out.
CREATE FUNCTION core_neutral(line text) RETURNS text
LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE WHEN line ~ '^\s*(Replaces: |Storage: \S+\s+Maximum Storage: )' THEN NULL
        ELSE regexp_replace(regexp_replace(line,
            '\m(SubPlan|InitPlan) ([a-z_]+_)?\d+', '\1', 'g'),
            'unnamed_subquery(_\d+)*|"\*SELECT\*( \d+(_\d+)*)?"', 'subquery', 'g')
    END
$$;

-- EXPLAIN with the options given, line by line through core_neutral.
CREATE FUNCTION explain_neutral(query text, options text DEFAULT 'COSTS OFF')
RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE format('EXPLAIN (%s) %s', options, query) LOOP
        line := core_neutral(line);
        CONTINUE WHEN line IS NULL;
        RETURN NEXT line;
    END LOOP;
END $$;
