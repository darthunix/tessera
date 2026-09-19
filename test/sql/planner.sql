CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set planner_test :libdir '/tessera_planner_test' :dlsuffix
LOAD :'planner_test';

CREATE FUNCTION tessera_test_planner_paths()
RETURNS boolean
AS :'planner_test', 'tessera_test_planner_paths'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_planner_errors(integer)
RETURNS void
AS :'planner_test', 'tessera_test_planner_errors'
LANGUAGE C STRICT;

SELECT tessera_test_planner_paths() AS paths \gset
\echo :paths

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
\set VERBOSITY default

DROP FUNCTION tessera_test_planner_errors(integer);
DROP FUNCTION tessera_test_planner_paths();
DROP EXTENSION tessera;
