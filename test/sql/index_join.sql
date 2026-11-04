CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

-- The index join: TessHashJoin looks the keys of each outer batch up in
-- an index of the inner table at once (`Inner Lookups`), through
-- TessHeapScan in its lookup mode, and builds a table of the rows found
-- for that batch. Every query is compared with Tessera off.
CREATE FUNCTION ij_same(query text) RETURNS text
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
    RETURN coalesce(array_length(with_tessera, 1), 0) || ' rows, hash ' ||
           md5(coalesce(with_tessera::text, ''));
END $$;

-- The index a plan looks up, or 'none'.
CREATE FUNCTION ij_lookup(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (COSTS OFF) ' || query LOOP
        IF line ~ 'Lookup Index:' THEN
            RETURN trim(split_part(line, ':', 2));
        END IF;
    END LOOP;
    RETURN 'none';
END $$;

-- A line of the join's EXPLAIN ANALYZE, as JSON.
CREATE FUNCTION ij_property(query text, name text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == "TessHashJoin").%I', name)::jsonpath)::text;
END $$;

-- 400 000 inner rows with indexes of a unique key in the table's order,
-- of four rows a key, and of text, date, numeric and char(6) keys; 3000
-- outer rows whose key k finds a row for the first 1000, none for the
-- next 1000, and is NULL for the last 1000, so that batches with pairs,
-- without and with NULL keys only come one after another.
CREATE TABLE ij_inner (id int PRIMARY KEY, o4 int, tk text, dk date, nk numeric, ck char(6),
                       v int, w int)
WITH (autovacuum_enabled = off, fillfactor = 90);
INSERT INTO ij_inner
SELECT g, g / 4, 'k' || g, date '2000-01-01' + g, g::numeric / 10, lpad(g::text, 6, '0'),
       g % 1000, g % 7
FROM generate_series(1, 400000) AS g;
CREATE INDEX ij_inner_o4 ON ij_inner (o4);
CREATE INDEX ij_inner_tk ON ij_inner (tk);
CREATE INDEX ij_inner_dk ON ij_inner (dk);
CREATE INDEX ij_inner_nk ON ij_inner (nk);
CREATE INDEX ij_inner_ck ON ij_inner (ck);
ANALYZE ij_inner;
CREATE TABLE ij_outer AS
SELECT g AS id,
       CASE WHEN g <= 1000 THEN g * 3 WHEN g <= 2000 THEN 500000 + g END AS k,
       g / 2 AS kd, 'k' || (g * 3) AS kt, date '2000-01-01' + g * 3 AS kdate,
       ((g * 3)::numeric / 10)::numeric(12, 2) AS kn, lpad((g * 3)::text, 6, '0')::char(8) AS kc,
       (g * 3)::bigint AS k8, g % 5 AS v, g * 3 AS ko,
       ((g * 7919) % 3000 + 1) * 133 AS ks
FROM generate_series(1, 3000) AS g;
ANALYZE ij_outer;
SET enable_mergejoin = off;
SET enable_nestloop = off;
SET max_parallel_workers_per_gather = 0;
-- The lookup's prices set to nothing: the suite checks the mode's rows
-- and plans; the section on the cost checks its model at the defaults.
SET tessera.join_lookup_batch_cost = 0;
SET tessera.join_lookup_descent_cost = 0;
SET tessera.join_lookup_key_cost = 0;

EXPLAIN (COSTS OFF) SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k;
-- A lookup and a table for each batch with a key; none for a batch of
-- NULL keys only.
SELECT ij_property($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$, 'Lookups') AS lookups,
       ij_property($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$, 'Builds') AS builds,
       ij_property($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$, 'Lookup Keys') AS lookup_keys,
       ij_property($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$, 'Index Searches')::int > 0 AS searched;
-- Batches with pairs, without, and with NULL keys only, one after another.
SELECT ij_same($$SELECT o.id, l.v, l.tk FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$);
SELECT ij_lookup($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$) AS left_join;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$);
-- Duplicates on both sides: two outer rows and four inner rows a key.
SELECT ij_lookup($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.o4 = o.kd WHERE o.id <= 1000$$) AS duplicates;
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.o4 = o.kd WHERE o.id <= 1000$$);
-- Sparse batches: one outer row in 50.
SELECT ij_lookup($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id % 50 = 0$$) AS sparse;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id % 50 = 0$$);
-- Another equality and a join clause: the lookup finds the candidates,
-- the join's other clauses decide; LEFT rejected by ON and by WHERE.
SELECT ij_lookup($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.id = o.k AND l.w = o.v$$) AS two_keys;
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.id = o.k AND l.w = o.v$$);
SELECT ij_same($$SELECT o.id, l.w FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k AND l.w > o.v$$);
SELECT ij_same($$SELECT o.id, l.w FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k WHERE l.w IS NULL OR l.w > 3$$);
-- The inner table's clauses in TessFilter above the lookup, and columns
-- computed over the pairs.
EXPLAIN (COSTS OFF) SELECT o.id, l.v * 2 + o.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE l.w < 5;
SELECT ij_same($$SELECT o.id, l.v * 2 + o.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE l.w < 5$$);
-- Keys of every kind: text, date, numeric (1.0 against 1.00), char(n)
-- of two lengths, a bigint outer key against the int index.
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.tk = o.kt WHERE o.id <= 1000$$) AS text_key,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.dk = o.kdate WHERE o.id <= 1000$$) AS date_key,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.nk = o.kn WHERE o.id <= 1000$$) AS numeric_key,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.ck = o.kc WHERE o.id <= 1000$$) AS char_key,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k8 WHERE o.id <= 1000$$) AS cross_type;
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.tk = o.kt WHERE o.id <= 1000$$);
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.dk = o.kdate WHERE o.id <= 1000$$);
SELECT ij_same($$SELECT o.id, l.id, l.nk FROM ij_outer o JOIN ij_inner l ON l.nk = o.kn WHERE o.id <= 1000$$);
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.ck = o.kc WHERE o.id <= 1000$$);
SELECT ij_same($$SELECT o.id, l.id FROM ij_outer o JOIN ij_inner l ON l.id = o.k8 WHERE o.id <= 1000$$);
-- The cost, against the core's nested loop over the index: keys of a
-- batch next to each other in the index share its descents and take the
-- lookup; shuffled keys, and batches of one key, descend once a key and
-- leave the core's nested loop the join. An inner table past
-- effective_cache_size gets no lookup. EXPLAIN VERBOSE shows the
-- planner's counts with the costs.
RESET tessera.join_lookup_batch_cost;
RESET tessera.join_lookup_descent_cost;
RESET tessera.join_lookup_key_cost;
SET enable_nestloop = on;
SELECT ij_lookup($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ko WHERE o.id <= 1000$$) AS ordered,
       ij_lookup($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ks WHERE o.id <= 1000$$) AS shuffled,
       ij_lookup($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ko WHERE o.id % 50 = 0$$) AS sparse;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.ks WHERE o.id <= 1000$$);
