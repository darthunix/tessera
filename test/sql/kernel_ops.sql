CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set kernel_ops_test :libdir '/tessera_kernel_ops_test' :dlsuffix
LOAD :'kernel_ops_test';

CREATE FUNCTION tessera_test_kernel_registry()
RETURNS boolean
AS :'kernel_ops_test', 'tessera_test_kernel_registry'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_kernels(integer)
RETURNS void
AS :'kernel_ops_test', 'tessera_test_invalid_kernels'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_second_kernels()
RETURNS void
AS :'kernel_ops_test', 'tessera_test_second_kernels'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_foreign_table_format()
RETURNS void
AS :'kernel_ops_test', 'tessera_test_foreign_table_format'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_status_report(integer)
RETURNS void
AS :'kernel_ops_test', 'tessera_test_status_report'
LANGUAGE C STRICT;

SELECT tessera_test_kernel_registry() AS kernel_registry \gset
\echo :kernel_registry

\set VERBOSITY terse
SELECT tessera_test_invalid_kernels(0);
SELECT tessera_test_invalid_kernels(1);
SELECT tessera_test_invalid_kernels(2);
SELECT tessera_test_invalid_kernels(3);
SELECT tessera_test_second_kernels();
SELECT tessera_test_foreign_table_format();
\set VERBOSITY default
-- The registry is empty again after every failed call.
SELECT tessera_test_kernel_registry() AS kernel_registry_after \gset
\echo :kernel_registry_after

\set VERBOSITY sqlstate
SELECT tessera_test_status_report(0);
SELECT tessera_test_status_report(1);
SELECT tessera_test_status_report(2);
SELECT tessera_test_status_report(3);
\set VERBOSITY terse
SELECT tessera_test_status_report(1);
-- Neither a success nor a malformed SQLSTATE is reported as is.
SELECT tessera_test_status_report(2);
SELECT tessera_test_status_report(3);
\set VERBOSITY default

DROP FUNCTION tessera_test_status_report(integer);
DROP FUNCTION tessera_test_foreign_table_format();
DROP FUNCTION tessera_test_second_kernels();
DROP FUNCTION tessera_test_invalid_kernels(integer);
DROP FUNCTION tessera_test_kernel_registry();
DROP EXTENSION tessera;
