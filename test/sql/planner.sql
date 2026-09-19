CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set planner_test :libdir '/tessera_planner_test' :dlsuffix
LOAD :'planner_test';

CREATE FUNCTION tessera_test_planner_paths()
RETURNS boolean
AS :'planner_test', 'tessera_test_planner_paths'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_planner_plans()
RETURNS boolean
AS :'planner_test', 'tessera_test_planner_plans'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_planner_errors(integer)
RETURNS void
AS :'planner_test', 'tessera_test_planner_errors'
LANGUAGE C STRICT;

SELECT tessera_test_planner_paths() AS paths \gset
\echo :paths
SELECT tessera_test_planner_plans() AS plans \gset
\echo :plans

-- The module's hook wraps the sequential scan of every planner_* table in
-- a forwarding node whose plan is built by the helpers.
CREATE TABLE planner_t (a int, b text);
INSERT INTO planner_t SELECT i, 'row' || i FROM generate_series(1, 5) AS i;
CREATE TABLE other_t (a int);
INSERT INTO other_t VALUES (1);
EXPLAIN (COSTS OFF) SELECT a, b FROM planner_t WHERE a > 1;
SELECT a, b FROM planner_t WHERE a > 1;
-- The projection PostgreSQL installs after PlanCustomPath changes the layout.
EXPLAIN (COSTS OFF) SELECT a + 1 AS next FROM planner_t;
SELECT a + 1 AS next FROM planner_t;
EXPLAIN (COSTS OFF) SELECT a FROM other_t;
-- A cached plan copies the private data.
PREPARE counted AS SELECT count(*) FROM planner_t WHERE a > $1;
EXECUTE counted(1);
EXECUTE counted(2);
EXECUTE counted(3);
EXECUTE counted(4);
EXECUTE counted(5);
EXECUTE counted(0);
EXPLAIN (COSTS OFF) EXECUTE counted(0);
DEALLOCATE counted;
-- A parallel worker reads the plan back from its text form.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT a, b FROM planner_t WHERE a > 1;
SELECT a, b FROM planner_t WHERE a > 1;
RESET debug_parallel_query;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a, b FROM planner_t WHERE a > 1;
RESET tessera.enable;
DROP TABLE planner_t, other_t;

\set VERBOSITY terse
SELECT tessera_test_planner_errors(0);
SELECT tessera_test_planner_errors(1);
SELECT tessera_test_planner_errors(2);
SELECT tessera_test_planner_errors(3);
SELECT tessera_test_planner_errors(4);
SELECT tessera_test_planner_errors(5);
SELECT tessera_test_planner_errors(6);
SELECT tessera_test_planner_errors(7);
SELECT tessera_test_planner_errors(8);
SELECT tessera_test_planner_errors(9);
SELECT tessera_test_planner_errors(10);
SELECT tessera_test_planner_errors(11);
SELECT tessera_test_planner_errors(12);
SELECT tessera_test_planner_errors(13);
SELECT tessera_test_planner_errors(14);
SELECT tessera_test_planner_errors(15);
SELECT tessera_test_planner_errors(16);
SELECT tessera_test_planner_errors(17);
SELECT tessera_test_planner_errors(18);
SELECT tessera_test_planner_errors(19);
SELECT tessera_test_planner_errors(20);
SELECT tessera_test_planner_errors(21);
SELECT tessera_test_planner_errors(22);
SELECT tessera_test_planner_errors(23);
\set VERBOSITY default

DROP FUNCTION tessera_test_planner_errors(integer);
DROP FUNCTION tessera_test_planner_plans();
DROP FUNCTION tessera_test_planner_paths();
DROP EXTENSION tessera;