SET enable_nestloop = off;
SET tessera.join_lookup_batch_cost = 0;
SET tessera.join_lookup_descent_cost = 0;
SET tessera.join_lookup_key_cost = 0;
SET effective_cache_size = '8kB';
SELECT ij_lookup($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 1000$$) AS past_cache;
RESET effective_cache_size;
-- The planner's counts of the ordered case, serial and under a Gather,
-- each participant counting its share: the batches of a page of the
-- outer table that hold a key, the keys, and the descents, a bound the
-- keys of a batch share.
CREATE FUNCTION ij_estimate(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
    estimate text;
BEGIN
    EXECUTE 'EXPLAIN (VERBOSE, FORMAT JSON) ' || query INTO plan;
    estimate := jsonb_path_query_first(plan, '$.** ? (@."Lookup Estimate" != null)."Lookup Estimate"') #>> '{}';
    RETURN substring(estimate from '^batches=[0-9]+ keys=[0-9]+') || ', descents within a quarter of the keys: ' ||
           ((regexp_match(estimate, 'descents=([0-9]+)'))[1]::int * 4 <=
            (regexp_match(estimate, 'keys=([0-9]+)'))[1]::int);
END $$;
SELECT ij_estimate($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ko WHERE o.id <= 1000$$) AS serial;
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT ij_lookup($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ko WHERE o.id <= 1000$$) AS parallel,
       ij_estimate($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.ko WHERE o.id <= 1000$$) AS each_participant;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
DROP FUNCTION ij_estimate(text);
-- Row output, a cursor read in part, and a limit.
SELECT ij_lookup($$SELECT o.id, l.tk FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$) AS rows_out;
SELECT o.id, l.tk FROM ij_outer o JOIN ij_inner l ON l.id = o.k ORDER BY o.id LIMIT 3;
BEGIN;
DECLARE ij_cursor CURSOR FOR SELECT o.id, l.tk FROM ij_outer o JOIN ij_inner l ON l.id = o.k;
FETCH 3 FROM ij_cursor;
FETCH 2 FROM ij_cursor;
COMMIT;
SELECT ij_same($$SELECT count(*) FROM (SELECT o.id FROM ij_outer o JOIN ij_inner l ON l.id = o.k LIMIT 700) AS q$$);
-- Rescans: a parameter of the outer side only, and one of the inner
-- table's clause too.
SELECT ij_lookup($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= x) FROM generate_series(100, 1000, 300) AS x$$) AS outer_parameter,
       ij_lookup($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE l.w <> x % 7 AND o.id <= 700) FROM generate_series(1, 4) AS x$$) AS inner_parameter;
