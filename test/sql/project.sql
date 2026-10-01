CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_limit';
LOAD 'tessera_kernels';

-- The same result with Tessera on and off, as text.
CREATE FUNCTION project_same(query text) RETURNS text
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
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('MISMATCH on %s off %s', with_tessera, without_tessera);
    END IF;
    RETURN with_tessera::text;
END $$;

CREATE TABLE project_t (a int, b int, c text);
INSERT INTO project_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i END, i % 10, 'r' || i
FROM generate_series(1, 200) AS i;

-- The filter computes the expressions of its targets: no Result above it.
EXPLAIN (COSTS OFF, VERBOSE)
SELECT a + 1 AS next, b, c || '!' AS shout FROM project_t WHERE a > 195;
SELECT project_same($$SELECT a + 1 AS next, b, c || '!' AS shout FROM project_t WHERE a > 195$$);
-- Chains and row-wise expressions: NULL arguments, a CASE, a cast, a function.
SELECT project_same($$SELECT -a AS neg, a * 2 + b AS mix, length(c) AS len,
    CASE WHEN a > 198 THEN 'big' ELSE 'small' END AS size, a::bigint * 3 AS wide
    FROM project_t WHERE a > 190$$);
-- A second column in a chain is a step's operand: computed over the batch.
SELECT project_same($$SELECT a + b AS s, (a + 1) * b AS p, b - a AS d FROM project_t WHERE a > 190$$);
-- Plain columns keep the plan they had.
EXPLAIN (COSTS OFF) SELECT a, c FROM project_t WHERE a > 195;
-- A limit above narrows the rows before the columns are computed.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a + 1 AS next, c || '!' AS shout FROM project_t WHERE a > 100 LIMIT 3;
SELECT a + 1 AS next, c || '!' AS shout FROM project_t WHERE a > 100 LIMIT 3;
-- An expression failing on a row the filter removed does not fail: a chain,
-- then a row-wise expression; both fail when the row survives.
SELECT project_same($$SELECT 100 / (a - 50) AS ratio FROM project_t WHERE a > 50 AND a < 55$$);
SELECT 100 / (a - 50) AS ratio FROM project_t WHERE a > 48 AND a < 52;
SELECT project_same($$SELECT 1.0 / (a - 50) AS ratio FROM project_t WHERE a > 50 AND a < 53$$);
SELECT 1.0 / (a - 50) AS ratio FROM project_t WHERE a > 49 AND a < 52;
-- A parameter in a computed column, and a rescan with another value.
PREPARE shifted(int) AS SELECT a + $1 AS shifted FROM project_t WHERE a > 197;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF, VERBOSE) EXECUTE shifted(1);
EXECUTE shifted(1);
EXECUTE shifted(1000);
RESET plan_cache_mode;
DEALLOCATE shifted;
SELECT o.a, (SELECT i.a + o.b FROM project_t AS i WHERE i.a > 190 ORDER BY 1 LIMIT 1) AS least
FROM project_t AS o WHERE o.a > 197 ORDER BY 1;
-- A row-wise expression through a sort above, and the row-wise residual
-- clause with a computed target.
SELECT project_same($$SELECT c || '!' AS shout, a FROM project_t WHERE a > 190 ORDER BY 1$$);
SELECT project_same($$SELECT a * 2 AS twice, c FROM project_t WHERE a > 190 AND c <> 'r195'$$);
-- The scan itself computes the targets of a query without clauses.
EXPLAIN (COSTS OFF, VERBOSE)
SELECT a * 2 AS twice, c || '!' AS shout FROM project_t LIMIT 3;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a * 2 AS twice, c || '!' AS shout FROM project_t LIMIT 3;
SELECT project_same($$SELECT a * 2 AS twice, c || '!' AS shout FROM project_t LIMIT 3$$);
SELECT project_same($$SELECT count(*) FROM (SELECT a + b AS s FROM project_t LIMIT 150) AS q WHERE s > 100$$);
-- A set-returning function stays above the node.
EXPLAIN (COSTS OFF) SELECT generate_series(1, a - 197) FROM project_t WHERE a > 197;
SELECT project_same($$SELECT generate_series(1, a - 197) AS n FROM project_t WHERE a > 197$$);
-- The switch off.
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a + 1 FROM project_t WHERE a > 195;
RESET tessera.enable;

DROP TABLE project_t;
DROP FUNCTION project_same(text);
DROP EXTENSION tessera;
