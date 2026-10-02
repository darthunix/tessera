-- The joincost family: the times of TessHashJoin, from which the planner's
-- model of it is fitted (tessera.join_build_cost and the rest,
-- docs/nodes.md). Every sample is a hash join of bench_fact, or bench_tfact
-- for keys of text and numeric, as the outer side against an inner side
-- of 10 000 to 2 000 000 rows, run twice: over every outer row and over
-- the half with f1 <= 1 000 000, so that the rows of the two sides and
-- the pairs vary apart. The node's own time is the query's less its
-- children's: the same scans timed alone with the same columns read, by
-- the cheapest aggregate. The samples vary the inner side's size, the
-- keys' types, the inner columns kept (integers and a text), four records
-- a key, a residual clause, semi, anti and left joins, the Bloom filter
-- (the selective joins with and without it), spilling at a small work_mem
-- and a shared table with two workers. Each time is the minimum of its
-- runs. The summary fits the times and prints the parameters' values in
-- the units of the scan model, a page of the full scan being 1, and each
-- sample against its prediction.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 7
\endif
SET jit = off;
SET max_parallel_workers_per_gather = 0;
SET work_mem = '256MB';
SET tessera.enable = on;
-- The hash join is the node calibrated; the others stay out of the way.
SET enable_nestloop = off;
SET enable_mergejoin = off;
SELECT set_config('bench.repetitions', :'repetitions', false);
SELECT set_config('bench.bloom_ratio', current_setting('tessera.join_bloom_ratio'), false),
       set_config('bench.parallel_setup_cost', current_setting('tessera.scan_parallel_setup_cost'), false),
       set_config('bench.worker_page_cost', current_setting('tessera.scan_worker_page_cost'), false);

/*
 * A sample: the query, the join's rows as its plan counted them (the
 * inner rows built, the outer rows probed, the records matched, the pairs
 * it returned, the batches its parent received), what the node did (a Bloom filter,
 * spilling, a shared table, workers), the inner columns kept, and the
 * times: the query's, its outer and inner children's alone, and the
 * difference, the node's own.
 */
CREATE TEMP TABLE samples
(
    name text,
    kind text,
    half boolean,
    inner_rel text,
    build_rows float8,
    probe_rows float8,
    matches float8,
    pairs float8,
    out_batches float8,
    chunks float8,
    spilled_chunks float8,
    hash_mem_bytes float8,
    hashed_keys integer,
    payload_ints integer,
    payload_texts integer,
    bloom boolean,
    shared boolean,
    workers integer,
    own_cost float8,
    join_ms float8,
    outer_ms float8,
    inner_ms float8,
    own_ms float8
);

