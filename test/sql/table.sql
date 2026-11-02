\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set table_test :libdir '/tessera_table_test' :dlsuffix
LOAD :'table_test';

CREATE FUNCTION tessera_test_table_layout()
RETURNS boolean
AS :'table_test', 'tessera_test_table_layout'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_cycle()
RETURNS boolean
AS :'table_test', 'tessera_test_table_cycle'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_groups()
RETURNS boolean
AS :'table_test', 'tessera_test_table_groups'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_regrow()
RETURNS boolean
AS :'table_test', 'tessera_test_table_regrow'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_errors()
RETURNS boolean
AS :'table_test', 'tessera_test_table_errors'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_partitions()
RETURNS boolean
AS :'table_test', 'tessera_test_table_partitions'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_combine()
RETURNS boolean
AS :'table_test', 'tessera_test_table_combine'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_shared_spill()
RETURNS boolean
AS :'table_test', 'tessera_test_table_shared_spill'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_header()
RETURNS boolean
AS :'table_test', 'tessera_test_spill_header'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_record_size()
RETURNS boolean
AS :'table_test', 'tessera_test_table_record_size'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_memory_limit()
RETURNS boolean
AS :'table_test', 'tessera_test_table_memory_limit'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_table_chunks_limit()
RETURNS text
AS :'table_test', 'tessera_test_table_chunks_limit'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_key_bytes(regtype, oid)
RETURNS text
AS :'table_test', 'tessera_test_key_bytes'
LANGUAGE C STRICT;

SELECT tessera_test_table_layout() AS layout \gset
\echo :layout
-- The planner's estimate of a record is the kernels' size of it.
SELECT tessera_test_table_record_size() AS record_size \gset
\echo :record_size
-- A node lets one table take at most 16 GiB, half of what its chunks hold.
SELECT tessera_test_table_memory_limit() AS memory_limit \gset
\echo :memory_limit
-- A node's directory of a table holds the most chunks and fails one past
-- them, SQLSTATE 54000.
-- spec: table-memory/The limit of a directory
SELECT tessera_test_table_chunks_limit();
-- The keys a grouping and a join hash and compare by their bytes, as the
-- type's functions read them: text, varchar and bytea whole, char(n)
-- without its trailing blanks, under a deterministic collation; under a
-- nondeterministic one, without a collation and of any other type by the
-- functions. bytea's own hash functions came in PostgreSQL 18.
CREATE COLLATION table_nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT t AS type, c.collname, tessera_test_key_bytes(t, c.oid) AS "grouping/join"
FROM unnest(ARRAY['text', 'varchar', 'bpchar', 'bytea', 'name', 'numeric', 'uuid']::regtype[]) AS t,
     (SELECT oid, collname FROM pg_collation WHERE collname IN ('C', 'und-x-icu', 'table_nd')) AS c
ORDER BY t::text, c.collname;
SELECT tessera_test_key_bytes('text', 0) AS no_collation, tessera_test_key_bytes('bytea', 0) AS bytea;
DROP COLLATION table_nd;
SELECT tessera_test_table_cycle() AS cycle \gset
\echo :cycle
SELECT tessera_test_table_groups() AS groups \gset
\echo :groups
SELECT tessera_test_table_regrow() AS regrow \gset
\echo :regrow
SELECT tessera_test_table_errors() AS errors \gset
\echo :errors
SELECT tessera_test_table_partitions() AS partitions \gset
\echo :partitions
SELECT tessera_test_table_combine() AS combine \gset
\echo :combine
SELECT tessera_test_table_shared_spill() AS shared_spill \gset
\echo :shared_spill
SELECT tessera_test_spill_header() AS spill \gset
\echo :spill

DROP FUNCTION tessera_test_table_layout();
DROP FUNCTION tessera_test_table_record_size();
DROP FUNCTION tessera_test_table_memory_limit();
DROP FUNCTION tessera_test_table_chunks_limit();
DROP FUNCTION tessera_test_key_bytes(regtype, oid);
DROP FUNCTION tessera_test_table_cycle();
DROP FUNCTION tessera_test_table_groups();
DROP FUNCTION tessera_test_table_regrow();
DROP FUNCTION tessera_test_table_errors();
DROP FUNCTION tessera_test_table_partitions();
DROP FUNCTION tessera_test_table_combine();
DROP FUNCTION tessera_test_table_shared_spill();
DROP FUNCTION tessera_test_spill_header();
