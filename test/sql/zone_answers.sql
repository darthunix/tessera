-- The answer of one second: when the time zone library fails to find the
-- span of an instant (pg_next_dst_boundary, a failure its sources call
-- unreachable), the offset of that second (pg_localtime) is the answer,
-- and the kernels keep it as a span of that one second. The injection
-- point "tessera-zone-span" makes every span a failure; the answers of
-- date_trunc, extract and the casts of a timestamptz stay the core's.
-- Without injection points the suite ends here.
\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set spill_test :libdir '/tessera_spill_test' :dlsuffix
\set zone_test :libdir '/tessera_zone_test' :dlsuffix
\set zone_module :libdir '/tessera_zone_test'
CREATE FUNCTION tessera_test_spill_injection()
RETURNS boolean
AS :'spill_test', 'tessera_test_spill_injection'
LANGUAGE C STRICT;
SELECT NOT tessera_test_spill_injection() AS skip \gset
\if :skip
DROP FUNCTION tessera_test_spill_injection();
\quit
\endif

CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
CREATE FUNCTION zone_fail(text) RETURNS void
AS :'zone_test', 'tessera_test_zone_fail' LANGUAGE C;
CREATE FUNCTION zone_failures() RETURNS integer
AS :'zone_test', 'tessera_test_zone_failures' LANGUAGE C STRICT;

-- The same result with Tessera on and off, as text.
CREATE FUNCTION zone_same(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text[];
    without_tessera text[];
    wrapped text := format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO without_tessera;
    PERFORM set_config('tessera.enable', 'on', true);
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('on: %s off: %s', with_tessera, without_tessera);
    END IF;
    RETURN 'same';
END
$$;

-- Instants seven hours and a quarter apart over a year and a half of a
-- zone with summer time, across three changes of it, and their days.
CREATE TABLE zone_t AS
SELECT i, timestamptz '2000-01-01 00:00+00' + i * interval '7 hours 13 minutes' AS tz,
       date '2000-01-01' + i / 3 AS d
FROM generate_series(1, 2000) AS i;
ANALYZE zone_t;
SET TimeZone = 'Europe/Berlin';
EXPLAIN (COSTS OFF) SELECT date_trunc('hour', tz), date_trunc('day', tz), count(*) FROM zone_t GROUP BY 1, 2;
SELECT zone_fail(:'zone_module');
SELECT zone_same($$SELECT date_trunc('hour', tz), date_trunc('day', tz), count(*) FROM zone_t GROUP BY 1, 2$$);
SELECT zone_same($$SELECT tz::date, d::timestamptz, extract(hour FROM tz), count(*) FROM zone_t GROUP BY 1, 2, 3$$);
SELECT zone_failures() > 0 AS failed;
SELECT zone_fail(NULL);
SELECT zone_same($$SELECT date_trunc('hour', tz), tz::date, count(*) FROM zone_t GROUP BY 1, 2$$);
RESET TimeZone;
DROP TABLE zone_t;
DROP FUNCTION zone_same(text);
DROP FUNCTION zone_failures();
DROP FUNCTION zone_fail(text);
DROP FUNCTION tessera_test_spill_injection();
DROP EXTENSION tessera;
