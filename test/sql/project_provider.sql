CREATE EXTENSION tessera;
LOAD 'tessera_kernels';

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set project_test :libdir '/tessera_project_test' :dlsuffix
LOAD :'project_test';

CREATE FUNCTION tessera_test_project_columns()
RETURNS boolean
AS :'project_test', 'tessera_test_project_columns'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_project_errors(integer)
RETURNS void
AS :'project_test', 'tessera_test_project_errors'
LANGUAGE C STRICT;

SELECT tessera_test_project_columns() AS columns \gset
\echo :columns

\set VERBOSITY terse
SELECT tessera_test_project_errors(0);
SELECT tessera_test_project_errors(1);
SELECT tessera_test_project_errors(2);
SELECT tessera_test_project_errors(3);
SELECT tessera_test_project_errors(4);
SELECT tessera_test_project_errors(5);
\set VERBOSITY default

DROP FUNCTION tessera_test_project_errors(integer);
DROP FUNCTION tessera_test_project_columns();
DROP EXTENSION tessera;
