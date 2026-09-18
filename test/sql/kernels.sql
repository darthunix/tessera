\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set kernels_test :libdir '/tessera_kernels_test' :dlsuffix
LOAD :'kernels_test';

CREATE FUNCTION tessera_test_kernels_layout()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_layout'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_filter()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_filter'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_errors()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_errors'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_aggregates()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_aggregates'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_arithmetic()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_arithmetic'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_panic()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_panic'
LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_kernels_report()
RETURNS boolean
AS :'kernels_test', 'tessera_test_kernels_report'
LANGUAGE C STRICT;

SELECT tessera_test_kernels_layout() AS layout \gset
\echo :layout
SELECT tessera_test_kernels_filter() AS filter \gset
\echo :filter
SELECT tessera_test_kernels_errors() AS errors \gset
\echo :errors
SELECT tessera_test_kernels_aggregates() AS aggregates \gset
\echo :aggregates
SELECT tessera_test_kernels_arithmetic() AS arithmetic \gset
\echo :arithmetic
SELECT tessera_test_kernels_panic() AS panic \gset
\echo :panic
SELECT tessera_test_kernels_report();

DROP FUNCTION tessera_test_kernels_layout();
DROP FUNCTION tessera_test_kernels_filter();
DROP FUNCTION tessera_test_kernels_errors();
DROP FUNCTION tessera_test_kernels_aggregates();
DROP FUNCTION tessera_test_kernels_arithmetic();
DROP FUNCTION tessera_test_kernels_panic();
DROP FUNCTION tessera_test_kernels_report();
