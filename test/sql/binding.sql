CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set binding_test :libdir '/tessera_binding_test' :dlsuffix
LOAD :'binding_test';

CREATE FUNCTION tessera_test_binding()
RETURNS boolean
AS :'binding_test', 'tessera_test_binding'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_binding_context_reset()
RETURNS boolean
AS :'binding_test', 'tessera_test_binding_context_reset'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_duplicate_binding()
RETURNS void
AS :'binding_test', 'tessera_test_duplicate_binding'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_layout(integer)
RETURNS void
AS :'binding_test', 'tessera_test_invalid_layout'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_request(integer)
RETURNS void
AS :'binding_test', 'tessera_test_invalid_request'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_request_after_freeze()
RETURNS void
AS :'binding_test', 'tessera_test_request_after_freeze'
LANGUAGE C STRICT;

SELECT tessera_test_binding() AS binding \gset
\echo :binding
SELECT tessera_test_binding_context_reset() AS context_reset \gset
\echo :context_reset

SELECT tessera_test_duplicate_binding();
SELECT tessera_test_invalid_layout(0);
SELECT tessera_test_invalid_layout(1);
SELECT tessera_test_invalid_layout(2);
SELECT tessera_test_invalid_request(0);
SELECT tessera_test_invalid_request(1);
SELECT tessera_test_invalid_request(2);
SELECT tessera_test_invalid_request(3);
SELECT tessera_test_request_after_freeze();

DROP FUNCTION tessera_test_request_after_freeze();
DROP FUNCTION tessera_test_invalid_request(integer);
DROP FUNCTION tessera_test_invalid_layout(integer);
DROP FUNCTION tessera_test_duplicate_binding();
DROP FUNCTION tessera_test_binding_context_reset();
DROP FUNCTION tessera_test_binding();
DROP EXTENSION tessera;
