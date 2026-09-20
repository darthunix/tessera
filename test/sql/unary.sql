CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set unary_test :libdir '/tessera_unary_test' :dlsuffix
LOAD :'unary_test';

CREATE FUNCTION tessera_test_unary_batches()
RETURNS boolean
AS :'unary_test', 'tessera_test_unary_batches'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_unary_rows()
RETURNS boolean
AS :'unary_test', 'tessera_test_unary_rows'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_unary_bound()
RETURNS boolean
AS :'unary_test', 'tessera_test_unary_bound'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_unary_errors(integer)
RETURNS void
AS :'unary_test', 'tessera_test_unary_errors'
LANGUAGE C STRICT;

SELECT tessera_test_unary_batches() AS batches \gset
\echo :batches
SELECT tessera_test_unary_rows() AS rows \gset
\echo :rows
SELECT tessera_test_unary_bound() AS bound \gset
\echo :bound

\set VERBOSITY terse
SELECT tessera_test_unary_errors(0);
SELECT tessera_test_unary_errors(1);
SELECT tessera_test_unary_errors(2);
SELECT tessera_test_unary_errors(3);
SELECT tessera_test_unary_errors(4);
SELECT tessera_test_unary_errors(5);
SELECT tessera_test_unary_errors(6);
SELECT tessera_test_unary_errors(7);
\set VERBOSITY default

DROP FUNCTION tessera_test_unary_errors(integer);
DROP FUNCTION tessera_test_unary_bound();
DROP FUNCTION tessera_test_unary_rows();
DROP FUNCTION tessera_test_unary_batches();
DROP EXTENSION tessera;
