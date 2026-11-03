-- The lookupcost family: the times of TessHashJoin's lookup mode and of
-- the core's nested loop over the same index, from which the planner's
-- model of the lookup is fitted (tessera.join_lookup_batch_cost and the
-- rest, docs/costs.md, the index join). Every sample joins
-- bench_lookup_outer to bench_lookup of the indexjoin family through an
-- index of it, and varies what the model counts: the keys of a batch in
-- the index's order or shuffled, keys a stride apart, the rows a batch
-- holds (all, a share), the rows a key (one and four), and the key's
-- type. Each runs with the node, its lookup forced by its prices set to
-- nothing for the plan (the run is the same), and with the core's nested
-- loop, the core's other joins off. The node's own time is the query's
-- less its outer side's alone; the core's likewise. The summary fits both
-- times over the counts of the node's plan and prints the parameters'
-- values in the units of the scan model, a page of the full scan being
-- 1, each sample against its prediction with the choice the model makes
-- between the two, and the planner's counts against the plan's.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 7
\endif
SET jit = off;
SET max_parallel_workers_per_gather = 0;
SET work_mem = '256MB';
SET tessera.enable = on;
SELECT set_config('bench.repetitions', :'repetitions', false);

/*
 * A sample: the plan's counts of the node's run (the lookups, the keys
 * given, the btree's descents, the rows fetched, the outer rows probed,
 * the pairs), the planner's own counts (the join's Lookup Estimate), the
 * times of the node's query and of its outer side alone, and the core's.
 */
CREATE TEMP TABLE samples
(
    name text,
    lookups float8,
    keys float8,
    descents float8,
    fetched float8,
    probed float8,
    pairs float8,
    published float8,
    hashed integer,
    est_batches float8,
    est_keys float8,
    est_descents float8,
    est_rows float8,
    node_ms float8,
    outer_ms float8,
    core_ms float8,
    core_outer_ms float8
);

CREATE TEMP TABLE scans
(
    query text PRIMARY KEY,
    milliseconds float8
);

/* The minimum time of repetitions runs, after two warm-ups. */
CREATE FUNCTION pg_temp.fastest(query text, repetitions integer)
RETURNS float8
LANGUAGE plpgsql
AS $function$
DECLARE
    started_at timestamptz;
    best float8;
BEGIN
    EXECUTE query;
    EXECUTE query;
    FOR sample IN 1..repetitions LOOP
        started_at := clock_timestamp();
        EXECUTE query;
        best := least(best, 1000 * extract(epoch FROM clock_timestamp() - started_at));
    END LOOP;
    RETURN best;
END
$function$;

/* The first value of a key anywhere in a plan, depth first. */
CREATE FUNCTION pg_temp.find_key(node jsonb, key text)
RETURNS text
LANGUAGE plpgsql
AS $function$
DECLARE
    child jsonb;
    found text;
BEGIN
    IF node ? key THEN
        RETURN node ->> key;
    END IF;
    FOR child IN SELECT * FROM jsonb_array_elements(coalesce(node -> 'Plans', '[]'::jsonb)) LOOP
        found := pg_temp.find_key(child, key);
        IF found IS NOT NULL THEN
            RETURN found;
        END IF;
    END LOOP;
    RETURN NULL;
END
$function$;

/* The first node of a kind anywhere in a plan, depth first. */
CREATE FUNCTION pg_temp.find_node(node jsonb, kind text)
RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    child jsonb;
    found jsonb;
BEGIN
    IF coalesce(node ->> 'Custom Plan Provider', node ->> 'Node Type') = kind THEN
        RETURN node;
    END IF;
    FOR child IN SELECT * FROM jsonb_array_elements(coalesce(node -> 'Plans', '[]'::jsonb)) LOOP
        found := pg_temp.find_node(child, kind);
        IF found IS NOT NULL THEN
            RETURN found;
        END IF;
    END LOOP;
    RETURN NULL;
END
$function$;

/* A number of the planner's Lookup Estimate, by its name. */
CREATE FUNCTION pg_temp.estimate(estimate text, name text)
RETURNS float8
LANGUAGE sql
AS $function$
    SELECT (regexp_match(estimate, name || '=([-0-9.e+]+)'))[1]::float8
