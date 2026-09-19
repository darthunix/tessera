CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set bridge_test :libdir '/tessera_bridge_test' :dlsuffix
LOAD :'bridge_test';

CREATE FUNCTION tessera_test_api_visible()
RETURNS boolean
AS :'bridge_test', 'tessera_test_api_visible'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_abi_helpers()
RETURNS boolean
AS :'bridge_test', 'tessera_test_abi_helpers'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_settings()
RETURNS boolean
AS :'bridge_test', 'tessera_test_settings'
LANGUAGE C STRICT;

SELECT tessera_test_api_visible() AS api_visible \gset
\echo :api_visible
SELECT tessera_test_abi_helpers() AS abi_helpers \gset
\echo :abi_helpers

-- The bridge defines tessera.enable and publishes it through the API.
SHOW tessera.enable;
SELECT tessera_test_settings() AS enabled \gset
\echo :enabled
SET tessera.enable = off;
SELECT tessera_test_settings() AS enabled \gset
\echo :enabled
RESET tessera.enable;
SELECT tessera_test_settings() AS enabled \gset
\echo :enabled
\set VERBOSITY terse
SET tessera.enabel = off;
\set VERBOSITY default

DROP FUNCTION tessera_test_settings();
DROP FUNCTION tessera_test_abi_helpers();
DROP FUNCTION tessera_test_api_visible();
DROP EXTENSION tessera;