/* The children's scans, each timed once per setting (the tag). */
CREATE TEMP TABLE scans
(
    tag text,
    query text,
    milliseconds float8,
    PRIMARY KEY (tag, query)
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

/* A child's scan timed, once per tag and query. */
CREATE FUNCTION pg_temp.reference(tag text, query text, repetitions integer)
RETURNS float8
LANGUAGE plpgsql
AS $function$
DECLARE
    ms float8;
BEGIN
    SELECT milliseconds INTO ms FROM scans AS s WHERE s.tag = reference.tag AND s.query = reference.query;
    IF ms IS NULL THEN
        ms := pg_temp.fastest(query, repetitions);
        INSERT INTO scans VALUES (tag, query, ms);
    END IF;
    RETURN ms;
END
$function$;

/* The first hash join of a plan, depth first: the node's, or the core's. */
CREATE FUNCTION pg_temp.find_join(node jsonb, core boolean)
RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    child jsonb;
    found jsonb;
BEGIN
    IF (NOT core AND node ->> 'Custom Plan Provider' = 'TessHashJoin') OR
       (core AND node ->> 'Node Type' = 'Hash Join') THEN
        RETURN node;
    END IF;
    FOR child IN SELECT * FROM jsonb_array_elements(coalesce(node -> 'Plans', '[]'::jsonb)) LOOP
        found := pg_temp.find_join(child, core);
        IF found IS NOT NULL THEN
            RETURN found;
        END IF;
    END LOOP;
    RETURN NULL;
END
$function$;

/* The parent of the first TessHashJoin of a plan, depth first. */
CREATE FUNCTION pg_temp.find_join_parent(node jsonb)
RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    child jsonb;
    found jsonb;
BEGIN
    FOR child IN SELECT * FROM jsonb_array_elements(coalesce(node -> 'Plans', '[]'::jsonb)) LOOP
        IF child ->> 'Custom Plan Provider' = 'TessHashJoin' THEN
            RETURN node;
        END IF;
        found := pg_temp.find_join_parent(child);
        IF found IS NOT NULL THEN
            RETURN found;
        END IF;
    END LOOP;
    RETURN NULL;
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

/*
 * A sample over every outer row and over the half: {half} in the queries
 * stands for the outer side's clause, nothing or "AND f1 <= 1000000" of
 * the outer alias. The query is explained once for the join's counts and
 * its own cost (the join's less its children's), and timed; the
 * children's scans are timed once each. With the core kind the sample is
 * the core's hash join with Tessera off, its inner rows the Hash node's
 * child's.
 */
CREATE FUNCTION pg_temp.sample(name text, kind text, query text, outer_ref text, inner_ref text,
                               half_clause text, hashed_keys integer, payload_ints integer,
                               payload_texts integer, tag text, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    core boolean := kind = 'core';
    half boolean;
    sql text;
    outer_sql text;
    plan jsonb;
    node jsonb;
    parent jsonb;
    outer_node jsonb;
    inner_node jsonb;
    inner_scan jsonb;
BEGIN
    PERFORM set_config('tessera.enable', CASE WHEN core THEN 'off' ELSE 'on' END, false);
    FOREACH half IN ARRAY ARRAY[false, true] LOOP
        sql := replace(query, '{half}', CASE WHEN half THEN half_clause ELSE '' END);
        outer_sql := replace(outer_ref, '{half}', CASE WHEN half THEN half_clause ELSE '' END);
        EXECUTE 'EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || sql
            INTO plan;
        node := pg_temp.find_join(plan -> 0 -> 'Plan', core);
        IF node IS NULL THEN
            RAISE EXCEPTION 'no hash join in the plan of %: %', name, sql;
        END IF;
        parent := pg_temp.find_join_parent(plan -> 0 -> 'Plan');
        outer_node := node -> 'Plans' -> 0;
        inner_node := node -> 'Plans' -> 1;
        /* The core's inner child is the Hash node over the scan. */
        inner_scan := CASE WHEN core THEN inner_node -> 'Plans' -> 0 ELSE inner_node END;
        WHILE inner_scan ->> 'Relation Name' IS NULL AND inner_scan -> 'Plans' -> 0 IS NOT NULL LOOP
            inner_scan := inner_scan -> 'Plans' -> 0;
        END LOOP;
        INSERT INTO samples
        VALUES (name, kind, half, inner_scan ->> 'Relation Name',
                CASE WHEN core THEN (inner_node -> 'Plans' -> 0 ->> 'Actual Rows')::float8
                     ELSE (node ->> 'Build Rows')::float8 END,
                CASE WHEN core THEN (outer_node ->> 'Actual Rows')::float8
                     ELSE (node ->> 'Probe Rows')::float8 END,
                (node ->> 'Matches')::float8,
                (node ->> 'Actual Rows')::float8 * (node ->> 'Actual Loops')::float8,
                (parent ->> 'Input Batches')::float8,
                (node ->> 'Chunks')::float8, coalesce((node ->> 'Spilled Chunks')::float8, 0),
                (SELECT s.setting::float8 * 1024 FROM pg_settings AS s WHERE s.name = 'work_mem') *
                    current_setting('hash_mem_multiplier')::float8,
                hashed_keys, payload_ints, payload_texts,
                coalesce((node ->> 'Bloom Filters')::integer, 0) > 0,
                coalesce((node ->> 'Shared Table')::boolean, false),
                coalesce(pg_temp.find_key(plan -> 0 -> 'Plan', 'Workers Launched')::integer, 0),
                (node ->> 'Total Cost')::float8 - (outer_node ->> 'Total Cost')::float8 -
                    (CASE WHEN core THEN inner_node -> 'Plans' -> 0 ELSE inner_node END ->> 'Total Cost')::float8,
                pg_temp.fastest(sql, repetitions),
                pg_temp.reference(tag, outer_sql, repetitions),
                pg_temp.reference(tag, inner_ref, repetitions),
                0);
    END LOOP;
    PERFORM set_config('tessera.enable', 'on', false);
END
$function$;

DO $do$
DECLARE
    repetitions integer := current_setting('bench.repetitions')::integer;
    fact text := 'SELECT count(fk) FROM bench_fact f WHERE true {half}';
    fact_half text := 'AND f.f1 <= 1000000';
    tfact_half text := 'AND t.v <= 1000000';
    work_mem text;
BEGIN
    -- The base: integer keys, nothing kept, no Bloom filter, over inner
    -- sides of 10 000 to 2 000 000 rows, and a join that matches nothing.
    PERFORM set_config('tessera.join_bloom_ratio', '0', false);
    PERFORM pg_temp.sample('dim_10k', 'size',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE d.id <= 10000 {half}',
        fact, 'SELECT count(id) FROM bench_dim WHERE id <= 10000', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('dim_100k', 'size',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('sort_1200k', 'size',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE s.k4 < 1200000 {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort WHERE k4 < 1200000', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('sort_2m', 'size',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE true {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('miss', 'size',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_miss = d.id WHERE true {half}',
        'SELECT count(fk_miss) FROM bench_fact f WHERE true {half}',
        'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    -- The core's hash join of the base's queries, with Tessera off: its
    -- time per unit of its own cost is the price the node's time is
    -- converted at, since the planner weighs the two against each other.
    PERFORM pg_temp.sample('dim_10k', 'core',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE d.id <= 10000 {half}',
        fact, 'SELECT count(id) FROM bench_dim WHERE id <= 10000', fact_half, 0, 0, 0, 'core', repetitions);
    PERFORM pg_temp.sample('dim_100k', 'core',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'core', repetitions);
    PERFORM pg_temp.sample('sort_1200k', 'core',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE s.k4 < 1200000 {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort WHERE k4 < 1200000', fact_half, 0, 0, 0, 'core', repetitions);
    PERFORM pg_temp.sample('sort_2m', 'core',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE true {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort', fact_half, 0, 0, 0, 'core', repetitions);
    PERFORM pg_temp.sample('miss', 'core',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_miss = d.id WHERE true {half}',
        'SELECT count(fk_miss) FROM bench_fact f WHERE true {half}',
        'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'core', repetitions);
    -- The keys: int8, an int4 against an int8, two keys, text and numeric
    -- through their types' hashes.
    PERFORM pg_temp.sample('int8', 'keys',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_big = d.big WHERE true {half}',
        'SELECT count(fk_big) FROM bench_fact f WHERE true {half}',
        'SELECT count(big) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('mixed', 'keys',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id8 WHERE true {half}',
        fact, 'SELECT count(id8) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('two_keys', 'keys',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id AND f.fk8 = d.id8 WHERE true {half}',
        'SELECT count(fk), count(fk8) FROM bench_fact f WHERE true {half}',
        'SELECT count(id), count(id8) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('text', 'keys',
        'SELECT count(*) FROM bench_tfact t JOIN bench_tdim d ON t.k = d.k WHERE true {half}',
        'SELECT count(k) FROM bench_tfact t WHERE true {half}',
        'SELECT count(k) FROM bench_tdim', tfact_half, 1, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('numeric', 'keys',
        'SELECT count(*) FROM bench_tfact t JOIN bench_tdim d ON t.n = d.n WHERE true {half}',
        'SELECT count(n) FROM bench_tfact t WHERE true {half}',
        'SELECT count(n) FROM bench_tdim', tfact_half, 1, 0, 0, 'serial', repetitions);
    -- The inner columns kept: one integer, two, and a text of bench_mixed
    -- (500 000 rows); one integer kept where no probe row has a pair.
    PERFORM pg_temp.sample('int_1', 'payload',
        'SELECT sum(d.d1) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id), sum(d1) FROM bench_dim', fact_half, 0, 1, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('int_2', 'payload',
        'SELECT sum(d.d1), sum(d.d2) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id), sum(d1), sum(d2) FROM bench_dim', fact_half, 0, 2, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('text_1', 'payload',
        'SELECT sum(length(m.b)) FROM bench_fact f JOIN bench_mixed m ON f.fk = m.a WHERE true {half}',
        fact, 'SELECT count(a), sum(length(b)) FROM bench_mixed', fact_half, 0, 0, 1, 'serial', repetitions);
    PERFORM pg_temp.sample('int_1_miss', 'payload',
        'SELECT sum(d.d1) FROM bench_fact f JOIN bench_dim d ON f.fk_miss = d.id WHERE true {half}',
        'SELECT count(fk_miss) FROM bench_fact f WHERE true {half}',
        'SELECT count(id), sum(d1) FROM bench_dim', fact_half, 0, 1, 0, 'serial', repetitions);
    -- Four records a key, a quarter of the probe rows with a pair: the
    -- rounds and the compact batches, without and with a column kept.
    PERFORM pg_temp.sample('dup', 'dup',
        'SELECT count(*) FROM bench_fact f JOIN bench_dup u ON f.fk = u.k WHERE true {half}',
        fact, 'SELECT count(k) FROM bench_dup', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('dup_col', 'dup',
        'SELECT sum(u.v) FROM bench_fact f JOIN bench_dup u ON f.fk = u.k WHERE true {half}',
        fact, 'SELECT count(k), sum(v) FROM bench_dup', fact_half, 0, 1, 0, 'serial', repetitions);
    -- A residual clause over columns of both sides, in batches.
    PERFORM pg_temp.sample('residual', 'residual',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id AND f.f1 > d.d1 * 10 WHERE true {half}',
        'SELECT count(fk), count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(id), count(d1) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    -- Semi, anti and left joins.
    PERFORM pg_temp.sample('semi', 'kinds',
        'SELECT count(*) FROM bench_fact f WHERE EXISTS (SELECT 1 FROM bench_dim d WHERE d.id = f.fk) {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('anti', 'kinds',
        'SELECT count(*) FROM bench_fact f WHERE NOT EXISTS (SELECT 1 FROM bench_dim d WHERE d.id = f.fk) {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('left', 'kinds',
        'SELECT count(*), count(d.id) FROM bench_fact f LEFT JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 1, 0, 'serial', repetitions);
    -- The Bloom filter: the selective joins again with it allowed.
    PERFORM set_config('tessera.join_bloom_ratio', current_setting('bench.bloom_ratio'), false);
    PERFORM pg_temp.sample('dim_10k', 'bloom',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE d.id <= 10000 {half}',
        fact, 'SELECT count(id) FROM bench_dim WHERE id <= 10000', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('miss', 'bloom',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk_miss = d.id WHERE true {half}',
        'SELECT count(fk_miss) FROM bench_fact f WHERE true {half}',
        'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('dup', 'bloom',
        'SELECT count(*) FROM bench_fact f JOIN bench_dup u ON f.fk = u.k WHERE true {half}',
        fact, 'SELECT count(k) FROM bench_dup', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM set_config('tessera.join_bloom_ratio', '0', false);
    -- Spilling: the two largest inner sides at a work_mem of 4 and 16 MB.
    work_mem := current_setting('work_mem');
    PERFORM set_config('work_mem', '4MB', false);
    PERFORM pg_temp.sample('sort_2m_4mb', 'spill',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE true {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM pg_temp.sample('sort_1200k_4mb', 'spill',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE s.k4 < 1200000 {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort WHERE k4 < 1200000', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM set_config('work_mem', '16MB', false);
    PERFORM pg_temp.sample('sort_2m_16mb', 'spill',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE true {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort', fact_half, 0, 0, 0, 'serial', repetitions);
    PERFORM set_config('work_mem', work_mem, false);
    -- A shared table with two workers, which the core's costs and the
    -- node's model are set to allow.
    PERFORM set_config('max_parallel_workers_per_gather', '2', false);
    PERFORM set_config('parallel_setup_cost', '0', false);
    PERFORM set_config('parallel_tuple_cost', '0', false);
    PERFORM set_config('min_parallel_table_scan_size', '0', false);
    PERFORM set_config('tessera.scan_parallel_setup_cost', '0', false);
    PERFORM set_config('tessera.scan_worker_page_cost', '0', false);
    PERFORM pg_temp.sample('dim_100k', 'parallel',
        'SELECT count(*) FROM bench_fact f JOIN bench_dim d ON f.fk = d.id WHERE true {half}',
        fact, 'SELECT count(id) FROM bench_dim', fact_half, 0, 0, 0, 'parallel', repetitions);
    PERFORM pg_temp.sample('sort_2m', 'parallel',
        'SELECT count(*) FROM bench_fact f JOIN bench_sort s ON f.f1 = s.k4 WHERE true {half}',
        'SELECT count(f1) FROM bench_fact f WHERE true {half}',
        'SELECT count(k4) FROM bench_sort', fact_half, 0, 0, 0, 'parallel', repetitions);
    PERFORM set_config('max_parallel_workers_per_gather', '0', false);
    PERFORM set_config('parallel_setup_cost', '1000', false);
    PERFORM set_config('parallel_tuple_cost', '0.1', false);
    PERFORM set_config('min_parallel_table_scan_size', '8MB', false);
    PERFORM set_config('tessera.scan_parallel_setup_cost',
                       current_setting('bench.parallel_setup_cost'), false);
    PERFORM set_config('tessera.scan_worker_page_cost',
                       current_setting('bench.worker_page_cost'), false);
    PERFORM set_config('tessera.join_bloom_ratio', current_setting('bench.bloom_ratio'), false);
END
$do$;
RESET enable_nestloop;
RESET enable_mergejoin;
UPDATE samples SET own_ms = join_ms - outer_ms - inner_ms;

\copy samples TO 'timings.csv' CSV HEADER
\copy scans TO 'scans.csv' CSV HEADER

\o summary.txt
-- The samples: the join's counts, its time, the children's and its own.
SELECT name, kind, half, inner_rel, build_rows AS build, probe_rows AS probe, matches, pairs,
       bloom, spilled_chunks > 0 AS spilled, shared, workers, round(own_cost::numeric, 1) AS own_cost,
       round(join_ms::numeric, 2) AS join_ms, round(outer_ms::numeric, 2) AS outer_ms,
       round(inner_ms::numeric, 2) AS inner_ms, round(own_ms::numeric, 2) AS own_ms
FROM samples ORDER BY kind, name, half;

-- The unit of the scan model: the time of a page as tessera.scan_page_cost
-- and scan_tuple_cost give it, the median over the children's full scans
-- without a clause.
CREATE TEMP VIEW model_unit AS
WITH tables AS (
    SELECT c.relname, c.relpages, c.reltuples, a.attname
    FROM pg_class AS c JOIN pg_attribute AS a ON a.attrelid = c.oid AND a.attnum = 1
    WHERE c.relname LIKE 'bench\_%' AND c.relkind = 'r'
)
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY s.milliseconds /
           (t.relpages * current_setting('tessera.scan_page_cost')::float8 +
            t.reltuples * current_setting('tessera.scan_tuple_cost')::float8)) AS unit_ms
FROM scans AS s
JOIN tables AS t ON s.query IN (format('SELECT count(%s) FROM %s', t.attname, t.relname),
                                format('SELECT count(%s) FROM %s f WHERE true ', t.attname, t.relname),
                                format('SELECT count(%s) FROM %s t WHERE true ', t.attname, t.relname))
WHERE s.tag = 'serial';

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

-- The base: own = build × inner rows + probe × outer rows + pair × pairs
-- + batch × batches out (a round published for an outer batch with a
-- pair, which the parent pays for however few rows it selects), least
-- squares through zero over the samples with integer keys and nothing
-- kept, serial, without a Bloom filter or spilling.
CREATE TEMP VIEW base_samples AS
SELECT * FROM samples WHERE kind = 'size';
CREATE TEMP VIEW base_fit AS
WITH x AS (
    SELECT ARRAY[build_rows, probe_rows, pairs, out_batches] AS r, own_ms AS y FROM base_samples
), a AS (
    SELECT array_agg(s ORDER BY i, j) AS a
    FROM (SELECT i, j, sum(r[i] * r[j]) AS s
          FROM x, generate_series(1, 4) AS i, generate_series(1, 4) AS j
          GROUP BY i, j) AS sums
), b AS (
    SELECT array_agg(s ORDER BY i) AS b
    FROM (SELECT i, sum(r[i] * y) AS s FROM x, generate_series(1, 4) AS i GROUP BY i) AS sums
), c AS (
    SELECT pg_temp.solve(a.a, b.b, 4) AS c FROM a, b
)
SELECT c[1] AS build_ms, c[2] AS probe_ms, c[3] AS pair_ms, c[4] AS batch_ms FROM c;
SELECT round((build_ms * 1e6)::numeric, 2) AS build_ns, round((probe_ms * 1e6)::numeric, 2) AS probe_ns,
       round((pair_ms * 1e6)::numeric, 2) AS pair_ns, round((batch_ms * 1e6)::numeric, 1) AS batch_ns
FROM base_fit;
-- The base samples against the fit, and by inner side: the probe row's
-- time (pairs included) from the two outer sizes, and the build's from
-- what is left, which shows whether the table's size changes them.
SELECT s.name, s.half, s.build_rows AS build, s.probe_rows AS probe, s.pairs, s.out_batches AS batches,
       round(s.own_ms::numeric, 2) AS own_ms,
       round((f.build_ms * s.build_rows + f.probe_ms * s.probe_rows + f.pair_ms * s.pairs +
              f.batch_ms * s.out_batches)::numeric, 2) AS predicted_ms
FROM base_samples AS s, base_fit AS f
ORDER BY s.build_rows, s.half;
SELECT a.name, a.build_rows AS build,
       round(((a.own_ms - b.own_ms) / nullif(a.probe_rows - b.probe_rows, 0) * 1e6)::numeric, 2) AS per_probe_row_ns,
       round(((a.own_ms - (a.own_ms - b.own_ms) / nullif(a.probe_rows - b.probe_rows, 0) * a.probe_rows) /
              a.build_rows * 1e6)::numeric, 2) AS per_build_row_ns
FROM samples AS a JOIN samples AS b ON a.name = b.name AND a.kind = b.kind
WHERE a.kind IN ('size', 'spill', 'parallel') AND NOT a.half AND b.half AND a.build_rows = b.build_rows
ORDER BY a.kind, a.build_rows;

-- Every other sample's excess over the base, per the rows of its own kind
-- of work: a kept integer per pair (the gather; the copy into the record
-- costs nothing a probe row without a pair shows, int_1_miss); a kept
-- text per value, copied into the table and gathered for a pair; a
-- hashed key per row of both sides; the rounds and compact batches of
-- four records a key per pair; the residual clause per record matched;
-- semi, anti and left per probe row; the Bloom filter against its twin
-- without; spilling per row of both sides in the chunks spilled; the
-- shared table against its serial twin.
CREATE TEMP VIEW excess AS
SELECT s.*,
       s.own_ms - (f.build_ms * s.build_rows + f.probe_ms * s.probe_rows + f.pair_ms * s.pairs +
                   f.batch_ms * s.out_batches) AS excess_ms
FROM samples AS s, base_fit AS f;
CREATE TEMP VIEW gather_fit AS
SELECT sum(excess_ms * pairs * payload_ints) / sum((pairs * payload_ints) ^ 2) AS gather_ms
FROM excess WHERE name IN ('int_1', 'int_2');
CREATE TEMP VIEW text_fit AS
SELECT sum(e.excess_ms * t.values) / sum(t.values ^ 2) AS text_ms
FROM excess AS e,
     LATERAL (SELECT e.build_rows * (1 - st.null_frac) + e.pairs AS values
              FROM pg_stats AS st WHERE st.tablename = 'bench_mixed' AND st.attname = 'b') AS t
WHERE e.name = 'text_1';
CREATE TEMP VIEW hashed_fit AS
SELECT sum(excess_ms * (build_rows + probe_rows)) / sum((build_rows + probe_rows) ^ 2) AS hashed_ms
FROM excess WHERE kind = 'keys' AND hashed_keys > 0;
CREATE TEMP VIEW compact_fit AS
SELECT sum(excess_ms * pairs) / sum(pairs ^ 2) AS compact_ms
FROM excess WHERE kind = 'dup' AND name = 'dup';
-- Spilling: once the table outgrows hash_mem, every row of both sides is
-- split into partitions or routed to one, and the samples' excess grows
-- with the rows, not with the share of the table past hash_mem (a fit
-- with the share as well gave it a negative price): one price a row of
-- both sides. The share, as the planner expects it from a record of 16
-- bytes, 8 a key, 8 for the NULL bits of the columns kept and 8 a
-- column, and 8 of the index a record, is printed for the samples.
CREATE TEMP VIEW spill_share AS
SELECT e.*,
       CASE WHEN e.spilled_chunks > 0 THEN greatest(0, least(1, 1 - e.hash_mem_bytes /
           (e.build_rows * (16 + 8 * greatest(e.hashed_keys, 1) + 8 * (1 + e.payload_ints + e.payload_texts) + 8))))
       ELSE 0 END AS share
FROM excess AS e;
CREATE TEMP VIEW spill_fit AS
SELECT sum(excess_ms * (build_rows + probe_rows)) / sum((build_rows + probe_rows) ^ 2) AS spill_ms
FROM spill_share
WHERE kind = 'spill' AND spilled_chunks > 0;
CREATE TEMP VIEW bloom_pairs AS
SELECT b.name, b.half, b.build_rows, b.probe_rows, b.matches, b.pairs,
       b.own_ms AS bloom_ms, w.own_ms AS without_ms, b.own_ms - w.own_ms AS delta_ms
FROM samples AS b
JOIN samples AS w ON w.name = b.name AND w.half = b.half AND w.kind IN ('size', 'dup')
WHERE b.kind = 'bloom' AND b.bloom;
-- The filter: a test per outer row, and a row it rejects skips the probe
-- (the bits of 10 000 or 100 000 inner rows cost nothing the twins show):
-- test = (delta + (probe - pairs) × probe_ms) / probe over the twins
-- with a unique inner side, whose pairs are the probe rows with one.
CREATE TEMP VIEW bloom_fit AS
SELECT sum((p.delta_ms + (p.probe_rows - p.pairs) * f.probe_ms) * p.probe_rows) / sum(p.probe_rows ^ 2) AS test_ms
FROM bloom_pairs AS p, base_fit AS f
WHERE p.name <> 'dup';

SELECT round((g.gather_ms * 1e6)::numeric, 2) AS gather_ns, round((t.text_ms * 1e6)::numeric, 2) AS text_value_ns,
       round((h.hashed_ms * 1e6)::numeric, 2) AS hashed_key_ns, round((c.compact_ms * 1e6)::numeric, 2) AS compact_pair_ns,
       round((bl.test_ms * 1e6)::numeric, 2) AS bloom_test_ns, round((sp.spill_ms * 1e6)::numeric, 2) AS spill_row_ns
FROM gather_fit AS g, text_fit AS t, hashed_fit AS h, compact_fit AS c, bloom_fit AS bl, spill_fit AS sp;
-- Every sample's excess over the base per the rows of its work, to read
-- against the fits above and for the kinds the model has no parameter of.
SELECT e.kind, e.name, e.half, e.build_rows AS build, e.probe_rows AS probe, e.matches, e.pairs,
       e.out_batches AS batches, e.chunks, e.spilled_chunks AS spilled,
       round(e.own_ms::numeric, 2) AS own_ms, round(e.excess_ms::numeric, 2) AS excess_ms,
       round((e.excess_ms / nullif(e.build_rows + e.probe_rows, 0) * 1e6)::numeric, 2) AS per_side_row_ns,
       round((e.excess_ms / nullif(e.pairs, 0) * 1e6)::numeric, 2) AS per_pair_ns,
       round((e.excess_ms / nullif(e.matches, 0) * 1e6)::numeric, 2) AS per_match_ns,
       round((e.excess_ms / nullif(e.probe_rows, 0) * 1e6)::numeric, 2) AS per_probe_row_ns
FROM excess AS e
WHERE e.kind <> 'size'
ORDER BY e.kind, e.name, e.half;
SELECT name, half, build_rows AS build, probe_rows AS probe, pairs,
       round(bloom_ms::numeric, 2) AS bloom_ms, round(without_ms::numeric, 2) AS without_ms,
       round(delta_ms::numeric, 2) AS delta_ms
FROM bloom_pairs ORDER BY name, half;
SELECT name, half, build_rows AS build, probe_rows AS probe, round(hash_mem_bytes / 1048576) AS hash_mem_mb,
       round(share::numeric, 2) AS share_past, round(excess_ms::numeric, 2) AS excess_ms,
       round((excess_ms / (build_rows + probe_rows) * 1e6)::numeric, 2) AS per_row_ns
FROM spill_share WHERE kind = 'spill' ORDER BY name, half;
-- The parallel samples against their serial twins, and against the
-- model's time of one participant of three: the build, the probe, the
-- pairs and the batches each divided by three.
SELECT p.name, p.half, p.workers, p.shared, round(p.own_ms::numeric, 2) AS parallel_own_ms,
       round(s.own_ms::numeric, 2) AS serial_own_ms,
       round(((f.build_ms * p.build_rows + f.probe_ms * p.probe_rows + f.pair_ms * p.pairs +
               f.batch_ms * p.out_batches) / (p.workers + 1))::numeric, 2) AS participant_ms,
       round(p.join_ms::numeric, 2) AS parallel_ms, round(s.join_ms::numeric, 2) AS serial_ms
FROM samples AS p
JOIN samples AS s ON s.name = p.name AND s.half = p.half AND s.kind = 'size'
CROSS JOIN base_fit AS f
WHERE p.kind = 'parallel' ORDER BY p.name, p.half;

-- Every serial sample against the whole model: the base, the gathers
-- and texts kept, the hashed keys, the compact pairs of four records a
-- key, the residual clause at the filter's cost of a batch clause with
-- the gather of its inner column, the Bloom filter's tests in place of
-- the probes it saves, an anti join's pairs being the probe rows with a
-- record rather than the rows it returns, the spilled rows.
CREATE TEMP VIEW prediction AS
SELECT e.*,
       f.build_ms * e.build_rows + f.probe_ms * e.probe_rows + f.pair_ms * e.pairs + f.batch_ms * e.out_batches
       + g.gather_ms * e.pairs * e.payload_ints
       + t.text_ms * (e.build_rows * (1 - coalesce(nulls.null_frac, 0)) + e.pairs) * e.payload_texts
       + h.hashed_ms * (e.build_rows + e.probe_rows) * e.hashed_keys
       + CASE WHEN e.kind IN ('dup', 'bloom') AND e.name LIKE 'dup%' THEN c.compact_ms * e.pairs ELSE 0 END
       + CASE WHEN e.kind = 'residual' THEN
             (current_setting('tessera.filter_clause_cost')::float8 * u.unit_ms + g.gather_ms) * e.matches
         ELSE 0 END
       + CASE WHEN e.bloom THEN bl.test_ms * e.probe_rows - f.probe_ms * (e.probe_rows - least(e.pairs, e.probe_rows))
         ELSE 0 END
       + CASE WHEN e.name = 'anti' THEN f.pair_ms * (e.probe_rows - 2 * e.pairs) ELSE 0 END
       + CASE WHEN e.spilled_chunks > 0 THEN sp.spill_ms * (e.build_rows + e.probe_rows) ELSE 0 END AS predicted_ms
FROM spill_share AS e
CROSS JOIN base_fit AS f CROSS JOIN gather_fit AS g CROSS JOIN text_fit AS t CROSS JOIN hashed_fit AS h
CROSS JOIN compact_fit AS c CROSS JOIN bloom_fit AS bl CROSS JOIN spill_fit AS sp CROSS JOIN model_unit AS u
LEFT JOIN LATERAL (SELECT null_frac FROM pg_stats WHERE tablename = e.inner_rel AND attname = 'b') AS nulls ON true
WHERE e.workers = 0;
SELECT kind, name, half, round(own_ms::numeric, 2) AS own_ms, round(predicted_ms::numeric, 2) AS predicted_ms,
       round(((predicted_ms - own_ms) / own_ms * 100)::numeric) AS error_pct
FROM prediction ORDER BY kind, name, half;
SELECT round(sqrt(avg(((predicted_ms - own_ms) / own_ms) ^ 2))::numeric, 3) AS rms_relative_error,
       round(max(abs((predicted_ms - own_ms) / own_ms))::numeric, 3) AS max_relative_error,
       count(*) AS samples
FROM prediction;

-- The core's hash join on the base's queries: its own time fitted as the
-- node's (a row built, a row probed, a pair), its time per unit of its
-- own cost (the join's less its children's), sample by sample and over
-- all, and that price in the scan model's units: tessera.join_cost_unit,
-- the time of the node that one unit of the core's hash join cost stands
-- for, by which the planner divides the node's time.
CREATE TEMP VIEW core_samples AS
SELECT * FROM samples WHERE kind = 'core';
CREATE TEMP VIEW core_fit AS
WITH x AS (
    SELECT ARRAY[build_rows, probe_rows, pairs] AS r, own_ms AS y FROM core_samples
), a AS (
    SELECT array_agg(s ORDER BY i, j) AS a
    FROM (SELECT i, j, sum(r[i] * r[j]) AS s
          FROM x, generate_series(1, 3) AS i, generate_series(1, 3) AS j
          GROUP BY i, j) AS sums
), b AS (
    SELECT array_agg(s ORDER BY i) AS b
    FROM (SELECT i, sum(r[i] * y) AS s FROM x, generate_series(1, 3) AS i GROUP BY i) AS sums
), c AS (
    SELECT pg_temp.solve(a.a, b.b, 3) AS c FROM a, b
)
SELECT c[1] AS build_ms, c[2] AS probe_ms, c[3] AS pair_ms FROM c;
SELECT round((build_ms * 1e6)::numeric, 2) AS core_build_ns, round((probe_ms * 1e6)::numeric, 2) AS core_probe_ns,
       round((pair_ms * 1e6)::numeric, 2) AS core_pair_ns
FROM core_fit;
SELECT s.name, s.half, s.build_rows AS build, s.probe_rows AS probe, s.pairs, round(s.own_cost::numeric) AS own_cost,
       round(s.own_ms::numeric, 2) AS own_ms, round((s.own_ms / s.own_cost * 1000)::numeric, 3) AS us_per_cost_unit,
       round(n.own_ms::numeric, 2) AS node_own_ms, round((s.own_ms / n.own_ms)::numeric, 2) AS core_over_node
FROM core_samples AS s
JOIN samples AS n ON n.name = s.name AND n.half = s.half AND n.kind = 'size'
ORDER BY s.build_rows, s.half;
CREATE TEMP VIEW core_rate AS
SELECT sum(own_ms) / sum(own_cost) AS ms_per_cost_unit FROM core_samples;
SELECT round((r.ms_per_cost_unit * 1000)::numeric, 3) AS core_us_per_cost_unit,
       round((r.ms_per_cost_unit / u.unit_ms)::numeric, 3) AS "tessera.join_cost_unit"
FROM core_rate AS r, model_unit AS u;

-- The parameters, in the units of the scan model: a full scan's page is 1.
SELECT round((u.unit_ms * 1000)::numeric, 4) AS unit_us,
       round((f.build_ms / u.unit_ms)::numeric, 5) AS "tessera.join_build_cost",
       round((f.probe_ms / u.unit_ms)::numeric, 5) AS "tessera.join_probe_cost",
       round((f.pair_ms / u.unit_ms)::numeric, 5) AS "tessera.join_pair_cost",
       round((f.batch_ms / u.unit_ms)::numeric, 4) AS "tessera.join_batch_cost",
       round((g.gather_ms / u.unit_ms)::numeric, 5) AS "tessera.join_gather_cost",
       round((t.text_ms / u.unit_ms)::numeric, 5) AS "tessera.join_text_value_cost",
       round((h.hashed_ms / u.unit_ms)::numeric, 5) AS "tessera.join_hashed_key_cost",
       round((c.compact_ms / u.unit_ms)::numeric, 5) AS "tessera.join_compact_pair_cost",
       round((bl.test_ms / u.unit_ms)::numeric, 5) AS "tessera.join_bloom_test_cost",
       round((sp.spill_ms / u.unit_ms)::numeric, 5) AS "tessera.join_spill_row_cost"
FROM model_unit AS u, base_fit AS f, gather_fit AS g, text_fit AS t, hashed_fit AS h, compact_fit AS c,
     bloom_fit AS bl, spill_fit AS sp;
\o
