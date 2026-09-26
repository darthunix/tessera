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
CREATE FUNCTION tessera_test_spill_header()
RETURNS boolean
AS :'table_test', 'tessera_test_spill_header'
LANGUAGE C STRICT;

SELECT tessera_test_table_layout() AS layout \gset
\echo :layout
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
SELECT tessera_test_spill_header() AS spill \gset
\echo :spill

DROP FUNCTION tessera_test_table_layout();
DROP FUNCTION tessera_test_table_cycle();
DROP FUNCTION tessera_test_table_groups();
DROP FUNCTION tessera_test_table_regrow();
DROP FUNCTION tessera_test_table_errors();
DROP FUNCTION tessera_test_table_partitions();
DROP FUNCTION tessera_test_table_combine();
DROP FUNCTION tessera_test_spill_header();
