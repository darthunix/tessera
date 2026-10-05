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
-- A block that is not where the list says: another partition, another
-- level, a length that is not the block's on disk, and a seek to a
-- position that holds no block.
SELECT tessera_test_spill_sqlstate(13);
SELECT tessera_test_spill_sqlstate(14);
SELECT tessera_test_spill_sqlstate(15);
SELECT tessera_test_spill_sqlstate(16);

-- temp_file_limit applies; the files are gone after the ERROR.
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
SET temp_file_limit = '2MB';
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
RESET temp_file_limit;
SELECT count(*) AS temporary_files FROM pg_ls_tmpdir();

DROP FUNCTION tessera_test_spill_serial();
DROP FUNCTION tessera_test_spill_shared();
DROP FUNCTION tessera_test_spill_packed();
DROP FUNCTION tessera_test_spill_sqlstate(integer);
DROP FUNCTION tessera_test_spill_error(integer);
DROP FUNCTION tessera_test_spill_bytes(bigint);
DROP FUNCTION tessera_test_spill_lanes();
