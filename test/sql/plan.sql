\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set plan_test :libdir '/tessera_plan_test' :dlsuffix
LOAD :'plan_test';

CREATE FUNCTION tessera_test_plan_roundtrip()
RETURNS boolean
AS :'plan_test', 'tessera_test_plan_roundtrip'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_plan_errors(integer)
RETURNS void
AS :'plan_test', 'tessera_test_plan_errors'
LANGUAGE C STRICT;

SELECT tessera_test_plan_roundtrip() AS roundtrip \gset
\echo :roundtrip

\set VERBOSITY terse
SELECT tessera_test_plan_errors(0);
SELECT tessera_test_plan_errors(1);
SELECT tessera_test_plan_errors(2);
SELECT tessera_test_plan_errors(3);
SELECT tessera_test_plan_errors(4);
SELECT tessera_test_plan_errors(5);
SELECT tessera_test_plan_errors(6);
SELECT tessera_test_plan_errors(7);
SELECT tessera_test_plan_errors(8);
SELECT tessera_test_plan_errors(9);
SELECT tessera_test_plan_errors(10);
SELECT tessera_test_plan_errors(11);
SELECT tessera_test_plan_errors(12);
SELECT tessera_test_plan_errors(13);
SELECT tessera_test_plan_errors(14);
SELECT tessera_test_plan_errors(15);
SELECT tessera_test_plan_errors(16);
SELECT tessera_test_plan_errors(17);
SELECT tessera_test_plan_errors(18);
SELECT tessera_test_plan_errors(19);
\set VERBOSITY default

DROP FUNCTION tessera_test_plan_errors(integer);
DROP FUNCTION tessera_test_plan_roundtrip();