$function$;

/*
 * A sample: the query with the node, its lookup forced, explained for its
 * counts and timed; its outer side alone timed with the node; then the
 * query with the core's nested loop over the index, explained for its
 * cost and timed, and its outer side alone with Tessera off.
 */
CREATE FUNCTION pg_temp.sample(name text, query text, outer_query text, repetitions integer,
                               hashed integer DEFAULT 0)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    plan jsonb;
    node jsonb;
    loop_node jsonb;
    estimate text;
    node_ms float8;
    outer_ms float8;
    saved text[];
    published float8;
BEGIN
    saved := ARRAY[current_setting('tessera.join_lookup_batch_cost'),
                   current_setting('tessera.join_lookup_descent_cost'),
                   current_setting('tessera.join_lookup_key_cost')];
    PERFORM set_config('tessera.enable', 'on', false);
    PERFORM set_config('tessera.join_lookup_batch_cost', '0', false);
    PERFORM set_config('tessera.join_lookup_descent_cost', '0', false);
    PERFORM set_config('tessera.join_lookup_key_cost', '0', false);
    EXECUTE 'EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query
        INTO plan;
    node := pg_temp.find_node(plan -> 0 -> 'Plan', 'TessHashJoin');
    IF node IS NULL OR node ->> 'Inner Lookups' IS NULL THEN
        RAISE EXCEPTION 'no index lookup in the plan of %: %', name, query;
    END IF;
    estimate := node ->> 'Lookup Estimate';
    published := (pg_temp.find_node(plan -> 0 -> 'Plan', 'TessAgg') ->> 'Input Batches')::float8;
    node_ms := pg_temp.fastest(query, repetitions);
    outer_ms := pg_temp.fastest(outer_query, repetitions);
    PERFORM set_config('tessera.join_lookup_batch_cost', saved[1], false);
    PERFORM set_config('tessera.join_lookup_descent_cost', saved[2], false);
    PERFORM set_config('tessera.join_lookup_key_cost', saved[3], false);

    PERFORM set_config('tessera.enable', 'off', false);
    PERFORM set_config('enable_hashjoin', 'off', false);
    PERFORM set_config('enable_mergejoin', 'off', false);
    PERFORM set_config('enable_memoize', 'off', false);
    PERFORM set_config('enable_bitmapscan', 'off', false);
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || query INTO plan;
    loop_node := pg_temp.find_node(plan -> 0 -> 'Plan', 'Nested Loop');
    IF loop_node IS NULL OR loop_node -> 'Plans' -> 1 ->> 'Node Type' <> 'Index Scan' THEN
        RAISE EXCEPTION 'no nested loop over an index in the core''s plan of %: %', name, query;
    END IF;
    INSERT INTO samples
    VALUES (name,
            (node ->> 'Lookups')::float8, (node ->> 'Lookup Keys')::float8,
            (node ->> 'Index Searches')::float8, (node ->> 'Build Rows')::float8,
            (node ->> 'Probe Rows')::float8,
            (node ->> 'Actual Rows')::float8 * (node ->> 'Actual Loops')::float8,
            published, hashed,
            pg_temp.estimate(estimate, 'batches'), pg_temp.estimate(estimate, 'keys'),
            pg_temp.estimate(estimate, 'descents'), pg_temp.estimate(estimate, 'rows'),
            node_ms, outer_ms,
            pg_temp.fastest(query, repetitions), pg_temp.fastest(outer_query, repetitions));
    PERFORM set_config('tessera.enable', 'on', false);
    PERFORM set_config('enable_hashjoin', 'on', false);
    PERFORM set_config('enable_mergejoin', 'on', false);
    PERFORM set_config('enable_memoize', 'on', false);
    PERFORM set_config('enable_bitmapscan', 'on', false);
END
$function$;

DO $do$
DECLARE
    repetitions integer := current_setting('bench.repetitions')::integer;
    join_sql text := 'SELECT count(*), sum(l.v) FROM bench_lookup_outer o JOIN bench_lookup l ON ';