SELECT ij_same($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= x) FROM generate_series(100, 1000, 300) AS x$$);
SELECT ij_same($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE l.w <> x % 7 AND o.id <= 700) FROM generate_series(1, 4) AS x$$);
-- A HOT update of a column no index holds: the new versions stay on
-- their page, where the index finds them through the old ones.
CREATE TEMP TABLE ij_pages AS
SELECT id, (ctid::text::point)[0] AS page FROM ij_inner WHERE id IN (3, 6, 9);
UPDATE ij_inner SET v = v + 1000000 WHERE id IN (3, 6, 9);
SELECT bool_and((l.ctid::text::point)[0] = p.page) AS same_page
FROM ij_inner l JOIN ij_pages p USING (id);
DROP TABLE ij_pages;
SELECT ij_lookup($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 10$$) AS hot;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 10$$);
-- A prepared plan after its index is dropped, and made again.
PREPARE ij_by_text AS SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.tk = o.kt WHERE o.id <= 1000;
EXECUTE ij_by_text;
DROP INDEX ij_inner_tk;
EXECUTE ij_by_text;
CREATE INDEX ij_inner_tk ON ij_inner (tk);
EXECUTE ij_by_text;
DEALLOCATE ij_by_text;

-- A partitioned outer side: TessAppend's batches feed the lookups, and
-- the planner counts each partition's.
CREATE TABLE ij_parts (id int, ko int) PARTITION BY RANGE (id);
CREATE TABLE ij_parts_1 PARTITION OF ij_parts FOR VALUES FROM (1) TO (1501);
CREATE TABLE ij_parts_2 PARTITION OF ij_parts FOR VALUES FROM (1501) TO (3001);
INSERT INTO ij_parts SELECT id, ko FROM ij_outer;
ANALYZE ij_parts;
SELECT ij_lookup($$SELECT count(*), sum(l.v) FROM ij_parts o JOIN ij_inner l ON l.id = o.ko WHERE o.id % 2 = 0$$) AS partitioned;
SELECT ij_same($$SELECT o.id, l.v FROM ij_parts o JOIN ij_inner l ON l.id = o.ko WHERE o.id % 2 = 0$$);
DROP TABLE ij_parts;

