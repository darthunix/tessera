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

SELECT tessera_test_spill_serial() AS serial \gset
\echo :serial
SELECT tessera_test_spill_shared() AS shared \gset
\echo :shared
-- A chunk of records goes packed, and reads back whole.
SELECT tessera_test_spill_packed() AS packed \gset
\echo :packed

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

-- temp_file_limit applies; the files are gone after the ERROR.
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
SET temp_file_limit = '2MB';
SELECT tessera_test_spill_bytes(3 * 1024 * 1024);
RESET temp_file_limit;
SELECT count(*) AS temporary_files FROM pg_ls_tmpdir();

DROP FUNCTION tessera_test_spill_serial();
DROP FUNCTION tessera_test_spill_shared();
DROP FUNCTION tessera_test_spill_packed();
DROP FUNCTION tessera_test_spill_error(integer);
DROP FUNCTION tessera_test_spill_bytes(bigint);
