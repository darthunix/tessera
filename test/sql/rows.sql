\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set rows_test :libdir '/tessera_rows_test' :dlsuffix
LOAD :'rows_test';

CREATE FUNCTION tessera_test_rows_cycle(integer)
RETURNS boolean
AS :'rows_test', 'tessera_test_rows_cycle'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_rows_sort(integer)
RETURNS boolean
AS :'rows_test', 'tessera_test_rows_sort'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_rows_error(integer)
RETURNS void
AS :'rows_test', 'tessera_test_rows_error'
LANGUAGE C STRICT;

-- Rows read back by reference in another order than appended: one batch,
-- a batch and a bit, then enough for several chunks of records and of
-- values and two values of a chunk of their own; each also after a reset.
SELECT tessera_test_rows_cycle(1) AS one,
       tessera_test_rows_cycle(64) AS batch,
       tessera_test_rows_cycle(65) AS past_batch,
       tessera_test_rows_cycle(100000) AS many;

-- The rows sorted by their key, both ways, and read back in that order.
SELECT tessera_test_rows_sort(1) AS one,
       tessera_test_rows_sort(65) AS past_batch,
       tessera_test_rows_sort(100000) AS many;

-- Misuse is an ERROR.
SELECT tessera_test_rows_error(1);
SELECT tessera_test_rows_error(2);

DROP FUNCTION tessera_test_rows_cycle(integer);
DROP FUNCTION tessera_test_rows_sort(integer);
DROP FUNCTION tessera_test_rows_error(integer);