-- Indexes the lookup refuses: partial, of an expression, of another
-- access method; an inheritance parent; a nondeterministic collation.
-- Beside each, the plain index or table the lookup takes.
CREATE TABLE ij_other (k int, e int, t text COLLATE "C", v int);
INSERT INTO ij_other SELECT g, g, 'k' || g, g % 10 FROM generate_series(1, 200000) AS g;
CREATE INDEX ij_other_partial ON ij_other (k) WHERE v > 0;
CREATE INDEX ij_other_expr ON ij_other ((e + 0));
CREATE INDEX ij_other_hash ON ij_other USING hash (t);
ANALYZE ij_other;
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_other x ON x.k = o.k WHERE o.id <= 1000$$) AS partial,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_other x ON x.e = o.k WHERE o.id <= 1000$$) AS expression,
       ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_other x ON x.t = o.kt WHERE o.id <= 1000$$) AS hash;
SELECT ij_same($$SELECT o.id, x.v FROM ij_outer o JOIN ij_other x ON x.k = o.k WHERE o.id <= 1000$$);
CREATE INDEX ij_other_k ON ij_other (k);
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_other x ON x.k = o.k WHERE o.id <= 1000$$) AS plain;
CREATE TABLE ij_parent (k int, v int);
CREATE TABLE ij_child () INHERITS (ij_parent);
INSERT INTO ij_parent SELECT g, 1 FROM generate_series(1, 100000) AS g;
INSERT INTO ij_child SELECT g, 2 FROM generate_series(1, 100000) AS g;
CREATE INDEX ij_parent_k ON ij_parent (k);
CREATE INDEX ij_child_k ON ij_child (k);
ANALYZE ij_parent, ij_child;
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_parent p ON p.k = o.k WHERE o.id <= 1000$$) AS inheritance;
SELECT ij_same($$SELECT o.id, p.v FROM ij_outer o JOIN ij_parent p ON p.k = o.k WHERE o.id <= 1000$$);
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ONLY ij_parent p ON p.k = o.k WHERE o.id <= 1000$$) AS only_parent;
SELECT ij_same($$SELECT o.id, p.v FROM ij_outer o JOIN ONLY ij_parent p ON p.k = o.k WHERE o.id <= 1000$$);
CREATE COLLATION ij_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE ij_ci_inner (t text COLLATE ij_ci, v int);
INSERT INTO ij_ci_inner SELECT 'K' || g, g FROM generate_series(1, 200000) AS g;
CREATE INDEX ij_ci_inner_t ON ij_ci_inner (t);
ANALYZE ij_ci_inner;
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_ci_inner c ON c.t = o.kt WHERE o.id <= 1000$$) AS nondeterministic;
SELECT ij_same($$SELECT o.id, c.v FROM ij_outer o JOIN ij_ci_inner c ON c.t = o.kt WHERE o.id <= 1000$$);
-- A policy of the inner table: its clauses stay above the lookup, in
-- their order.
ALTER TABLE ij_inner ENABLE ROW LEVEL SECURITY;
CREATE POLICY ij_inner_visible ON ij_inner USING (w <> 3);
CREATE ROLE regress_tessera_ij;
GRANT SELECT ON ij_inner, ij_outer TO regress_tessera_ij;
SET ROLE regress_tessera_ij;
SELECT ij_same($$SELECT o.id, l.w FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$);
RESET ROLE;
DROP POLICY ij_inner_visible ON ij_inner;
ALTER TABLE ij_inner DISABLE ROW LEVEL SECURITY;
REVOKE SELECT ON ij_inner, ij_outer FROM regress_tessera_ij;
DROP ROLE regress_tessera_ij;

