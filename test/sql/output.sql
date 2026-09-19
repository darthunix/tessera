CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set output_test :libdir '/tessera_output_test' :dlsuffix
LOAD :'output_test';

CREATE FUNCTION tessera_test_output_rows()
RETURNS boolean
AS :'output_test', 'tessera_test_output_rows'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_output_batch()
RETURNS boolean
AS :'output_test', 'tessera_test_output_batch'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_output_layout()
RETURNS boolean
AS :'output_test', 'tessera_test_output_layout'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_output_errors(integer)
RETURNS void
AS :'output_test', 'tessera_test_output_errors'
LANGUAGE C STRICT;

SELECT tessera_test_output_rows() AS rows \gset
\echo :rows
SELECT tessera_test_output_batch() AS batch \gset
\echo :batch
SELECT tessera_test_output_layout() AS layout \gset
\echo :layout

\set VERBOSITY terse
SELECT tessera_test_output_errors(0);
SELECT tessera_test_output_errors(1);
SELECT tessera_test_output_errors(2);
SELECT tessera_test_output_errors(3);
SELECT tessera_test_output_errors(4);
SELECT tessera_test_output_errors(5);
SELECT tessera_test_output_errors(6);
\set VERBOSITY default

DROP FUNCTION tessera_test_output_errors(integer);
DROP FUNCTION tessera_test_output_layout();
DROP FUNCTION tessera_test_output_batch();
DROP FUNCTION tessera_test_output_rows();
DROP EXTENSION tessera;
