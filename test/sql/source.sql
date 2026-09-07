CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set source_test :libdir '/tessera_source_test' :dlsuffix
LOAD :'source_test';

CREATE FUNCTION tessera_test_source_registry()
RETURNS boolean
AS :'source_test', 'tessera_test_source_registry'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_sizes()
RETURNS boolean
AS :'source_test', 'tessera_test_source_sizes'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_source(integer)
RETURNS void
AS :'source_test', 'tessera_test_invalid_source'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_duplicate_source()
RETURNS void
AS :'source_test', 'tessera_test_duplicate_source'
LANGUAGE C STRICT;

SELECT tessera_test_source_registry() AS source_registry \gset
\echo :source_registry
SELECT tessera_test_source_sizes() AS source_sizes \gset
\echo :source_sizes

\set VERBOSITY terse
SELECT tessera_test_invalid_source(0);
SELECT tessera_test_invalid_source(1);
SELECT tessera_test_invalid_source(2);
SELECT tessera_test_invalid_source(3);
SELECT tessera_test_invalid_source(4);
SELECT tessera_test_duplicate_source();
\set VERBOSITY default

DROP FUNCTION tessera_test_duplicate_source();
DROP FUNCTION tessera_test_invalid_source(integer);
DROP FUNCTION tessera_test_source_sizes();
DROP FUNCTION tessera_test_source_registry();
DROP EXTENSION tessera;