-- spec: table-memory/A batch's table past the limit
-- One key holds 60 000 rows the statistics, taken before them, do not
-- show: the batch's table is built whole past hash_mem, and its excess
-- is counted against the hash_mem in force at its build: a limit raised
-- by a later outer batch, past the table, leaves the excess as it was.
CREATE TABLE ij_skew (k int, t text) WITH (autovacuum_enabled = off);
INSERT INTO ij_skew SELECT g, 'x' || g FROM generate_series(1, 200000) AS g;
CREATE INDEX ij_skew_k ON ij_skew (k);
ANALYZE ij_skew;
INSERT INTO ij_skew SELECT 3, repeat('y', 20) FROM generate_series(1, 60000);
SET hash_mem_multiplier = 1;
SET work_mem = '1MB';
SELECT ij_lookup($$SELECT count(*), sum(length(s.t)) FROM ij_outer o JOIN ij_skew s ON s.k = o.k WHERE o.id <= 1000$$) AS skewed;
SELECT ij_property($$SELECT count(*), sum(length(s.t)) FROM ij_outer o JOIN ij_skew s ON s.k = o.k WHERE o.id <= 1000$$, 'Overrun')::int AS small_limit \gset
SELECT ij_same($$SELECT count(*), sum(length(s.t)) FROM ij_outer o JOIN ij_skew s ON s.k = o.k WHERE o.id <= 1000$$);
CREATE FUNCTION ij_raise_work_mem(id int) RETURNS boolean
LANGUAGE plpgsql AS $$
BEGIN
    IF id >= 100 THEN
        PERFORM set_config('work_mem', '64MB', true);
    END IF;
    RETURN true;
END $$;
SELECT ij_property($$SELECT count(*), sum(length(s.t)) FROM ij_outer o JOIN ij_skew s ON s.k = o.k WHERE o.id <= 1000 AND ij_raise_work_mem(o.id)$$, 'Overrun')::int AS raised_limit \gset
SELECT :small_limit > 1024 AS past_the_limit, :raised_limit = :small_limit AS kept_when_raised;
RESET work_mem;
DROP FUNCTION ij_raise_work_mem(int);
RESET hash_mem_multiplier;

