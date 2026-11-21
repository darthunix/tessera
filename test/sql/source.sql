CREATE EXTENSION tessera;

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set source_test :libdir '/tessera_source_test' :dlsuffix
LOAD :'source_test';

CREATE FUNCTION tessera_test_source_registry()
RETURNS boolean
AS :'source_test', 'tessera_test_source_registry'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_sizes()
RETURNS boolean
AS :'source_test', 'tessera_test_source_sizes'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_invalid_source(integer)
RETURNS void
AS :'source_test', 'tessera_test_invalid_source'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_duplicate_source()
RETURNS void
AS :'source_test', 'tessera_test_duplicate_source'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_duplicate_cleanup()
RETURNS boolean
AS :'source_test', 'tessera_test_duplicate_cleanup'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_list()
RETURNS text
AS :'source_test', 'tessera_test_source_list'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_remove(text)
RETURNS boolean
AS :'source_test', 'tessera_test_source_remove'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_next_unknown()
RETURNS void
AS :'source_test', 'tessera_test_source_next_unknown'
LANGUAGE C STRICT;

CREATE FUNCTION tessera_test_source_registry_check(integer)
RETURNS boolean
AS :'source_test', 'tessera_test_source_registry_check'
LANGUAGE C STRICT;

-- The SQLSTATE and message of a statement's error.
CREATE FUNCTION source_error(statement text)
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE statement;
    RETURN 'no error';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE || ': ' || SQLERRM;
END
$$;

-- spec: source-scan/Sources found and listed
SELECT tessera_test_source_registry() AS source_registry \gset
\echo :source_registry
SELECT tessera_test_source_sizes() AS source_sizes \gset
\echo :source_sizes

-- spec: source-scan/Sources refused
-- A NULL source, another version, below the least size, no name, an
-- empty name, a second source of a registered name, and next given a
-- pointer never registered; the registry stays empty.
SELECT source_error('SELECT tessera_test_invalid_source(0)');
SELECT source_error('SELECT tessera_test_invalid_source(1)');
SELECT source_error('SELECT tessera_test_invalid_source(2)');
SELECT source_error('SELECT tessera_test_invalid_source(3)');
SELECT source_error('SELECT tessera_test_invalid_source(4)');
SELECT source_error('SELECT tessera_test_duplicate_source()');
SELECT tessera_test_duplicate_cleanup() AS first_kept;
SELECT source_error('SELECT tessera_test_source_next_unknown()');
SELECT tessera_test_source_list() AS still_listed;
SELECT tessera_test_source_remove('tessera_test.one') AS removed;
SELECT tessera_test_source_list() = '' AS registry_empty;

-- spec: source-scan/A registry table too old
-- The bridge's table; one that ends at find, one of another version, and
-- NULL.
SELECT tessera_test_source_registry_check(0) AS bridge_table;
SELECT source_error('SELECT tessera_test_source_registry_check(1)');
SELECT source_error('SELECT tessera_test_source_registry_check(2)');
SELECT source_error('SELECT tessera_test_source_registry_check(3)');

DROP FUNCTION source_error(text);
DROP FUNCTION tessera_test_source_registry_check(integer);
DROP FUNCTION tessera_test_source_next_unknown();
DROP FUNCTION tessera_test_source_remove(text);
DROP FUNCTION tessera_test_source_list();
DROP FUNCTION tessera_test_duplicate_cleanup();
DROP FUNCTION tessera_test_duplicate_source();
DROP FUNCTION tessera_test_invalid_source(integer);
DROP FUNCTION tessera_test_source_sizes();
DROP FUNCTION tessera_test_source_registry();
DROP EXTENSION tessera;
