CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
LOAD 'tessera_limit';

-- The same result with Tessera on and off, as text.
CREATE FUNCTION forward_same(query text) RETURNS text
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

CREATE TABLE forward_t (a int, b text);
INSERT INTO forward_t
SELECT i, CASE WHEN i % 10 = 0 THEN NULL ELSE 'row' || i END
FROM generate_series(1, 100) AS i;

-- A subquery with LIMIT cannot be pulled up: the planner keeps a subquery
-- scan between the aggregate and the limit. count(*) needs none of its
-- outputs, so the scan is not trivial and stays in the plan; the pack
-- forwards the batches of the limit under it, and the scan never runs.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s;
SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s;

-- Columns through the map: the subquery scan is trivial here, so the
-- planner drops it and the pack stands above the limit itself.
EXPLAIN (COSTS OFF)
SELECT sum(a), count(a), max(a) FROM (SELECT a FROM forward_t LIMIT 70) AS s;
SELECT sum(a), count(a), max(a) FROM (SELECT a FROM forward_t LIMIT 70) AS s;

-- A target of the subquery that is not its first column, and only some
-- of its columns used: the scan stays and the map follows the targets.
EXPLAIN (COSTS OFF)
SELECT sum(a), max(a) FROM (SELECT b, a FROM forward_t OFFSET 95) AS s;
SELECT sum(a), max(a) FROM (SELECT b, a FROM forward_t OFFSET 95) AS s;
SELECT count(*), min(a) FROM (SELECT a FROM forward_t OFFSET 90) AS s;

-- A limit above the subquery: the bound reaches the limit under it, and
-- the rows are served to the client by the outer limit.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM (SELECT a FROM forward_t OFFSET 90) AS s LIMIT 3;
SELECT a FROM (SELECT a FROM forward_t OFFSET 90) AS s LIMIT 3;

-- A correlated subquery: every value rescans through the subquery scan,
-- with the changed parameter reaching the limit and the filter under it.
SELECT g, (SELECT count(*) FROM (SELECT a FROM forward_t LIMIT g) AS s) AS n
FROM generate_series(0, 3) AS g;
SELECT g, (SELECT sum(a) FROM (SELECT a FROM forward_t WHERE a > g LIMIT 5) AS s) AS n
FROM generate_series(95, 99) AS g;

-- A sort under the limit: the limit's own pack packs the sorted rows, and
-- the pack above the subquery scan still forwards the limit's batches.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM (SELECT a FROM forward_t ORDER BY a DESC LIMIT 5) AS s;
SELECT count(*), min(a) FROM (SELECT a FROM forward_t ORDER BY a DESC LIMIT 5) AS s;

-- A clause the planner cannot push into the subquery is the scan's own:
-- the scan evaluates it, and the pack packs its rows.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s WHERE a % 2 = 0;
SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s WHERE a % 2 = 0;

-- An empty subquery result.
SELECT count(*), sum(a) FROM (SELECT a FROM forward_t OFFSET 100) AS s;

SELECT forward_same('SELECT count(*) FROM (SELECT a FROM forward_t LIMIT 70) AS s');
SELECT forward_same('SELECT sum(a), max(a) FROM (SELECT b, a FROM forward_t OFFSET 95) AS s');
SELECT forward_same('SELECT a FROM (SELECT a FROM forward_t OFFSET 90) AS s LIMIT 3');
SELECT forward_same('SELECT g, (SELECT sum(a) FROM (SELECT a FROM forward_t WHERE a > g LIMIT 5) AS s) FROM generate_series(95, 99) AS g');

DROP TABLE forward_t;
DROP FUNCTION forward_same(text);
DROP EXTENSION tessera;