-- spec: key-filter/A table per outer batch
-- A table of one batch's keys builds no Bloom filter and hands none to
-- the outer side's TessFilter, which takes one from other joins; the
-- same join without the lookup builds one.
SET tessera.join_bloom_ratio = 1;
SET enable_indexscan = off;
SELECT ij_lookup($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%'$$) AS without_index,
       ij_property($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%'$$, 'Bloom Filters') AS filters;
RESET enable_indexscan;
EXPLAIN (COSTS OFF) SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%';
SELECT ij_property($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%'$$, 'Bloom Filters') IS NULL AS no_filter,
       ij_property($$SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%'$$, 'Bloom Filter Below') IS NULL AS none_below;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= 2000 AND o.kt LIKE '%1%'$$);
RESET tessera.join_bloom_ratio;

-- Under a Gather: each participant looks up the keys of its own batches;
-- the workers alone, and a rescan of the Gather.
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
EXPLAIN (COSTS OFF) SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_inner l ON l.id = o.k;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_inner l ON l.id = o.k$$);
SET parallel_leader_participation = off;
SELECT ij_lookup($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$) AS workers_alone,
       ij_property($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$, 'Lookup Keys') AS lookup_keys,
       ij_property($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$, 'Index Searches')::int > 0 AS searched;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o LEFT JOIN ij_inner l ON l.id = o.k$$);
RESET parallel_leader_participation;
SELECT ij_lookup($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= x) FROM generate_series(500, 2500, 1000) AS x$$) AS gather_rescan;
SELECT ij_same($$SELECT x, (SELECT count(*) FROM ij_outer o JOIN ij_inner l ON l.id = o.k WHERE o.id <= x) FROM generate_series(500, 2500, 1000) AS x$$);
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;

-- The lookup's count of the batches its input gathers (Lookup Estimate's
-- batches) beside the lookups made: a scan's filter that keeps a share
-- of each page's rows at random, from a few to most of a batch, the
-- planner counting at random what the table holds evenly; a join's
-- output; an append of partitions of a row each, whose rows the input
-- gathers across the parts but for the first two, which go as they are.
SET tessera.join_lookup_batch_cost = 0;
SET tessera.join_lookup_descent_cost = 0;
SET tessera.join_lookup_key_cost = 0;
CREATE FUNCTION ij_gathered(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
    estimate text;
BEGIN
    EXECUTE 'EXPLAIN (VERBOSE, FORMAT JSON) ' || query INTO plan;
    estimate := jsonb_path_query_first(plan, '$.** ? (@."Lookup Estimate" != null)."Lookup Estimate"') #>> '{}';
    RETURN 'planned ' || coalesce((regexp_match(estimate, 'batches=([0-9]+)'))[1], 'none') ||
           ', made ' || coalesce(ij_property(query, 'Lookups'), 'none');
END $$;
CREATE TABLE ij_share (id int, ko int, r int) WITH (autovacuum_enabled = off);
INSERT INTO ij_share SELECT g, g * 3, (g * 7919) % 100 FROM generate_series(1, 6000) AS g;
ANALYZE ij_share;
SELECT share, ij_gathered(format($$SELECT count(*), sum(l.v) FROM ij_share o JOIN ij_inner l ON l.id = o.ko WHERE o.r < %s$$, share))
FROM (VALUES (5), (20), (45), (55), (80)) AS s(share);
SELECT share, ij_same(format($$SELECT o.id, l.v FROM ij_share o JOIN ij_inner l ON l.id = o.ko WHERE o.r < %s$$, share))
FROM (VALUES (5), (20), (45), (55), (80)) AS s(share);
CREATE TABLE ij_sel (k int);
INSERT INTO ij_sel SELECT g * 10 FROM generate_series(1, 300) AS g;
ANALYZE ij_sel;
SELECT ij_gathered($$SELECT count(*), sum(l.v) FROM ij_outer o JOIN ij_sel s ON s.k = o.id JOIN ij_inner l ON l.id = o.ko$$) AS join_output;
SELECT ij_same($$SELECT o.id, l.v FROM ij_outer o JOIN ij_sel s ON s.k = o.id JOIN ij_inner l ON l.id = o.ko$$);
CREATE TABLE ij_one (id int, ko int) PARTITION BY LIST (id);
DO $$
BEGIN
    FOR part IN 1..64 LOOP
        EXECUTE format('CREATE TABLE ij_one_%s PARTITION OF ij_one FOR VALUES IN (%s)', part, part);
    END LOOP;
END $$;
INSERT INTO ij_one SELECT g, g * 3 FROM generate_series(1, 64) AS g;
ANALYZE ij_one;
SELECT ij_gathered($$SELECT count(*), sum(l.v) FROM ij_one o JOIN ij_inner l ON l.id = o.ko$$) AS parts_of_a_row;
SELECT ij_same($$SELECT o.id, l.v FROM ij_one o JOIN ij_inner l ON l.id = o.ko$$);
DROP TABLE ij_sel, ij_one, ij_share;
DROP FUNCTION ij_gathered(text);
RESET tessera.join_lookup_batch_cost;
RESET tessera.join_lookup_descent_cost;
RESET tessera.join_lookup_key_cost;

DROP TABLE ij_inner, ij_outer, ij_other, ij_parent, ij_child, ij_ci_inner, ij_skew;
DROP COLLATION ij_ci;
DROP FUNCTION ij_same(text);
DROP FUNCTION ij_lookup(text);
DROP FUNCTION ij_property(text, text);
DROP EXTENSION tessera;
