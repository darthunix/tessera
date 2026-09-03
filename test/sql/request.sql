\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set request_test :libdir '/tessera_request_test' :dlsuffix
LOAD :'request_test';

CREATE FUNCTION tessera_test_request()
RETURNS boolean
AS :'request_test', 'tessera_test_request'
LANGUAGE C STRICT;

SELECT tessera_test_request() AS request \gset
\echo :request

DROP FUNCTION tessera_test_request();
