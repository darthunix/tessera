CREATE EXTENSION tessera;
LOAD 'tessera_kernels';

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set expr_test :libdir '/tessera_expr_test' :dlsuffix
LOAD :'expr_test';

CREATE FUNCTION tessera_test_expr_supports()
RETURNS boolean
AS :'expr_test', 'tessera_test_expr_supports'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_expr_values()
RETURNS boolean
AS :'expr_test', 'tessera_test_expr_values'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_expr_filters()
RETURNS boolean
AS :'expr_test', 'tessera_test_expr_filters'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_expr_errors(integer)
RETURNS void
AS :'expr_test', 'tessera_test_expr_errors'
LANGUAGE C STRICT;

SELECT tessera_test_expr_supports() AS supports \gset
\echo :supports
SELECT tessera_test_expr_values() AS values \gset
\echo :values
SELECT tessera_test_expr_filters() AS filters \gset
\echo :filters

\set VERBOSITY terse
SELECT tessera_test_expr_errors(0);
SELECT tessera_test_expr_errors(1);
SELECT tessera_test_expr_errors(2);
SELECT tessera_test_expr_errors(3);
SELECT tessera_test_expr_errors(4);
SELECT tessera_test_expr_errors(5);
SELECT tessera_test_expr_errors(6);
\set VERBOSITY default

DROP FUNCTION tessera_test_expr_errors(integer);
DROP FUNCTION tessera_test_expr_filters();
DROP FUNCTION tessera_test_expr_values();
DROP FUNCTION tessera_test_expr_supports();
DROP EXTENSION tessera;
