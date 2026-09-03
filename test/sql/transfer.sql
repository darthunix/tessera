CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set transfer_test :libdir '/tessera_transfer_test' :dlsuffix
LOAD :'transfer_test';

CREATE FUNCTION tessera_test_transfer()
RETURNS boolean
AS :'transfer_test', 'tessera_test_transfer'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_batch(integer)
RETURNS void
AS :'transfer_test', 'tessera_test_invalid_batch'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_double_publish()
RETURNS void
AS :'transfer_test', 'tessera_test_double_publish'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_publish_freezes_request()
RETURNS void
AS :'transfer_test', 'tessera_test_publish_freezes_request'
LANGUAGE C STRICT;

SELECT tessera_test_transfer() AS transfer \gset
\echo :transfer

SELECT tessera_test_invalid_batch(0);
SELECT tessera_test_invalid_batch(1);
SELECT tessera_test_invalid_batch(2);
SELECT tessera_test_invalid_batch(3);
SELECT tessera_test_invalid_batch(4);
SELECT tessera_test_invalid_batch(5);
SELECT tessera_test_invalid_batch(6);
SELECT tessera_test_invalid_batch(7);
SELECT tessera_test_invalid_batch(8);
SELECT tessera_test_invalid_batch(9);
SELECT tessera_test_invalid_batch(10);
SELECT tessera_test_double_publish();
SELECT tessera_test_publish_freezes_request();

DROP FUNCTION tessera_test_publish_freezes_request();
DROP FUNCTION tessera_test_double_publish();
DROP FUNCTION tessera_test_invalid_batch(integer);
DROP FUNCTION tessera_test_transfer();
DROP EXTENSION tessera;
