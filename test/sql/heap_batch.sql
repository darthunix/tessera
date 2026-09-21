\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set heap_batch_test :libdir '/tessera_heap_batch_test' :dlsuffix
LOAD :'heap_batch_test';

CREATE FUNCTION tessera_test_heap_batch(regclass)
RETURNS boolean
AS :'heap_batch_test', 'tessera_test_heap_batch'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_heap_batch_errors(integer, regclass)
RETURNS void
AS :'heap_batch_test', 'tessera_test_heap_batch_errors'
LANGUAGE C STRICT;

-- Wide rows, so that one batch of 64 rows spans several pages.
CREATE TABLE heap_batch_t (a int, b text, c bigint);
INSERT INTO heap_batch_t
SELECT i, CASE WHEN i % 3 = 0 THEN NULL ELSE repeat('x', 300) END, i * 10
FROM generate_series(1, 200) AS i;
SELECT count(DISTINCT (ctid::text::point)[0]) > 4 AS pages FROM heap_batch_t;

SELECT tessera_test_heap_batch('heap_batch_t') AS heap_batch \gset
\echo :heap_batch

\set VERBOSITY terse
SELECT tessera_test_heap_batch_errors(0, 'heap_batch_t');
SELECT tessera_test_heap_batch_errors(1, 'heap_batch_t');
SELECT tessera_test_heap_batch_errors(2, 'heap_batch_t');
SELECT tessera_test_heap_batch_errors(3, 'heap_batch_t');
SELECT tessera_test_heap_batch_errors(4, 'heap_batch_t');
SELECT tessera_test_heap_batch_errors(5, 'heap_batch_t');
\set VERBOSITY default

DROP TABLE heap_batch_t;
DROP FUNCTION tessera_test_heap_batch_errors(integer, regclass);
DROP FUNCTION tessera_test_heap_batch(regclass);
