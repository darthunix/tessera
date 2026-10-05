\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set spill_test :libdir '/tessera_spill_test' :dlsuffix
LOAD :'spill_test';

CREATE FUNCTION tessera_test_spill_serial()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_serial'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_shared()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_shared'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_packed()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_packed'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_error(integer)
RETURNS void
AS :'spill_test', 'tessera_test_spill_error'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_bytes(bigint)
RETURNS void
AS :'spill_test', 'tessera_test_spill_bytes'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_lanes()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_lanes'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_memory()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_memory'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_shared_bytes(bigint)
RETURNS void
AS :'spill_test', 'tessera_test_spill_shared_bytes'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_tablespace(oid)
RETURNS bigint
AS :'spill_test', 'tessera_test_spill_tablespace'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_value()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_value'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_spill_reset()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_reset'
LANGUAGE C STRICT;

SELECT tessera_test_spill_serial() AS serial \gset
\echo :serial
SELECT tessera_test_spill_shared() AS shared \gset
\echo :shared
-- A chunk of records goes packed, and reads back whole.
SELECT tessera_test_spill_packed() AS packed \gset
\echo :packed
-- The C formulas of a chunk's lanes are Rust's at every count of words.
SELECT tessera_test_spill_lanes() AS lanes \gset
\echo :lanes
-- A reference to a by-reference value, and what is none.
SELECT tessera_test_spill_value() AS value \gset
\echo :value
-- A shared file set used again: nothing of its first use is read.
SELECT tessera_test_spill_reset() AS reset \gset
\echo :reset
-- The bytes a set holds, and a chunk longer than its write buffer.
SELECT tessera_test_spill_memory() AS memory \gset
\echo :memory

-- Every misuse and damaged block is an ERROR.
SELECT tessera_test_spill_error(1);
SELECT tessera_test_spill_error(2);
SELECT tessera_test_spill_error(3);
SELECT tessera_test_spill_error(4);
SELECT tessera_test_spill_error(5);
SELECT tessera_test_spill_error(6);
SELECT tessera_test_spill_error(7);
SELECT tessera_test_spill_error(8);
SELECT tessera_test_spill_error(9);

-- A damaged file is damaged data, SQLSTATE XX001; a wrong call is an
-- internal error, XX000.
CREATE FUNCTION tessera_test_spill_sqlstate(which integer)
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM tessera_test_spill_error(which);
    RETURN 'no error';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE || ': ' || SQLERRM;
END
$$;
-- A wrong call, and a block of another table.
SELECT tessera_test_spill_sqlstate(5);
SELECT tessera_test_spill_sqlstate(7);
-- A shared file without its lists: cut short, a trailer without its
-- magic, a trailer of another number of partitions.
SELECT tessera_test_spill_sqlstate(10);
SELECT tessera_test_spill_sqlstate(11);
SELECT tessera_test_spill_sqlstate(12);
-- Lists that do not fit the file: a count of more blocks than the lists
-- hold, and a block that ends past the file's blocks.
SELECT tessera_test_spill_sqlstate(17);
SELECT tessera_test_spill_sqlstate(18);
-- A block that is not where the list says: another partition, another
-- level, a length that is not the block's on disk.
SELECT tessera_test_spill_sqlstate(13);
SELECT tessera_test_spill_sqlstate(14);
SELECT tessera_test_spill_sqlstate(15);
-- A packed block whose header names another length than its body
-- unpacks into.
SELECT tessera_test_spill_sqlstate(19);
-- A packed block whose body is damaged in the file.
SELECT tessera_test_spill_sqlstate(23);
-- A block that the list says is shorter than a header, or longer than
-- the longest block a set takes; a count that does not hold in a
-- partition the reader does not open.
SELECT tessera_test_spill_sqlstate(22);
SELECT tessera_test_spill_sqlstate(24);
SELECT tessera_test_spill_sqlstate(25);
-- A block that the list says runs into the next block of its partition.
SELECT tessera_test_spill_sqlstate(26);
-- A row read back that refers to a value outside its chunk of values.
SELECT tessera_test_spill_sqlstate(28);
-- A file opened before its writer finished cannot be told from a
-- damaged one.
SELECT tessera_test_spill_sqlstate(27);
-- Wrong calls: a seek to a position that holds no block, a partition
-- dropped while it is read, a block of a kind that does not exist.
SELECT tessera_test_spill_sqlstate(16);
SELECT tessera_test_spill_sqlstate(20);
SELECT tessera_test_spill_sqlstate(21);

-- temp_file_limit applies; the files are gone after the ERROR.
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
SET temp_file_limit = '2MB';
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
-- The limit holds for the files of a shared set too.
SELECT tessera_test_spill_shared_bytes(3 * 1024 * 1024);
RESET temp_file_limit;
SELECT tessera_test_spill_shared_bytes(3 * 1024 * 1024);
-- Nothing is left: no temporary file, and no directory of a shared set.
SELECT count(*) AS temporary_files FROM pg_ls_tmpdir();
SELECT count(*) AS temporary_entries
FROM pg_ls_dir('base/pgsql_tmp', true, false);

-- A serial set's file is made in the session's temporary tablespace.
SET allow_in_place_tablespaces = true;
CREATE TABLESPACE tessera_spill_space LOCATION '';
SET temp_tablespaces = tessera_spill_space;
SELECT tessera_test_spill_tablespace(oid) AS files_in_the_tablespace
FROM pg_tablespace WHERE spcname = 'tessera_spill_space';
RESET temp_tablespaces;
SELECT count(*) AS files_left_in_the_tablespace
FROM pg_tablespace, pg_ls_tmpdir(oid) WHERE spcname = 'tessera_spill_space';
DROP TABLESPACE tessera_spill_space;
RESET allow_in_place_tablespaces;

DROP FUNCTION tessera_test_spill_serial();
DROP FUNCTION tessera_test_spill_shared();
DROP FUNCTION tessera_test_spill_packed();
DROP FUNCTION tessera_test_spill_sqlstate(integer);
DROP FUNCTION tessera_test_spill_error(integer);
DROP FUNCTION tessera_test_spill_bytes(bigint);
DROP FUNCTION tessera_test_spill_lanes();
DROP FUNCTION tessera_test_spill_memory();
DROP FUNCTION tessera_test_spill_shared_bytes(bigint);
DROP FUNCTION tessera_test_spill_tablespace(oid);
DROP FUNCTION tessera_test_spill_value();
DROP FUNCTION tessera_test_spill_reset();
