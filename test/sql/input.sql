CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set input_test :libdir '/tessera_input_test' :dlsuffix
LOAD :'input_test';

CREATE FUNCTION tessera_test_input_batches()
RETURNS boolean
AS :'input_test', 'tessera_test_input_batches'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_input_forwarding()
RETURNS boolean
AS :'input_test', 'tessera_test_input_forwarding'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_input_errors(integer)
RETURNS void
AS :'input_test', 'tessera_test_input_errors'
LANGUAGE C STRICT;

SELECT tessera_test_input_batches() AS batches \gset
\echo :batches
SELECT tessera_test_input_forwarding() AS forwarding \gset
\echo :forwarding

\set VERBOSITY terse
SELECT tessera_test_input_errors(0);
SELECT tessera_test_input_errors(1);
SELECT tessera_test_input_errors(2);
SELECT tessera_test_input_errors(3);
SELECT tessera_test_input_errors(4);
SELECT tessera_test_input_errors(5);
\set VERBOSITY default

DROP FUNCTION tessera_test_input_errors(integer);
DROP FUNCTION tessera_test_input_forwarding();
DROP FUNCTION tessera_test_input_batches();
DROP EXTENSION tessera;
