CREATE EXTENSION tessera;
LOAD 'tessera_kernels';

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set module_test :libdir '/tessera_kernels_module_test' :dlsuffix
LOAD :'module_test';

CREATE FUNCTION tessera_test_kernels_module_registry()
RETURNS boolean
AS :'module_test', 'tessera_test_kernels_module_registry'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_kernels_module_arithmetic()
RETURNS boolean
AS :'module_test', 'tessera_test_kernels_module_arithmetic'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_kernels_module_predicate()
RETURNS boolean
AS :'module_test', 'tessera_test_kernels_module_predicate'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_kernels_module_errors()
RETURNS boolean
AS :'module_test', 'tessera_test_kernels_module_errors'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_kernels_module_aggregates()
RETURNS boolean
AS :'module_test', 'tessera_test_kernels_module_aggregates'
LANGUAGE C STRICT;

SELECT tessera_test_kernels_module_registry() AS registry \gset
\echo :registry
SELECT tessera_test_kernels_module_arithmetic() AS arithmetic \gset
\echo :arithmetic
SELECT tessera_test_kernels_module_predicate() AS predicate \gset
\echo :predicate
SELECT tessera_test_kernels_module_errors() AS errors \gset
\echo :errors
SELECT tessera_test_kernels_module_aggregates() AS aggregates \gset
\echo :aggregates

DROP FUNCTION tessera_test_kernels_module_aggregates();
DROP FUNCTION tessera_test_kernels_module_errors();
DROP FUNCTION tessera_test_kernels_module_predicate();
DROP FUNCTION tessera_test_kernels_module_arithmetic();
DROP FUNCTION tessera_test_kernels_module_registry();
DROP EXTENSION tessera;
