CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set function_test :libdir '/tessera_function_test' :dlsuffix
LOAD :'function_test';

CREATE FUNCTION tessera_test_function_registry()
RETURNS boolean
AS :'function_test', 'tessera_test_function_registry'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_function_sizes()
RETURNS boolean
AS :'function_test', 'tessera_test_function_sizes'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_function(integer)
RETURNS void
AS :'function_test', 'tessera_test_invalid_function'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_duplicate_function()
RETURNS void
AS :'function_test', 'tessera_test_duplicate_function'
LANGUAGE C STRICT;

SELECT tessera_test_function_registry() AS function_registry \gset
\echo :function_registry
SELECT tessera_test_function_sizes() AS function_sizes \gset
\echo :function_sizes

\set VERBOSITY terse
SELECT tessera_test_invalid_function(0);
SELECT tessera_test_invalid_function(1);
SELECT tessera_test_invalid_function(2);
SELECT tessera_test_invalid_function(3);
SELECT tessera_test_invalid_function(4);
SELECT tessera_test_invalid_function(5);
SELECT tessera_test_invalid_function(6);
SELECT tessera_test_invalid_function(7);
SELECT tessera_test_duplicate_function();
\set VERBOSITY default

DROP FUNCTION tessera_test_duplicate_function();
DROP FUNCTION tessera_test_invalid_function(integer);
DROP FUNCTION tessera_test_function_sizes();
DROP FUNCTION tessera_test_function_registry();
DROP EXTENSION tessera;
