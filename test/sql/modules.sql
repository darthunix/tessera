CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set producer_test :libdir '/tessera_producer_test' :dlsuffix
\set consumer_test :libdir '/tessera_consumer_test' :dlsuffix
LOAD :'producer_test';
LOAD :'consumer_test';

CREATE FUNCTION tessera_test_modules(regprocedure)
RETURNS boolean
AS :'producer_test', 'tessera_test_modules'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_consumer(internal, boolean)
RETURNS boolean
AS :'consumer_test', 'tessera_test_consumer'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_consumer_error(internal, boolean)
RETURNS boolean
AS :'consumer_test', 'tessera_test_consumer_error'
LANGUAGE C STRICT;

SELECT tessera_test_modules('tessera_test_consumer(internal,boolean)') AS modules \gset
\echo :modules

SELECT tessera_test_modules('int4abs(integer)');
SELECT tessera_test_modules('tessera_test_consumer_error(internal,boolean)');

SELECT tessera_test_modules('tessera_test_consumer(internal,boolean)') AS modules \gset
\echo :modules

DROP FUNCTION tessera_test_consumer_error(internal, boolean);
DROP FUNCTION tessera_test_consumer(internal, boolean);
DROP FUNCTION tessera_test_modules(regprocedure);
DROP EXTENSION tessera;
