CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set shared_stats_test :libdir '/tessera_shared_stats_test' :dlsuffix
LOAD :'shared_stats_test';

CREATE FUNCTION tessera_test_shared_stats_sum()
RETURNS boolean
AS :'shared_stats_test', 'tessera_test_shared_stats_sum'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_shared_stats_segment()
RETURNS boolean
AS :'shared_stats_test', 'tessera_test_shared_stats_segment'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_shared_stats_errors(integer)
RETURNS void
AS :'shared_stats_test', 'tessera_test_shared_stats_errors'
LANGUAGE C STRICT;

-- Rows of the leader and two workers in a plain chunk: sum, replace, reset, end.
SELECT tessera_test_shared_stats_sum();
-- A dynamic shared memory segment: summed at detach, or at end before it.
SELECT tessera_test_shared_stats_segment();
-- Errors: no counters, negative workers, no chunk, a chunk not laid out,
-- the leader's slot and a slot past the workers, no values, no handle.
SELECT tessera_test_shared_stats_errors(0);
SELECT tessera_test_shared_stats_errors(1);
SELECT tessera_test_shared_stats_errors(2);
SELECT tessera_test_shared_stats_errors(3);
SELECT tessera_test_shared_stats_errors(4);
SELECT tessera_test_shared_stats_errors(5);
SELECT tessera_test_shared_stats_errors(6);
SELECT tessera_test_shared_stats_errors(7);

DROP FUNCTION tessera_test_shared_stats_sum();
DROP FUNCTION tessera_test_shared_stats_segment();
DROP FUNCTION tessera_test_shared_stats_errors(integer);
DROP EXTENSION tessera;
