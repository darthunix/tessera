\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set builder_test :libdir '/tessera_builder_test' :dlsuffix
LOAD :'builder_test';

CREATE FUNCTION tessera_test_builder_byval()
RETURNS boolean
AS :'builder_test', 'tessera_test_builder_byval'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_builder_columns()
RETURNS boolean
AS :'builder_test', 'tessera_test_builder_columns'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_builder_errors(integer)
RETURNS void
AS :'builder_test', 'tessera_test_builder_errors'
LANGUAGE C STRICT;

SELECT tessera_test_builder_byval() AS byval \gset
\echo :byval
SELECT tessera_test_builder_columns() AS columns \gset
\echo :columns

\set VERBOSITY terse
SELECT tessera_test_builder_errors(0);
SELECT tessera_test_builder_errors(1);
SELECT tessera_test_builder_errors(2);
SELECT tessera_test_builder_errors(3);
SELECT tessera_test_builder_errors(4);
SELECT tessera_test_builder_errors(5);
SELECT tessera_test_builder_errors(6);
SELECT tessera_test_builder_errors(7);
SELECT tessera_test_builder_errors(8);
\set VERBOSITY default

DROP FUNCTION tessera_test_builder_errors(integer);
DROP FUNCTION tessera_test_builder_columns();
DROP FUNCTION tessera_test_builder_byval();