BEGIN
    -- Keys in the index's order, ten rows of it apart: few descents.
    PERFORM pg_temp.sample('ordered', join_sql || 'l.id = o.k WHERE o.id <= 5000',
        'SELECT count(o.k) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    PERFORM pg_temp.sample('ordered_20k', join_sql || 'l.id = o.k WHERE o.id <= 20000',
        'SELECT count(o.k) FROM bench_lookup_outer o WHERE o.id <= 20000', repetitions);
    -- The same keys shuffled: a descent a key.
    PERFORM pg_temp.sample('shuffled', join_sql || 'l.id = o.ks WHERE o.id <= 5000',
        'SELECT count(o.ks) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    PERFORM pg_temp.sample('shuffled_20k', join_sql || 'l.id = o.ks WHERE o.id <= 20000',
        'SELECT count(o.ks) FROM bench_lookup_outer o WHERE o.id <= 20000', repetitions);
    -- Keys 120 rows of the index apart, in batches of their own.
    PERFORM pg_temp.sample('stride', join_sql || 'l.id = o.id * 120 WHERE o.id <= 5000',
        'SELECT count(o.id * 120) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    -- A row in a hundred: a batch of one key; and four in a hundred.
    PERFORM pg_temp.sample('sparse', join_sql || 'l.id = o.k WHERE o.sel = 7',
        'SELECT count(o.k) FROM bench_lookup_outer o WHERE o.sel = 7', repetitions);
    PERFORM pg_temp.sample('sparse_shuffled', join_sql || 'l.id = o.ks WHERE o.sel = 7',
        'SELECT count(o.ks) FROM bench_lookup_outer o WHERE o.sel = 7', repetitions);
    PERFORM pg_temp.sample('sparse_4', join_sql || 'l.id = o.k WHERE o.sel < 4',
        'SELECT count(o.k) FROM bench_lookup_outer o WHERE o.sel < 4', repetitions);
    -- Four rows a key, the keys next to each other.
    PERFORM pg_temp.sample('four', join_sql || 'l.o4 = o.id WHERE o.id <= 1250',
        'SELECT count(o.id) FROM bench_lookup_outer o WHERE o.id <= 1250', repetitions);
    PERFORM pg_temp.sample('four_5000', join_sql || 'l.o4 = o.id WHERE o.id <= 5000',
        'SELECT count(o.id) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    -- An index out of the table's order: a page a row.
    PERFORM pg_temp.sample('scattered', join_sql || 'l.s = o.k WHERE o.id <= 5000',
        'SELECT count(o.k) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    -- Text and date keys.
    PERFORM pg_temp.sample('text', join_sql || 'l.tk = o.kt WHERE o.id <= 5000',
        'SELECT count(o.kt) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions, 1);
    PERFORM pg_temp.sample('date', join_sql || 'l.dk = o.kd WHERE o.id <= 5000',
        'SELECT count(o.kd) FROM bench_lookup_outer o WHERE o.id <= 5000', repetitions);
    -- The scans that give the scan model's unit.
    INSERT INTO scans VALUES
        ('SELECT count(id) FROM bench_lookup', pg_temp.fastest('SELECT count(id) FROM bench_lookup', repetitions)),
        ('SELECT count(id) FROM bench_lookup_outer',
         pg_temp.fastest('SELECT count(id) FROM bench_lookup_outer', repetitions));
END
$do$;

/* The solution of a x = b, a of n rows in one array by rows (Gauss-Jordan). */
CREATE FUNCTION pg_temp.solve(a float8[], b float8[], n integer)
RETURNS float8[]
LANGUAGE plpgsql
AS $function$
DECLARE
    m float8[] := a;
    v float8[] := b;
    r integer;
    pivot float8;
    factor float8;
    swap float8;
BEGIN
    FOR c IN 1..n LOOP
        r := c;
        FOR k IN c + 1..n LOOP
            IF abs(m[(k - 1) * n + c]) > abs(m[(r - 1) * n + c]) THEN
                r := k;
            END IF;
        END LOOP;
        IF r <> c THEN
            FOR k IN 1..n LOOP
                swap := m[(r - 1) * n + k];
                m[(r - 1) * n + k] := m[(c - 1) * n + k];
                m[(c - 1) * n + k] := swap;
            END LOOP;
            swap := v[r];
            v[r] := v[c];
            v[c] := swap;
        END IF;
        pivot := m[(c - 1) * n + c];
        FOR k IN 1..n LOOP
            m[(c - 1) * n + k] := m[(c - 1) * n + k] / pivot;
        END LOOP;
        v[c] := v[c] / pivot;
        FOR r IN 1..n LOOP
            factor := m[(r - 1) * n + c];
            IF r <> c AND factor <> 0 THEN
                FOR k IN 1..n LOOP
                    m[(r - 1) * n + k] := m[(r - 1) * n + k] - factor * m[(c - 1) * n + k];
                END LOOP;
                v[r] := v[r] - factor * v[c];
            END IF;
        END LOOP;
    END LOOP;
    RETURN v;
END
$function$;

\o summary.txt
-- A unit of the scan model in milliseconds: the full scans of the two
-- tables against the model's time of them.
CREATE TEMP VIEW model_unit AS
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY s.milliseconds /
           (c.relpages * current_setting('tessera.scan_page_cost')::float8 +
            c.reltuples * current_setting('tessera.scan_tuple_cost')::float8)) AS unit_ms
FROM scans AS s
JOIN pg_class AS c ON s.query = format('SELECT count(id) FROM %s', c.relname);
SELECT name, round((core_ms - core_outer_ms)::numeric, 3) AS core_join_ms,
       round((node_ms - outer_ms)::numeric, 3) AS node_join_ms,
       round(((node_ms - outer_ms) / (core_ms - core_outer_ms))::numeric, 2) AS node_over_core
FROM samples ORDER BY name;
-- The planner's counts against the plan's.
SELECT name, lookups, round(est_batches::numeric) AS est_batches, keys, round(est_keys::numeric) AS est_keys,
       descents, round(est_descents::numeric) AS est_descents, fetched, round(est_rows::numeric) AS est_rows
FROM samples ORDER BY name;
-- The node's own time less the scan model's time of the rows fetched
-- through the index (tessera.index_tuple_cost) and the hash join's model
-- of its work over the batches' tables (the rows built, a text key
-- hashed on both sides, the outer rows probed, the pairs and the integer
-- each gathers, the batches the aggregate above received), fitted as
-- batch × lookups + descent × descents + key × other keys; the core's
-- nested loop's time, as key
-- × keys + descent × the same descents + row × rows fetched: keys near
-- each other in the index spare its descents too. Least squares through
-- zero over every sample.
CREATE TEMP VIEW own AS
SELECT s.*,
       s.node_ms - s.outer_ms -
       (s.fetched * (current_setting('tessera.index_tuple_cost')::float8 +
                     current_setting('tessera.join_build_cost')::float8) +
        (s.fetched + s.probed) * s.hashed * current_setting('tessera.join_hashed_key_cost')::float8 +
        s.probed * current_setting('tessera.join_probe_cost')::float8 +
        s.pairs * (current_setting('tessera.join_pair_cost')::float8 +
                   current_setting('tessera.join_gather_cost')::float8) +
        s.published * current_setting('tessera.join_batch_cost')::float8) * u.unit_ms AS y,
       s.node_ms - s.outer_ms - (s.node_ms - s.outer_ms -
       (s.fetched * (current_setting('tessera.index_tuple_cost')::float8 +
                     current_setting('tessera.join_build_cost')::float8) +
        (s.fetched + s.probed) * s.hashed * current_setting('tessera.join_hashed_key_cost')::float8 +
        s.probed * current_setting('tessera.join_probe_cost')::float8 +
        s.pairs * (current_setting('tessera.join_pair_cost')::float8 +
                   current_setting('tessera.join_gather_cost')::float8) +
        s.published * current_setting('tessera.join_batch_cost')::float8) * u.unit_ms) AS hash_ms,
       ARRAY[s.lookups, s.descents, s.keys - s.descents] AS x,
       s.core_ms - s.core_outer_ms AS loop_y,
       ARRAY[s.keys, s.descents, s.fetched] AS loop_x
FROM samples AS s, model_unit AS u;
CREATE TEMP VIEW fit AS
WITH a AS (
    SELECT array_agg(v ORDER BY i, j) AS a
    FROM (SELECT i, j, sum(x[i] * x[j]) AS v
          FROM own, generate_series(1, 3) AS i, generate_series(1, 3) AS j
          GROUP BY i, j) AS sums
), b AS (
    SELECT array_agg(v ORDER BY i) AS b
    FROM (SELECT i, sum(x[i] * y) AS v FROM own, generate_series(1, 3) AS i GROUP BY i) AS sums
), la AS (
    SELECT array_agg(v ORDER BY i, j) AS a
    FROM (SELECT i, j, sum(loop_x[i] * loop_x[j]) AS v
          FROM own, generate_series(1, 3) AS i, generate_series(1, 3) AS j
          GROUP BY i, j) AS sums
), lb AS (
    SELECT array_agg(v ORDER BY i) AS b
    FROM (SELECT i, sum(loop_x[i] * loop_y) AS v FROM own, generate_series(1, 3) AS i GROUP BY i) AS sums
)
SELECT pg_temp.solve(a.a, b.b, 3) AS c, pg_temp.solve(la.a, lb.b, 3) AS l FROM a, b, la, lb;
SELECT round((f.c[1] / u.unit_ms)::numeric, 3) AS join_lookup_batch_cost,
       round((f.c[2] / u.unit_ms)::numeric, 3) AS join_lookup_descent_cost,
       round((f.c[3] / u.unit_ms)::numeric, 3) AS join_lookup_key_cost,
       round((f.l[1] / u.unit_ms)::numeric, 3) AS join_loop_key_cost,
       round((f.l[2] / u.unit_ms)::numeric, 3) AS join_loop_descent_cost,
       round((f.l[3] / u.unit_ms)::numeric, 4) AS join_loop_row_cost,
       round((u.unit_ms * 1e6)::numeric, 1) AS unit_ns
FROM fit AS f, model_unit AS u;
-- Each sample against the model: the node's time and the nested loop's,
-- and whether the model chooses as the times do.
CREATE TEMP VIEW prediction AS
SELECT o.name, o.node_ms - o.outer_ms AS node_ms, o.loop_y AS loop_ms,
       o.hash_ms + f.c[1] * o.x[1] + f.c[2] * o.x[2] + f.c[3] * o.x[3] AS node_predicted,
       f.l[1] * o.loop_x[1] + f.l[2] * o.loop_x[2] + f.l[3] * o.loop_x[3] AS loop_predicted,
       o.hash_ms + f.c[1] * o.est_batches + f.c[2] * o.est_descents +
           f.c[3] * (o.est_keys - o.est_descents) AS node_planned,
       f.l[1] * o.est_keys + f.l[2] * o.est_descents + f.l[3] * o.est_rows AS loop_planned
FROM own AS o, fit AS f;
SELECT name, round(node_ms::numeric, 3) AS node_ms, round(node_predicted::numeric, 3) AS node_predicted,
       round(((node_predicted - node_ms) / node_ms * 100)::numeric) AS node_error_pct,
       round(loop_ms::numeric, 3) AS loop_ms, round(loop_predicted::numeric, 3) AS loop_predicted,
       round(((loop_predicted - loop_ms) / loop_ms * 100)::numeric) AS loop_error_pct,
       CASE WHEN (node_ms < loop_ms) = (node_predicted < loop_predicted) THEN 'right' ELSE 'WRONG' END AS choice,
       -- The planner's: its counts, and the lookup within 0.8 of the loop.
       round((node_planned / loop_planned)::numeric, 2) AS planned_ratio,
       CASE WHEN node_planned <= 0.8 * loop_planned THEN 'lookup' ELSE 'loop' END AS planned,
       CASE WHEN node_ms < loop_ms THEN 'lookup' ELSE 'loop' END AS faster
FROM prediction ORDER BY name;
\o
