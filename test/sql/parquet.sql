-- The foreign data wrapper tessera_parquet over files its test module
-- writes: the options and their privileges, the files and patterns, and
-- the columns refused, at planning and when a scan begins.
\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set parquet_test :libdir '/tessera_parquet_test' :dlsuffix
CREATE EXTENSION tessera;
-- The nodes and the kernels, so that Tessera on plans its own nodes.
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
CREATE EXTENSION tessera_parquet;
CREATE FUNCTION parquet_test_write(text, text) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_write' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_rewrite(text, bool, bool, bool, int, int) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_rewrite' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_head_dictionaries(text) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_head_dictionaries' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_replace(text, text, text) RETURNS text
    AS :'parquet_test', 'tessera_parquet_test_replace' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_remove(text) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_remove' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_cut(text, int) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_cut' LANGUAGE C STRICT;
CREATE FUNCTION parquet_test_damage(text, int, int) RETURNS void
    AS :'parquet_test', 'tessera_parquet_test_damage' LANGUAGE C STRICT;

-- The SQLSTATE a statement raises, or ok: the paths of the files, which
-- the messages name, differ from one machine to another.
CREATE FUNCTION sqlstate_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE statement;
    RETURN 'ok';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE;
END $$;
-- The rows two queries differ by, counted both ways.
CREATE FUNCTION differ(a text, b text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    n bigint;
BEGIN
    EXECUTE format('SELECT count(*) FROM ((%s EXCEPT ALL %s) UNION ALL (%s EXCEPT ALL %s)) d',
                   a, b, b, a) INTO n;
    RETURN n;
END $$;

-- The rows a query with Tessera on differs by from the same query with
-- it off, counted both ways.
CREATE FUNCTION differ_off(statement text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    n bigint;
    saved text := current_setting('tessera.enable');
BEGIN
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE 'CREATE TEMP TABLE tessera_off AS ' || statement;
    PERFORM set_config('tessera.enable', saved, true);
    EXECUTE format('SELECT count(*) FROM ((%s EXCEPT ALL TABLE tessera_off) UNION ALL '
                   '(TABLE tessera_off EXCEPT ALL %s)) d', statement, statement) INTO n;
    DROP TABLE tessera_off;
    RETURN n;
END $$;
-- The scans of a statement's plan, and the lines of its EXPLAIN ANALYZE
-- that match a pattern, the paths of the suite's files written $dir.
CREATE FUNCTION scans_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    line text;
    found text[] := '{}';
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (COSTS OFF) ' || statement LOOP
        found := found || ARRAY(SELECT m[1] FROM regexp_matches(line,
            '(Foreign Scan|Seq Scan|Custom Scan \(\w+\))', 'g') m);
    END LOOP;
    RETURN array_to_string(found, ', ');
END $$;
-- The rows of a statement.
CREATE FUNCTION rows_of(statement text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    n bigint;
BEGIN
    EXECUTE format('SELECT count(*) FROM (%s) s', statement) INTO n;
    RETURN n;
END $$;
-- Who checks a statement's clauses: the source alone, TessFilter, or
-- another plan.
CREATE FUNCTION checked_by(statement text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    line text;
    source boolean := false;
    filter boolean := false;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (COSTS OFF) ' || statement LOOP
        source := source OR line ~ 'Source Exact';
        filter := filter OR line ~ 'Filter:';
    END LOOP;
    RETURN CASE WHEN filter THEN 'filter' WHEN source THEN 'source' ELSE 'none' END;
END $$;
CREATE FUNCTION explain_lines(options text, statement text, pattern text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE format('EXPLAIN (%s, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) %s',
                               options, statement) LOOP
        IF line ~ pattern THEN
            RETURN NEXT replace(trim(line), current_setting('parquet.dir'), '$dir');
        END IF;
    END LOOP;
END $$;
-- The same for a parallel plan, each node's actual rows and loops left
-- out: they depend on the workers launched.
CREATE FUNCTION parallel_lines(statement text, pattern text) RETURNS SETOF text
LANGUAGE sql AS $$
    SELECT regexp_replace(line, ' \(actual [^)]*\)', '')
    FROM explain_lines('ANALYZE', statement, pattern) line
$$;
-- The workers a parallel plan launched, over its gathers.
CREATE FUNCTION workers_launched(statement text) RETURNS int LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) '
        || statement INTO plan;
    RETURN (SELECT sum(w::int) FROM jsonb_path_query(plan, '$.** ."Workers Launched"') w);
END $$;

\getenv abs_builddir PG_ABS_BUILDDIR
\set dir :abs_builddir '/results/parquet_files'
\set one :dir '/one.parquet'
\set none :dir '/none*.parquet'
\set gzip :dir '/gzip.parquet'
\set types :dir '/types.parquet'
SELECT set_config('parquet.dir', :'dir', false) IS NOT NULL AS dir_set;

-- Files of a few groups, uncompressed and compressed by Snappy.
SELECT parquet_test_write(current_setting('parquet.dir') || '/one.parquet',
    '{"group_rows": 2, "columns": [
      {"name": "a", "type": "int32", "values": [1, 2, 3]},
      {"name": "s", "type": "bytes", "values": ["x", "y", "z"]}]}');
SELECT parquet_test_write(current_setting('parquet.dir') || format('/p%s.parquet', i),
    format('{"group_rows": 2, "compression": "%s", "columns": [
      {"name": "a", "type": "int32", "values": [%s, %s, %s]},
      {"name": "s", "type": "bytes", "values": ["p", "q", "r"]}]}',
      CASE WHEN i % 2 = 0 THEN 'snappy' ELSE 'none' END, i * 10, i * 10 + 1, i * 10 + 2))
FROM generate_series(1, 2) i;

CREATE SERVER pq FOREIGN DATA WRAPPER tessera_parquet;

-- spec: parquet-fdw/Options refused
-- Options refused, each with its SQLSTATE.
SELECT sqlstate_of('CREATE SERVER pq_bad FOREIGN DATA WRAPPER tessera_parquet OPTIONS (x ''1'')') AS server_option;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (a int) SERVER pq OPTIONS (files %L, other ''x'')',
    current_setting('parquet.dir') || '/one.parquet')) AS unknown_option;
SELECT sqlstate_of('CREATE FOREIGN TABLE t_bad (a int) SERVER pq') AS without_files;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (a int) SERVER pq OPTIONS (files %L, files %L)',
    current_setting('parquet.dir') || '/one.parquet', current_setting('parquet.dir') || '/one.parquet')) AS files_twice;
SELECT sqlstate_of('CREATE FOREIGN TABLE t_bad (a int) SERVER pq OPTIONS (files ''relative/one.parquet'')') AS relative_path;
SELECT sqlstate_of('CREATE FOREIGN TABLE t_bad (a int) SERVER pq OPTIONS (files '' , '')') AS empty_list;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (a int OPTIONS (unit ''weeks'')) SERVER pq OPTIONS (files %L)',
    current_setting('parquet.dir') || '/one.parquet')) AS unit_weeks;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (s text OPTIONS (utf8 ''maybe'')) SERVER pq OPTIONS (files %L)',
    current_setting('parquet.dir') || '/one.parquet')) AS utf8_maybe;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (s text OPTIONS (utf8 ''trust'')) SERVER pq OPTIONS (files %L)',
    current_setting('parquet.dir') || '/one.parquet')) AS trust_without_column;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_bad (a int OPTIONS (nope ''1'')) SERVER pq OPTIONS (files %L)',
    current_setting('parquet.dir') || '/one.parquet')) AS unknown_column_option;

-- The privileges: a role with those of pg_read_server_files, not a
-- superuser, makes a table; a role without them may not.
CREATE ROLE parquet_reader IN ROLE pg_read_server_files;
CREATE ROLE parquet_other;
GRANT USAGE ON FOREIGN SERVER pq TO parquet_reader, parquet_other;
GRANT CREATE ON SCHEMA public TO parquet_reader, parquet_other;
SET ROLE parquet_reader;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_files (a int, s text) SERVER pq OPTIONS (files %L)',
    concat_ws(',', current_setting('parquet.dir') || '/one.parquet',
              current_setting('parquet.dir') || '/p*.parquet',
              current_setting('parquet.dir') || '/p?.parquet',
              current_setting('parquet.dir') || '/p[12].parquet'))) AS reader_makes_a_table;
RESET ROLE;
SET ROLE parquet_other;
SELECT sqlstate_of(format('CREATE FOREIGN TABLE t_other (a int) SERVER pq OPTIONS (files %L)',
    current_setting('parquet.dir') || '/one.parquet')) AS other_makes_a_table;
RESET ROLE;
CREATE FOREIGN TABLE t_owned (a int, s text OPTIONS (column 's', utf8 'trust')) SERVER pq
    OPTIONS (files :'one');
CREATE FOREIGN TABLE t_plain (a int, s text) SERVER pq OPTIONS (files :'one');
ALTER FOREIGN TABLE t_owned OWNER TO parquet_other;
ALTER FOREIGN TABLE t_plain OWNER TO parquet_other;
SET ROLE parquet_other;
SELECT sqlstate_of(format('ALTER FOREIGN TABLE t_plain OPTIONS (SET files %L)',
    current_setting('parquet.dir') || '/p1.parquet')) AS other_changes_files;
SELECT sqlstate_of('ALTER FOREIGN TABLE t_plain ALTER COLUMN s OPTIONS (ADD column ''s'', ADD utf8 ''trust'')') AS other_trusts;
SELECT sqlstate_of('ALTER FOREIGN TABLE t_owned ALTER COLUMN s OPTIONS (SET column ''a'')') AS other_moves_trust;
SELECT sqlstate_of('ALTER FOREIGN TABLE t_plain ALTER COLUMN s OPTIONS (ADD column ''s'')') AS other_names_a_column;
RESET ROLE;

-- A pattern that matches no file, at planning.
CREATE FOREIGN TABLE t_none (a int) SERVER pq OPTIONS (files :'none');
SELECT sqlstate_of('EXPLAIN SELECT a FROM t_none') AS no_file_matches;
-- The patterns of the reader's table plan, the files found.
SELECT sqlstate_of('EXPLAIN SELECT a FROM t_files') AS patterns_planned;

-- spec: parquet-fdw/A table over files and patterns
-- The rows of the seven files, the file first and then each pattern's
-- two in the order of the bytes of their names, the groups of a file and
-- the rows of a group in their order, with Tessera off in a serial plan.
SET tessera.enable = off;
SET max_parallel_workers_per_gather = 0;
SELECT a, s FROM t_files;
SELECT count(*), sum(a) FROM t_files;
RESET tessera.enable;
SELECT count(*), sum(a) FROM t_files;
RESET max_parallel_workers_per_gather;

-- spec: parquet-fdw/Rows with Tessera off
-- The seven files against a heap table of their rows: in full, by
-- clauses, under a grouping, and as the inner side of a nested loop that
-- scans them again for each outer row.
CREATE TABLE h_files (a int, s text);
INSERT INTO h_files VALUES (1, 'x'), (2, 'y'), (3, 'z');
INSERT INTO h_files SELECT i * 10 + j, (ARRAY['p', 'q', 'r'])[j + 1]
FROM generate_series(1, 3) k, generate_series(1, 2) i, generate_series(0, 2) j;
SET tessera.enable = off;
SELECT differ('TABLE t_files', 'TABLE h_files') AS in_full,
       differ('SELECT a FROM t_files WHERE a > 11 AND s <> ''q''',
              'SELECT a FROM h_files WHERE a > 11 AND s <> ''q''') AS by_clauses,
       differ('SELECT s, count(*), sum(a) FROM t_files GROUP BY s',
              'SELECT s, count(*), sum(a) FROM h_files GROUP BY s') AS grouped;
EXPLAIN (COSTS OFF) SELECT o.x, t.a FROM (VALUES (0), (1), (2)) o(x),
    LATERAL (SELECT a FROM t_files WHERE a % 10 = o.x OFFSET 0) t;
SELECT differ('SELECT o.x, t.a FROM (VALUES (0), (1), (2)) o(x),
                   LATERAL (SELECT a FROM t_files WHERE a % 10 = o.x OFFSET 0) t',
              'SELECT o.x, t.a FROM (VALUES (0), (1), (2)) o(x),
                   LATERAL (SELECT a FROM h_files WHERE a % 10 = o.x OFFSET 0) t') AS nested_loop;
RESET tessera.enable;
-- A group of 3000 rows read in three windows, text among the values,
-- against a heap table; and a file of rows between a file of no group
-- and one of a group of no rows.
SELECT parquet_test_write(:'dir' || '/windows.parquet', json_build_object(
    'group_rows', 3000, 'page_rows', 100, 'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int64', 'range', 3000),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE WHEN i % 7 = 0 THEN NULL ELSE 'v' || i END ORDER BY i)
         FROM generate_series(0, 2999) i))))::text);
SELECT parquet_test_write(:'dir' || format('/%s.parquet', f),
    format('{"no_group": %s, "columns": [{"name": "a", "type": "int32", "values": []},
                  {"name": "s", "type": "bytes", "values": []}]}', (f = 'no_group')::text))
FROM unnest(ARRAY['no_group', 'empty_group']) f;
\set windows :dir '/windows.parquet'
\set empty :dir '/no_group.parquet,' :dir '/one.parquet,' :dir '/empty_group.parquet'
CREATE FOREIGN TABLE t_windows (i bigint, s text) SERVER pq OPTIONS (files :'windows');
CREATE FOREIGN TABLE t_empty (a int, s text) SERVER pq OPTIONS (files :'empty');
CREATE TABLE h_windows AS
SELECT i::bigint, CASE WHEN i % 7 = 0 THEN NULL ELSE 'v' || i END AS s FROM generate_series(0, 2999) i;
SET tessera.enable = off;
SELECT differ('TABLE t_windows', 'TABLE h_windows') AS three_windows;
SELECT a, s FROM t_empty;
SELECT count(*) FROM t_empty;
RESET tessera.enable;
SELECT differ('TABLE t_windows', 'TABLE h_windows') AS three_windows;
SELECT a, s FROM t_empty;
SELECT count(*) FROM t_empty;

-- spec: parquet-fdw/ClickBench's types
-- ClickBench's types: 16-bit integers, days in an unsigned 16-bit INT32,
-- seconds in an INT64, 64-bit integers and unmarked strings, with NULL
-- values.
SELECT parquet_test_write(current_setting('parquet.dir') || '/clickbench.parquet',
    '{"group_rows": 2, "columns": [
      {"name": "AdvEngineID", "type": "int32", "logical": "int16", "values": [0, 3, null, -7]},
      {"name": "EventDate", "type": "int32", "logical": "uint16", "values": [15901, null, 15900, 0]},
      {"name": "EventTime", "type": "int64", "values": [1373832000, 1373918399, null, 0]},
      {"name": "UserID", "type": "int64", "values": [-1, 9223372036854775807, 0, null]},
      {"name": "URL", "type": "bytes", "values": ["http://a", "", null, "яндекс"]}]}');
\set clickbench :dir '/clickbench.parquet'
CREATE FOREIGN TABLE t_clickbench (
    "AdvEngineID" smallint, "EventDate" date OPTIONS (unit 'days'),
    "EventTime" timestamp OPTIONS (unit 'seconds'), "UserID" bigint, "URL" text)
    SERVER pq OPTIONS (files :'clickbench');
CREATE TABLE h_clickbench (
    "AdvEngineID" smallint, "EventDate" date, "EventTime" timestamp, "UserID" bigint, "URL" text);
INSERT INTO h_clickbench VALUES
    (0, '2013-07-15', '2013-07-14 20:00:00', -1, 'http://a'),
    (3, NULL, '2013-07-15 19:59:59', 9223372036854775807, ''),
    (NULL, '2013-07-14', NULL, 0, NULL),
    (-7, '1970-01-01', '1970-01-01 00:00:00', NULL, 'яндекс');
SET tessera.enable = off;
SELECT * FROM t_clickbench;
SELECT differ('TABLE t_clickbench', 'TABLE h_clickbench') AS rows_differ;
RESET tessera.enable;
SELECT differ('TABLE t_clickbench', 'TABLE h_clickbench') AS rows_differ;

-- spec: parquet-fdw/Every mapping of the list
-- Every other physical type and mark of the list, through the option
-- column where the names differ, in required and optional columns, the
-- file's metadata keeping an Arrow schema that names other types. The
-- writer gives a logical type its older mark too: ms is marked not
-- adjusted to UTC and TIMESTAMP_MILLIS, and the logical type decides.
SELECT parquet_test_write(current_setting('parquet.dir') || '/mappings.parquet',
    '{"arrow_schema": [{"name": "b", "type": "utf8"}, {"name": "u8", "type": "utf8"}],
      "columns": [
      {"name": "b", "type": "bool", "values": [true, false, null]},
      {"name": "f", "type": "float", "values": [1.5, -0.0, null]},
      {"name": "d", "type": "double", "values": [2.25, 1e300, null]},
      {"name": "u8", "type": "int32", "logical": "uint8", "values": [255, 0, null]},
      {"name": "u16", "type": "int32", "logical": "uint16", "values": [65535, 0, 1]},
      {"name": "u32", "type": "int32", "logical": "uint32", "values": [-1, 0, 1]},
      {"name": "day", "type": "int32", "logical": "date", "values": [0, 15901, null]},
      {"name": "ms", "type": "int64", "logical": "timestamp_ms", "values": [1500, 0, null]},
      {"name": "ms_utc", "type": "int64", "logical": "timestamp_ms_utc", "values": [1500, 0, null]},
      {"name": "us", "type": "int64", "logical": "timestamp_us", "values": [1000001, 0, null]},
      {"name": "us_utc", "type": "int64", "logical": "timestamp_us_utc", "values": [1000001, 0, null]},
      {"name": "old_ms", "type": "int64", "converted": "TIMESTAMP_MILLIS", "values": [1, 2, 3]},
      {"name": "old_us", "type": "int64", "converted": "TIMESTAMP_MICROS", "values": [1, 2, 3]},
      {"name": "str", "type": "bytes", "logical": "string", "values": ["ab", "абв", null]},
      {"name": "raw", "type": "bytes", "values": ["x", "", "zz"]},
      {"name": "fx", "type": "fixed:2", "values": ["ab", null, {"hex": "0001"}]}]}');
\set mappings :dir '/mappings.parquet'
SET TIME ZONE 'UTC';
CREATE FOREIGN TABLE t_mappings (
    b boolean, f real, f8 double precision OPTIONS (column 'f'), d double precision,
    u8 smallint, u16 integer, u32 bigint, day date, ms timestamp, ms_utc timestamptz,
    us timestamp, us_utc timestamptz, old_ms timestamptz, old_us timestamptz,
    str varchar(3), str_any varchar OPTIONS (column 'str'), raw text,
    raw_vc varchar(2) OPTIONS (column 'raw'), raw_any varchar OPTIONS (column 'raw'), fx bytea)
    SERVER pq OPTIONS (files :'mappings');
CREATE TABLE h_mappings (
    b boolean, f real, f8 double precision, d double precision,
    u8 smallint, u16 integer, u32 bigint, day date, ms timestamp, ms_utc timestamptz,
    us timestamp, us_utc timestamptz, old_ms timestamptz, old_us timestamptz,
    str varchar(3), str_any varchar, raw text, raw_vc varchar(2), raw_any varchar, fx bytea);
INSERT INTO h_mappings VALUES
    (true, 1.5, 1.5, 2.25, 255, 65535, 4294967295, '1970-01-01', '1970-01-01 00:00:01.5',
     '1970-01-01 00:00:01.5+00', '1970-01-01 00:00:01.000001', '1970-01-01 00:00:01.000001+00',
     '1970-01-01 00:00:00.001+00', '1970-01-01 00:00:00.000001+00', 'ab', 'ab', 'x', 'x', 'x',
     '\x6162'),
    (false, -0.0, -0.0, 1e300, 0, 0, 0, '2013-07-15', '1970-01-01 00:00:00',
     '1970-01-01 00:00:00+00', '1970-01-01 00:00:00', '1970-01-01 00:00:00+00',
     '1970-01-01 00:00:00.002+00', '1970-01-01 00:00:00.000002+00', 'абв', 'абв', '', '', '',
     NULL),
    (NULL, NULL, NULL, NULL, NULL, 1, 1, NULL, NULL,
     NULL, NULL, NULL, '1970-01-01 00:00:00.003+00', '1970-01-01 00:00:00.000003+00', NULL, NULL,
     'zz', 'zz', 'zz', '\x0001');
SET tessera.enable = off;
SELECT * FROM t_mappings;
SELECT differ('TABLE t_mappings', 'TABLE h_mappings') AS rows_differ;
RESET tessera.enable;
SELECT differ('TABLE t_mappings', 'TABLE h_mappings') AS rows_differ;
RESET TIME ZONE;

-- spec: parquet-fdw/Bytes checked when they become values
-- A string that is not UTF-8 and one with a zero byte, marked as a
-- string and unmarked, in plain pages and through a dictionary: checked,
-- each raises 22021; trusted, the bytes become text; as bytea, bytes.
-- Under a clause the source checks that rejects their row, the batches
-- return the other row, the dictionary's value no row passing uses
-- raising nothing.
SELECT parquet_test_write(:'dir' || format('/bad_%s.parquet', d), format('{"dictionary": %s, "columns": [
      {"name": "k", "type": "int32", "values": [1, 2]},
      {"name": "m_bad", "type": "bytes", "logical": "string", "values": ["a", {"hex": "ff"}]},
      {"name": "u_bad", "type": "bytes", "values": ["a", {"hex": "ff"}]},
      {"name": "m_zero", "type": "bytes", "logical": "string", "values": ["a", {"hex": "6100"}]},
      {"name": "u_zero", "type": "bytes", "values": ["a", {"hex": "6100"}]}]}', d))
FROM unnest(ARRAY['false', 'true']) d;
\set bad_plain :dir '/bad_false.parquet'
\set bad_dict :dir '/bad_true.parquet'
CREATE FOREIGN TABLE t_check_plain (k int, m_bad text, u_bad text, m_zero text, u_zero text)
    SERVER pq OPTIONS (files :'bad_plain');
CREATE FOREIGN TABLE t_check_dict (k int, m_bad text, u_bad text, m_zero text, u_zero text)
    SERVER pq OPTIONS (files :'bad_dict');
CREATE FOREIGN TABLE t_trust (m_bad text OPTIONS (column 'm_bad', utf8 'trust'),
    u_zero text OPTIONS (column 'u_zero', utf8 'trust'))
    SERVER pq OPTIONS (files :'bad_dict');
CREATE FOREIGN TABLE t_bytes (m_bad bytea, u_bad bytea, m_zero bytea, u_zero bytea)
    SERVER pq OPTIONS (files :'bad_plain');
SET tessera.enable = off;
SELECT t, c, sqlstate_of(format('SELECT %I FROM %I', c, t)) AS checked
FROM unnest(ARRAY['t_check_plain', 't_check_dict']) t,
     unnest(ARRAY['m_bad', 'u_bad', 'm_zero', 'u_zero']) c ORDER BY t, c;
SELECT encode(textsend(m_bad), 'hex') AS m_bad, encode(textsend(u_zero), 'hex') AS u_zero
FROM t_trust;
SELECT * FROM t_bytes;
RESET tessera.enable;
SELECT scans_of('SELECT m_bad FROM t_check_plain') AS through;
SELECT t, c, sqlstate_of(format('SELECT %I FROM %I', c, t)) AS checked
FROM unnest(ARRAY['t_check_plain', 't_check_dict']) t,
     unnest(ARRAY['m_bad', 'u_bad', 'm_zero', 'u_zero']) c ORDER BY t, c;
SELECT encode(textsend(m_bad), 'hex') AS m_bad, encode(textsend(u_zero), 'hex') AS u_zero
FROM t_trust;
SELECT * FROM t_bytes;
SELECT checked_by('SELECT m_bad FROM t_check_dict WHERE k = 1') AS checked_by;
SELECT t, c, sqlstate_of(format('SELECT %I FROM %I WHERE k = 1', c, t)) AS under_a_clause
FROM unnest(ARRAY['t_check_plain', 't_check_dict']) t,
     unnest(ARRAY['m_bad', 'u_bad', 'm_zero', 'u_zero']) c ORDER BY t, c;
SELECT m_bad, u_bad, m_zero, u_zero FROM t_check_dict WHERE k = 1;

-- spec: parquet-fdw/The length of a varchar
-- Into varchar(3): three characters of two bytes each are returned, four
-- characters raise 22001, and a trusted varchar(3) is refused, 42804.
-- Under a clause the source checks that rejects the row of four, its
-- batches return the other row.
SELECT parquet_test_write(:'dir' || '/vc_three.parquet',
    '{"columns": [{"name": "s", "type": "bytes", "logical": "string", "values": ["абв", "ab"]}]}');
SELECT parquet_test_write(:'dir' || '/vc_four.parquet',
    '{"columns": [{"name": "s", "type": "bytes", "values": ["абв", "abcd"]},
                  {"name": "k", "type": "int32", "values": [1, 2]}]}');
\set vc_three :dir '/vc_three.parquet'
\set vc_four :dir '/vc_four.parquet'
CREATE FOREIGN TABLE t_vc_three (s varchar(3)) SERVER pq OPTIONS (files :'vc_three');
CREATE FOREIGN TABLE t_vc_four (s varchar(3), k int) SERVER pq OPTIONS (files :'vc_four');
CREATE FOREIGN TABLE t_vc_trust (s varchar(3) OPTIONS (column 's', utf8 'trust'))
    SERVER pq OPTIONS (files :'vc_three');
SET tessera.enable = off;
SELECT s, octet_length(s) FROM t_vc_three;
SELECT sqlstate_of('SELECT s FROM t_vc_four') AS four_characters;
SELECT sqlstate_of('EXPLAIN SELECT s FROM t_vc_trust') AS trusted_varchar;
RESET tessera.enable;
SELECT scans_of('SELECT s FROM t_vc_four') AS through;
SELECT s, octet_length(s) FROM t_vc_three;
SELECT sqlstate_of('SELECT s FROM t_vc_four') AS four_characters;
SELECT sqlstate_of('EXPLAIN SELECT s FROM t_vc_trust') AS trusted_varchar;
SELECT scans_of('SELECT s FROM t_vc_four WHERE k = 1') AS under_a_clause;
SELECT s FROM t_vc_four WHERE k = 1;
-- contract: parquet-fdw/text checked by ANALYZE
-- ANALYZE makes the values of its sample by each column's check too: a
-- string that is not UTF-8, in plain pages and through a dictionary, and
-- one longer than its varchar fail it.
SELECT t, sqlstate_of(format('ANALYZE %I', t)) AS analyzed
FROM unnest(ARRAY['t_check_plain', 't_check_dict', 't_vc_four']) t ORDER BY t;

-- spec: parquet-fdw/A chunk that leaves its dictionary
-- Two groups of 2 000 rows whose string chunks begin through a small
-- dictionary and go on in plain pages: NULLs and values of 0, 12, 13 and
-- a thousand bytes in both parts, and short ones for a varchar(3). Each
-- query runs with Tessera off and on, through the wrapper's rows and the
-- source's batches, against a heap table of the same rows. A column of
-- the same values holds bytes that are not UTF-8 in the dictionary part
-- of the first group, row 5, and in the plain part of the second, row
-- 3 500: they raise 22021 where a query returns them, and in the
-- source's batches nothing under a clause the source checks that
-- rejects them, where the wrapper's rows make every value.
CREATE FUNCTION leaves_value(i int, bad boolean) RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE WHEN bad AND i IN (5, 3500) THEN '\xff'::bytea
                WHEN i % 7 = 0 THEN NULL
                ELSE convert_to(CASE i % 7
                    WHEN 1 THEN ''
                    WHEN 2 THEN lpad(i::text, 12, 'a')
                    WHEN 3 THEN lpad(i::text, 13, 'b')
                    WHEN 4 THEN repeat('ж', 495) || lpad(i::text, 10, '0')
                    WHEN 5 THEN 'common'
                    ELSE lpad((i % 50)::text, 14, 'c') END, 'UTF8') END $$;
CREATE FUNCTION leaves_short(i int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE WHEN i % 5 = 0 THEN NULL
                WHEN i % 97 = 3 THEN '😀😀😀'
                ELSE chr(1040 + i % 32) || chr(1040 + i / 32 % 32) || (i % 10)::text END $$;
CREATE FUNCTION leaves_json(v bytea) RETURNS json LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE WHEN v IS NOT NULL THEN json_build_object('hex', encode(v, 'hex')) END $$;
SELECT parquet_test_write(:'dir' || '/leaves.parquet', json_build_object(
    'group_rows', 2000, 'page_rows', 100, 'dictionary', true, 'dictionary_bytes', 4096,
    'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32',
        'values', (SELECT json_agg(i ORDER BY i) FROM generate_series(0, 3999) i)),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg(leaves_json(leaves_value(i, false)) ORDER BY i)
         FROM generate_series(0, 3999) i)),
      json_build_object('name', 'bad', 'type', 'bytes', 'values',
        (SELECT json_agg(leaves_json(leaves_value(i, true)) ORDER BY i)
         FROM generate_series(0, 3999) i)),
      json_build_object('name', 'v', 'type', 'bytes', 'values',
        (SELECT json_agg(leaves_json(convert_to(leaves_short(i), 'UTF8')) ORDER BY i)
         FROM generate_series(0, 3999) i))))::text);
\set leaves :dir '/leaves.parquet'
CREATE FOREIGN TABLE t_leaves (i int, s text, v varchar(3), bad text)
    SERVER pq OPTIONS (files :'leaves');
CREATE FOREIGN TABLE t_leaves_bytes (i int, s bytea, bad bytea)
    SERVER pq OPTIONS (files :'leaves');
CREATE TABLE h_leaves AS
    SELECT i, convert_from(leaves_value(i, false), 'UTF8') AS s,
           leaves_short(i)::varchar(3) AS v, leaves_value(i, true) AS bad
    FROM generate_series(0, 3999) i;
CREATE FUNCTION leaves_order(o text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    of_file text[];
    of_heap text[];
BEGIN
    EXECUTE format('SELECT array_agg(q::text) FROM (SELECT s, i FROM t_leaves ORDER BY %s) q', o)
        INTO of_file;
    EXECUTE format('SELECT array_agg(q::text) FROM (SELECT s, i FROM h_leaves ORDER BY %s) q', o)
        INTO of_heap;
    RETURN of_file IS NOT DISTINCT FROM of_heap;
END $$;
CREATE TABLE leaves_queries (name text, of_file text, of_heap text);
INSERT INTO leaves_queries VALUES
    ('bytea', 'SELECT i, s, bad FROM t_leaves_bytes',
     $q$SELECT i, convert_to(s, 'UTF8'), bad FROM h_leaves$q$),
    ('text', 'SELECT i, s FROM t_leaves', 'SELECT i, s FROM h_leaves'),
    ('grouped', 'SELECT s, count(*) FROM t_leaves GROUP BY s',
     'SELECT s, count(*) FROM h_leaves GROUP BY s'),
    ('length', 'SELECT i, length(s), octet_length(s) FROM t_leaves',
     'SELECT i, length(s), octet_length(s) FROM h_leaves'),
    ('ordered', $q$SELECT * FROM (SELECT s, i FROM t_leaves
        ORDER BY s COLLATE "C" DESC NULLS LAST, i LIMIT 30) o$q$,
     $q$SELECT * FROM (SELECT s, i FROM h_leaves
        ORDER BY s COLLATE "C" DESC NULLS LAST, i LIMIT 30) o$q$),
    ('varchar', 'SELECT i, v FROM t_leaves', 'SELECT i, v FROM h_leaves'),
    ('varchar grouped', 'SELECT v, count(*) FROM t_leaves GROUP BY v',
     'SELECT v, count(*) FROM h_leaves GROUP BY v');
SET tessera.enable = off;
SELECT name, differ(of_file, of_heap) AS rows_differ FROM leaves_queries ORDER BY name;
SELECT q, sqlstate_of(format('SELECT bad FROM t_leaves WHERE %s', q)) AS bad_bytes
FROM unnest(ARRAY['i = 5', 'i = 3500', 'i <> 5 AND i <> 3500']) q;
SELECT o, leaves_order(o) AS same_order
FROM unnest(ARRAY['s COLLATE "C" DESC NULLS LAST, i LIMIT 30',
                  's COLLATE "C" NULLS FIRST, i LIMIT 30']) o;
RESET tessera.enable;
SELECT scans_of('SELECT s FROM t_leaves') AS through,
       checked_by('SELECT bad FROM t_leaves WHERE i <> 5 AND i <> 3500') AS checked_by;
SELECT name, differ(of_file, of_heap) AS rows_differ FROM leaves_queries ORDER BY name;
SELECT q, sqlstate_of(format('SELECT bad FROM t_leaves WHERE %s', q)) AS bad_bytes
FROM unnest(ARRAY['i = 5', 'i = 3500', 'i <> 5 AND i <> 3500', 'true']) q;
SELECT o, leaves_order(o) AS same_order
FROM unnest(ARRAY['s COLLATE "C" DESC NULLS LAST, i LIMIT 30',
                  's COLLATE "C" NULLS FIRST, i LIMIT 30']) o;
SELECT differ('SELECT i, bad FROM t_leaves WHERE i <> 5 AND i <> 3500',
              $q$SELECT i, convert_from(bad, 'UTF8') FROM h_leaves WHERE i <> 5 AND i <> 3500$q$)
       AS rows_differ_under_the_clause;

-- spec: parquet-fdw/Options refused
-- A column compressed by a codec other than Snappy is refused, its
-- neighbours planned.
SELECT parquet_test_write(current_setting('parquet.dir') || '/gzip.parquet',
    '{"compression": "snappy", "columns": [{"name": "a", "type": "int32", "values": [1, 2]},
                  {"name": "b", "type": "int32", "values": [3, 4]}]}');
SELECT parquet_test_rewrite(current_setting('parquet.dir') || '/gzip.parquet', false, false, false, 1, -1);
CREATE FOREIGN TABLE t_gzip (a int, b int) SERVER pq OPTIONS (files :'gzip');
SELECT sqlstate_of('EXPLAIN SELECT b FROM t_gzip') AS gzip_column;
SELECT sqlstate_of('EXPLAIN SELECT a FROM t_gzip') AS snappy_beside_gzip;
SELECT a FROM t_gzip;

-- spec: parquet-fdw/Columns refused
-- The columns refused, in the order of the spec's requirement, at
-- planning; a query that reads only the other columns is planned.
SELECT parquet_test_write(current_setting('parquet.dir') || '/types.parquet',
    '{"columns": [
      {"name": "i", "type": "int32", "values": [1, 2]},
      {"name": "l", "type": "int64", "values": [1, 2]},
      {"name": "day", "type": "int32", "logical": "date", "values": [1, 2]},
      {"name": "ts", "type": "int64", "logical": "timestamp_us", "values": [1, 2]},
      {"name": "s", "type": "bytes", "values": ["a", "b"]},
      {"name": "dec", "type": "int32", "logical": "decimal:9:2", "values": [1, 2]},
      {"name": "big", "type": "bytes", "logical": "decimal:300:0", "values": ["1", "2"]},
      {"name": "tod", "type": "int64", "logical": "time_us", "values": [1, 2]},
      {"name": "ns", "type": "int64", "logical": "timestamp_ns", "values": [1, 2]},
      {"name": "json", "type": "bytes", "logical": "json", "values": ["1", "2"]},
      {"name": "t96", "type": "int96", "values": [[1, 2, 3], [4, 5, 6]]},
      {"name": "lst", "type": "int32", "list": true, "values": [1, 2]}]}');
CREATE FOREIGN TABLE t_types (
    i int, l bigint, day date, ts timestamp, s text, dec int, big bytea, tod bigint,
    ns timestamp, json text, t96 timestamp, lst int, missing int, "I" int,
    n numeric OPTIONS (column 'i'), t0 timestamp(0) OPTIONS (column 'ts'),
    s_unit text OPTIONS (column 's', unit 'days'), i_utf8 int OPTIONS (column 'i', utf8 'check'),
    day_days date OPTIONS (column 'day', unit 'days'),
    ts_seconds timestamp OPTIONS (column 'ts', unit 'seconds'),
    l_days date OPTIONS (column 'l', unit 'days'),
    i_seconds timestamp OPTIONS (column 'i', unit 'seconds'),
    l_small smallint OPTIONS (column 'l'),
    n_days numeric OPTIONS (column 'i', unit 'days'),
    l_utf8 smallint OPTIONS (column 'l', utf8 'check'),
    i_text text OPTIONS (column 'i'), i_text_check text OPTIONS (column 'i', utf8 'check'))
    SERVER pq OPTIONS (files :'types');
SELECT c AS column_read, sqlstate_of(format('EXPLAIN SELECT %I FROM t_types', c)) AS sqlstate
FROM unnest(ARRAY['missing', 'I', 'dec', 'big', 'tod', 'ns', 'json', 't96', 'lst', 'n', 't0', 's_unit',
                  'i_utf8', 'day_days', 'ts_seconds', 'l_days', 'i_seconds', 'l_small',
                  'n_days', 'l_utf8', 'i_text', 'i_text_check', 'i', 'l', 'day', 'ts', 's']) c;
-- A query that reads none of the refused columns, and one that counts.
SELECT sqlstate_of('EXPLAIN SELECT i, l, day, ts, s FROM t_types') AS other_columns;
SELECT sqlstate_of('EXPLAIN SELECT count(*) FROM t_types') AS count_reads_none;
-- Their rows, and the count, with Tessera off and on.
SET tessera.enable = off;
SELECT i, l, day, ts, s FROM t_types;
SELECT count(*) FROM t_types;
RESET tessera.enable;
SELECT i, l, day, ts, s FROM t_types;
SELECT count(*) FROM t_types;
-- A time in seconds past the range of timestamp, 22008.
SELECT parquet_test_write(:'dir' || '/far.parquet',
    '{"columns": [{"name": "t", "type": "int64", "values": [0, 9223372036854775807]}]}');
\set far :dir '/far.parquet'
CREATE FOREIGN TABLE t_far (t timestamp OPTIONS (unit 'seconds')) SERVER pq OPTIONS (files :'far');
SELECT sqlstate_of('SELECT t FROM t_far') AS past_the_range;

-- A whole row reads every column but those dropped.
CREATE FOREIGN TABLE t_drop (i int, gone int, l bigint) SERVER pq OPTIONS (files :'types');
ALTER FOREIGN TABLE t_drop DROP COLUMN gone;
SELECT sqlstate_of('EXPLAIN SELECT t_drop FROM t_drop') AS whole_row_after_a_drop;
-- A scan under a lateral reference of an outer join is planned.
SELECT sqlstate_of('EXPLAIN SELECT x.i, s.* FROM t_drop x
    LEFT JOIN LATERAL (SELECT x.i AS xi, y.l FROM t_drop y) s ON true') AS lateral_outer_join;

-- spec: parquet-fdw/A database of another encoding
-- A database of the encoding LATIN1: text is refused when it is planned;
-- integers and bytes, and a column of a name that is not ASCII, are
-- planned.
SELECT parquet_test_write(current_setting('parquet.dir') || '/latin.parquet',
    '{"columns": [{"name": "i", "type": "int32", "values": [1]},
                  {"name": "é", "type": "int32", "values": [2]},
                  {"name": "s", "type": "bytes", "values": ["x"]}]}');
\set latin :dir '/latin.parquet'
\set main_db :DBNAME
CREATE DATABASE parquet_latin1 ENCODING 'LATIN1' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
\c parquet_latin1
CREATE EXTENSION tessera;
CREATE EXTENSION tessera_parquet;
CREATE FUNCTION sqlstate_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE statement;
    RETURN 'ok';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE;
END $$;
CREATE SERVER pq FOREIGN DATA WRAPPER tessera_parquet;
CREATE FOREIGN TABLE t_latin (i int, "é" int, s text, b bytea OPTIONS (column 's'), "è" int)
    SERVER pq OPTIONS (files :'latin');
SELECT sqlstate_of('EXPLAIN SELECT i, "é", b FROM t_latin') AS integers_and_bytes;
SELECT i, "é", b FROM t_latin;
SELECT sqlstate_of('EXPLAIN SELECT s FROM t_latin') AS text_in_latin1;
-- The reader's message, of a name it quotes, in the database's encoding.
EXPLAIN SELECT "è" FROM t_latin;
\c :main_db
-- A new backend reads a table before the bridge is loaded: the wrapper
-- alone; once the bridge and the nodes are loaded, the source plans.
SELECT count(*) FROM t_files;
LOAD 'tessera';
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
SELECT scans_of('SELECT a FROM t_files') AS after_the_bridge;
DROP DATABASE parquet_latin1;
SELECT set_config('parquet.dir', :'dir', false) IS NOT NULL AS dir_set_again;

-- spec: parquet-fdw/Batch parents over the source
-- A grouping, a sort under a limit and a hash join over a table of ten
-- columns and 100 000 rows, with Tessera on: each through
-- TessParquetScan, with no TessPack, and each equal to the result with
-- Tessera off.
SELECT parquet_test_write(:'dir' || '/wide.parquet', json_build_object(
    'group_rows', 30000, 'columns', json_build_array(
      json_build_object('name', 'k', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'a', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'b', 'type', 'int32', 'values',
        (SELECT json_agg(i % 3 ORDER BY i) FROM generate_series(0, 99999) i)),
      json_build_object('name', 'v', 'type', 'int64', 'range', 100000),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE WHEN i % 5 = 0 THEN NULL ELSE 'w' || i END ORDER BY i)
         FROM generate_series(0, 99999) i)),
      json_build_object('name', 'c1', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'c2', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'c3', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'c4', 'type', 'int64', 'range', 100000),
      json_build_object('name', 'c5', 'type', 'int64', 'range', 100000)))::text);
\set wide :dir '/wide.parquet'
CREATE FOREIGN TABLE t_wide (k bigint, a bigint, b int, v bigint, s text,
    c1 bigint, c2 bigint, c3 bigint, c4 bigint, c5 bigint) SERVER pq OPTIONS (files :'wide');
CREATE TABLE h_wide AS
SELECT i::bigint AS k, i::bigint AS a, i % 3 AS b, i::bigint AS v,
       CASE WHEN i % 5 = 0 THEN NULL ELSE 'w' || i END AS s,
       i::bigint AS c1, i::bigint AS c2, i::bigint AS c3, i::bigint AS c4, i::bigint AS c5
FROM generate_series(0, 99999) i;
CREATE TABLE h_keys AS SELECT i * 7::bigint AS k FROM generate_series(0, 999) i;
ANALYZE h_wide, h_keys;
SELECT scans_of('SELECT b, count(*), sum(v) FROM t_wide GROUP BY b') AS grouping,
       scans_of('SELECT * FROM t_wide ORDER BY k LIMIT 10') AS sort_under_a_limit,
       scans_of('SELECT count(*), sum(t.v) FROM t_wide t JOIN h_keys h ON t.k = h.k') AS hash_join;
SELECT differ_off('SELECT b, count(*), sum(v) FROM t_wide GROUP BY b') AS grouping,
       differ_off('(SELECT * FROM t_wide ORDER BY k LIMIT 10)') AS sort_under_a_limit,
       differ_off('SELECT count(*), sum(t.v) FROM t_wide t JOIN h_keys h ON t.k = h.k') AS hash_join;

-- spec: parquet-fdw/Columns made for the rows kept
-- The sort, top-N where its heap fits work_mem, makes every value of
-- its column, 100 000, and of the nine others only those of the rows it
-- took in, the first batch's 64; and the rows equal those with Tessera
-- off.
SET work_mem = '64MB';
SELECT explain_lines('ANALYZE', 'SELECT * FROM t_wide ORDER BY k LIMIT 10',
                     'TessParquetScan|Sort Method|Values Decoded');
SELECT differ_off('(SELECT * FROM t_wide ORDER BY k LIMIT 10)') AS rows_differ;
RESET work_mem;
-- A clause the source checks keeps eight rows, in windows after gaps,
-- two in one batch and across a group's end: the ten columns are made for
-- those alone, each read forward over the selection.
SELECT scans_of('SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)')
    AS filtered;
SELECT explain_lines('ANALYZE',
    'SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)',
    'TessParquetScan|Values Decoded');
SELECT differ_off('SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)')
    AS rows_differ;
-- The same clause by a function of the column, which TessFilter checks:
-- the column is made for every row, the nine others for the rows kept,
-- each read for the rows asked where its reader passed no window.
SELECT explain_lines('ANALYZE',
    'SELECT * FROM t_wide WHERE k + 0 IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)',
    'TessParquetScan|Values Decoded');
SELECT differ_off('SELECT * FROM t_wide WHERE k + 0 IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)')
    AS rows_differ;
-- A clause the source checks keeps part of a window, 1100 rows: the
-- second window's 76 in a batch of 64 and its short last of 12, whose
-- rows take their places in the window's selection from the rank of the
-- batch's first. The five rows kept are of the short batch.
SET work_mem = '64MB';
SELECT explain_lines('ANALYZE', 'SELECT * FROM t_wide WHERE k < 1100 ORDER BY v DESC LIMIT 5',
                     'TessParquetScan|Source Exact|Values Decoded');
SELECT differ_off('(SELECT * FROM t_wide WHERE k < 1100 ORDER BY v DESC LIMIT 5)') AS rows_differ;
RESET work_mem;
-- A count over a file whose data pages are damaged and whose footer is
-- whole makes no value and returns the footer's count.
SELECT parquet_test_write(:'dir' || '/damaged_pages.parquet',
    '{"columns": [{"name": "i", "type": "int64", "range": 3000}]}');
SELECT parquet_test_damage(:'dir' || '/damaged_pages.parquet', 4, 16);
\set damaged_pages :dir '/damaged_pages.parquet'
CREATE FOREIGN TABLE t_damaged_pages (i bigint) SERVER pq OPTIONS (files :'damaged_pages');
SELECT explain_lines('ANALYZE', 'SELECT count(*) FROM t_damaged_pages',
                     'TessParquetScan|Values Decoded');
SELECT count(*) FROM t_damaged_pages;
-- Its pages are damaged: reading a value raises XX001.
SELECT sqlstate_of('SELECT sum(i) FROM t_damaged_pages') AS pages_damaged;

-- spec: parquet-fdw/One column asked again
-- A batch asked for a and for v more than once, for rows that do not
-- nest and in reverse order, against the heap table.
SELECT scans_of('SELECT sum(a) FILTER (WHERE b = 1) FROM t_wide') AS filtered_sums;
SELECT differ('SELECT sum(a) FILTER (WHERE b = 1), sum(a) FILTER (WHERE b = 2),
                      sum(CASE WHEN k % 64 = 63 THEN v ELSE v + 1 END) FROM t_wide',
              'SELECT sum(a) FILTER (WHERE b = 1), sum(a) FILTER (WHERE b = 2),
                      sum(CASE WHEN k % 64 = 63 THEN v ELSE v + 1 END) FROM h_wide') AS rows_differ;

-- The node scanned again for each outer row of a correlated subquery:
-- every scan reads the group again, and the counts of EXPLAIN total the
-- scans.
SELECT explain_lines('ANALYZE',
    'SELECT x, (SELECT count(*) FROM t_windows t WHERE t.i % 10 = o.x) FROM (VALUES (1), (2), (3)) o(x)',
    'TessParquetScan|Row Groups Read|Values Decoded');
SELECT differ_off('SELECT x, (SELECT count(*) FROM t_windows t WHERE t.i % 10 = o.x) AS n,
                          (SELECT max(s) FROM t_windows t WHERE t.i % 10 = o.x) AS s
                   FROM (VALUES (1), (2), (3)) o(x)') AS rows_differ;

-- spec: parquet-fdw/A whole row and a system column
-- A whole row and a system column, which the runtime's projection does
-- not compute, are read by the wrapper's scan with Tessera on.
SELECT scans_of('SELECT t FROM t_files t') AS whole_row,
       scans_of('SELECT tableoid::regclass, a FROM t_files') AS system_column;
SELECT differ_off('SELECT t::text AS row FROM t_files t') AS whole_row,
       differ_off('SELECT tableoid::regclass::text AS rel, a FROM t_files') AS system_column;

-- spec: parquet-fdw/Values kept across the asks of a batch
-- The test module asks one batch of the node for an integer, a time and
-- a text column, each with NULL rows, for row 63, then rows 0 to 62, then
-- row 63 again, keeping the arrays each ask gave, row 63 taken out of
-- the batch's mask before it is asked: every ask gives the rows it
-- asked, and the first ask's arrays still hold row 63, NULL in one
-- column. Then, in batches of 10 rows, the batch of rows 60 to 69,
-- across a word of the window's masks, the same way.
CREATE FUNCTION parquet_test_asks(text, int, int, bool DEFAULT false)
    RETURNS TABLE (col text, n int, ask text, value text)
    AS :'parquet_test', 'tessera_parquet_test_asks' LANGUAGE C STRICT;
SELECT parquet_test_write(:'dir' || '/asks.parquet', json_build_object('columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32', 'values',
        (SELECT json_agg(CASE WHEN n % 7 = 0 THEN NULL ELSE n END ORDER BY n)
         FROM generate_series(0, 99) n)),
      json_build_object('name', 't', 'type', 'int64', 'logical', 'timestamp_us', 'values',
        (SELECT json_agg(CASE WHEN n % 5 = 2 THEN NULL ELSE n * 1000000 END ORDER BY n)
         FROM generate_series(0, 99) n)),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE WHEN n % 6 = 1 THEN NULL ELSE 's' || n END ORDER BY n)
         FROM generate_series(0, 99) n))))::text);
\set asks :dir '/asks.parquet'
CREATE FOREIGN TABLE t_asks (i int, t timestamp, s text) SERVER pq OPTIONS (files :'asks');
CREATE TABLE h_asks AS
SELECT n, CASE WHEN n % 7 = 0 THEN NULL ELSE n END AS i,
       CASE WHEN n % 5 = 2 THEN NULL ELSE '1970-01-01'::timestamp + n * interval '1 second' END AS t,
       CASE WHEN n % 6 = 1 THEN NULL ELSE 's' || n END AS s
FROM generate_series(0, 99) n;
SELECT max_rows, count(*) AS asked, count(*) FILTER (WHERE value IS NULL) AS null_rows,
       count(*) FILTER (WHERE ask = 'kept' AND value IS NULL) AS kept_null
FROM (VALUES (64, 0), (10, 6)) b(max_rows, skip),
     parquet_test_asks('SELECT i, t, s FROM t_asks', max_rows, skip)
GROUP BY max_rows ORDER BY max_rows;
SELECT max_rows, differ(
    format($$SELECT col, n, ask, value FROM parquet_test_asks('SELECT i, t, s FROM t_asks', %s, %s)$$,
           max_rows, skip),
    format($$(SELECT c, n, a, CASE c WHEN 'i' THEN i::text WHEN 't' THEN t::text ELSE s END
              FROM h_asks, unnest(ARRAY['i', 't', 's']) c,
                   unnest(ARRAY['first', 'kept', 'again']) a WHERE n = %s
              UNION ALL
              SELECT c, n, 'second', CASE c WHEN 'i' THEN i::text WHEN 't' THEN t::text ELSE s END
              FROM h_asks, unnest(ARRAY['i', 't', 's']) c WHERE n >= %s AND n < %s)$$,
           first + max_rows - 1, first, first + max_rows - 1)) AS asks_differ
FROM (VALUES (64, 0, 0), (10, 6, 60)) b(max_rows, skip, first) ORDER BY max_rows;
-- The same batch of ten after the six before it were asked for every
-- row of each column: those rows were made at the checked batch's places,
-- where it makes its own again.
SELECT differ(
    $$SELECT col, n, ask, value FROM parquet_test_asks('SELECT i, t, s FROM t_asks', 10, 6, true)$$,
    $$(SELECT c, n, a, CASE c WHEN 'i' THEN i::text WHEN 't' THEN t::text ELSE s END
       FROM h_asks, unnest(ARRAY['i', 't', 's']) c,
            unnest(ARRAY['first', 'kept', 'again']) a WHERE n = 69
       UNION ALL
       SELECT c, n, 'second', CASE c WHEN 'i' THEN i::text WHEN 't' THEN t::text ELSE s END
       FROM h_asks, unnest(ARRAY['i', 't', 's']) c WHERE n >= 60 AND n < 69)$$) AS asks_differ;
-- Columns first asked in a batch of three rows, a group of its own, and
-- then in the full batches of the next file, up to row 63 of the first:
-- as values for the rows returned, and s in the bytes form for the
-- grouping by it.
SELECT parquet_test_write(:'dir' || '/short.parquet', json_build_object(
    'group_rows', 3, 'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32', 'values', json_build_array(-1, NULL, -3)),
      json_build_object('name', 't', 'type', 'int64', 'logical', 'timestamp_us',
                        'values', json_build_array(NULL, -1000000, -2000000)),
      json_build_object('name', 's', 'type', 'bytes',
                        'values', json_build_array('a', 'b', NULL))))::text);
\set short :dir '/short.parquet,' :asks
CREATE FOREIGN TABLE t_short (i int, t timestamp, s text) SERVER pq OPTIONS (files :'short');
CREATE TABLE h_short AS
SELECT * FROM (VALUES (-1, NULL, 'a'), (NULL, '1969-12-31 23:59:59'::timestamp, 'b'),
                      (-3, '1969-12-31 23:59:58', NULL)) v(i, t, s)
UNION ALL SELECT i, t, s FROM h_asks;
SELECT scans_of('SELECT i, t, s FROM t_short') AS rows_returned,
       scans_of('SELECT s, count(*), sum(i), max(t) FROM t_short GROUP BY s') AS grouped;
SELECT differ('SELECT i, t, s FROM t_short', 'SELECT i, t, s FROM h_short') AS rows_differ,
       differ('SELECT s, count(*), sum(i), max(t) FROM t_short GROUP BY s',
              'SELECT s, count(*), sum(i), max(t) FROM h_short GROUP BY s') AS groups_differ;
DROP FOREIGN TABLE t_short;
DROP TABLE h_short;

-- spec: string-bytes/Indexes of a dictionary chunk
-- The bytes form of the node's batches, which TessAgg and the kernels
-- read: the test module asks one batch's text, bytea and varchar columns
-- as bytes for every other row, then every row as Datum values and the
-- rest as bytes, and reads the first ask's places again; an int column
-- is not offered. Over a file of plain pages and one through a
-- dictionary, with NULL rows, empty strings and characters of several
-- bytes, in batches of 64 and of 10 across a word of the window's masks:
-- every row's bytes are its value's, the first ask's still after the
-- others, and the rows through a dictionary carry indexes, which the
-- module checks lie among the dictionary's values, one a value.
CREATE FUNCTION parquet_test_bytes(text, int, int, bigint, bool, bool, bool DEFAULT false)
    RETURNS TABLE (col text, n int, ask text, bytes bytea, ind int, dict bool)
    AS :'parquet_test', 'tessera_parquet_test_bytes' LANGUAGE C STRICT;
SELECT parquet_test_write(:'dir' || '/bytes_' || f || '.parquet', json_build_object(
    'dictionary', f = 'dict', 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE WHEN n % 6 = 1 THEN NULL WHEN n % 10 = 3 THEN ''
                              WHEN n % 7 = 4 THEN 'é' || n % 5 ELSE 's' || n % 13 END ORDER BY n)
         FROM generate_series(0, 99) n)),
      json_build_object('name', 'i', 'type', 'int32', 'values',
        (SELECT json_agg(n ORDER BY n) FROM generate_series(0, 99) n)),
      json_build_object('name', 'b', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE WHEN n % 9 = 0 THEN NULL ELSE 'b' || n % 4 END ORDER BY n)
         FROM generate_series(0, 99) n)),
      json_build_object('name', 'v', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg('v' || n % 3 ORDER BY n) FROM generate_series(0, 99) n))))::text)
FROM (VALUES ('plain'), ('dict')) t(f);
\set bytes_plain :dir '/bytes_plain.parquet'
\set bytes_dict :dir '/bytes_dict.parquet'
CREATE FOREIGN TABLE t_bytes_plain (s text, i int, b bytea, v varchar(3))
    SERVER pq OPTIONS (files :'bytes_plain');
CREATE FOREIGN TABLE t_bytes_dict (s text, i int, b bytea, v varchar(3))
    SERVER pq OPTIONS (files :'bytes_dict');
CREATE TABLE h_bytes AS
SELECT n, CASE WHEN n % 6 = 1 THEN NULL WHEN n % 10 = 3 THEN ''
               WHEN n % 7 = 4 THEN 'é' || n % 5 ELSE 's' || n % 13 END AS s,
       CASE WHEN n % 9 = 0 THEN NULL ELSE ('b' || n % 4)::bytea END AS b,
       ('v' || n % 3)::varchar(3) AS v
FROM generate_series(0, 99) n;
SELECT tbl, max_rows, col, count(*) FILTER (WHERE ask <> 'not offered') AS asked,
       count(*) FILTER (WHERE ask = 'not offered') AS not_offered,
       count(*) FILTER (WHERE bytes IS NULL AND ask <> 'not offered') AS null_rows,
       bool_or(dict) AS dict,
       CASE WHEN bool_or(dict) THEN
         count(DISTINCT (ind, bytes)) FILTER (WHERE bytes IS NOT NULL) = count(DISTINCT ind)
         AND count(DISTINCT ind) = count(DISTINCT bytes) END AS one_index_a_value
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl),
     (VALUES (64, 0), (10, 6)) b(max_rows, skip),
     parquet_test_bytes(format('SELECT s, i, b, v FROM %s', tbl), max_rows, skip,
                        x'5555555555555555'::bigint, true, false)
GROUP BY tbl, max_rows, col ORDER BY tbl, max_rows, col;
SELECT tbl, max_rows, differ(
    format($$SELECT col, n, ask, bytes FROM parquet_test_bytes('SELECT s, b, v FROM %s', %s, %s,
                                                                x'5555555555555555'::bigint, true, false)$$,
           tbl, max_rows, skip),
    format($$(SELECT c, n, a, CASE c WHEN 's' THEN convert_to(s, 'UTF8') WHEN 'b' THEN b
                                    ELSE convert_to(v, 'UTF8') END
              FROM h_bytes, unnest(ARRAY['s', 'b', 'v']) c,
                   unnest(ARRAY['first', 'kept']) a
              WHERE n >= %s AND n < %s AND (n - %s) %% 2 = 0
              UNION ALL
              SELECT c, n, 'rest', CASE c WHEN 's' THEN convert_to(s, 'UTF8') WHEN 'b' THEN b
                                        ELSE convert_to(v, 'UTF8') END
              FROM h_bytes, unnest(ARRAY['s', 'b', 'v']) c
              WHERE n >= %s AND n < %s AND (n - %s) %% 2 = 1)$$,
           first, first + max_rows, first, first, first + max_rows, first)) AS bytes_differ
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl),
     (VALUES (64, 0, 0), (10, 6, 60)) b(max_rows, skip, first)
ORDER BY tbl, max_rows;
-- The same batch of ten after the six before it were asked for every
-- row in the bytes form, at the checked batch's places.
SELECT tbl, differ(
    format($$SELECT col, n, ask, bytes FROM parquet_test_bytes('SELECT s, b, v FROM %s', 10, 6,
                                                                x'5555555555555555'::bigint, true, false,
                                                                true)$$, tbl),
    $$(SELECT c, n, a, CASE c WHEN 's' THEN convert_to(s, 'UTF8') WHEN 'b' THEN b
                             ELSE convert_to(v, 'UTF8') END
       FROM h_bytes, unnest(ARRAY['s', 'b', 'v']) c, unnest(ARRAY['first', 'kept']) a
       WHERE n >= 60 AND n < 70 AND n % 2 = 0
       UNION ALL
       SELECT c, n, 'rest', CASE c WHEN 's' THEN convert_to(s, 'UTF8') WHEN 'b' THEN b
                                 ELSE convert_to(v, 'UTF8') END
       FROM h_bytes, unnest(ARRAY['s', 'b', 'v']) c
       WHERE n >= 60 AND n < 70 AND n % 2 = 1)$$) AS bytes_differ
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl) ORDER BY tbl;
-- An ask of no row of that batch, through the dictionary the six before
-- it named: it names none, as a batch of which no ask made the column.
SELECT col, n, ask, dict
FROM parquet_test_bytes('SELECT s FROM t_bytes_dict', 10, 6, 0, false, false, true);
-- spec: string-bytes/Bytes that are not UTF-8
-- spec: string-bytes/A value longer than its varchar
-- Bytes that are not UTF-8, a zero byte and a value longer than its
-- varchar fail only where a row asked holds them, with the SQLSTATEs of
-- their Datums; the last fails in the ask of every row as Datums, after
-- the first ask took a reference to the window's array, which the error
-- frees. A grouping by each column, through the bytes form and with
-- Tessera off through Datums, fails with the same SQLSTATE.
SELECT parquet_test_write(:'dir' || '/bytes_bad.parquet', '{"dictionary": false, "columns": [
      {"name": "s", "type": "bytes", "logical": "string",
       "values": ["a", "b", "c", "d", "e", {"hex": "ff"}, "g", "h"]},
      {"name": "v", "type": "bytes", "logical": "string",
       "values": ["a", "b", "c", "d", "e", "f", "g", "long"]},
      {"name": "z", "type": "bytes", "logical": "string",
       "values": ["a", "b", "c", "d", "e", "f", {"hex": "610062"}, "h"]}]}');
\set bytes_bad :dir '/bytes_bad.parquet'
CREATE FOREIGN TABLE t_bytes_bad (s text, v varchar(3), z text) SERVER pq
    OPTIONS (files :'bytes_bad');
SELECT count(*) AS asked FROM parquet_test_bytes('SELECT s FROM t_bytes_bad', 8, 0, 31, false, false);
SELECT sqlstate_of($q$SELECT count(*) FROM parquet_test_bytes('SELECT s FROM t_bytes_bad', 8, 0, 32,
                                                              false, false)$q$) AS not_utf8;
SELECT count(*) AS asked FROM parquet_test_bytes('SELECT z FROM t_bytes_bad', 8, 0, 63, false, false);
SELECT sqlstate_of($q$SELECT count(*) FROM parquet_test_bytes('SELECT z FROM t_bytes_bad', 8, 0, 64,
                                                              false, false)$q$) AS zero_byte;
SELECT count(*) AS asked FROM parquet_test_bytes('SELECT v FROM t_bytes_bad', 8, 0, 127, false, false);
SELECT sqlstate_of($q$SELECT count(*) FROM parquet_test_bytes('SELECT v FROM t_bytes_bad', 8, 0, 128,
                                                              false, false)$q$) AS too_long;
SELECT sqlstate_of($q$SELECT count(*) FROM parquet_test_bytes('SELECT s FROM t_bytes_bad', 8, 0, 31,
                                                              true, false)$q$) AS as_datums;
SELECT explain_lines('ANALYZE false', 'SELECT s, count(*) FROM t_bytes_bad GROUP BY 1',
                     'TessAgg|Scan');
SELECT c, sqlstate_of(format('SELECT %s, count(*) FROM t_bytes_bad GROUP BY 1', c)) AS tessera_on
FROM unnest(ARRAY['s', 'z', 'v']) c;
SET tessera.enable = off;
SELECT c, sqlstate_of(format('SELECT %s, count(*) FROM t_bytes_bad GROUP BY 1', c)) AS tessera_off
FROM unnest(ARRAY['s', 'z', 'v']) c;
RESET tessera.enable;
-- spec: string-bytes/Groups by bytes
-- TessAgg reads a grouping's string keys in the bytes form: by text, by
-- varchar and the int column before it, whose values the grouping makes,
-- and by bytea, over the file of plain pages and the one through a
-- dictionary: its groups are the core's, and the scan makes no value of
-- a key, Values Decoded counting the int column's alone. The ungrouped
-- COUNT(DISTINCT) of text, of bytea, and of varchar beside a sum of the
-- int column before it, reads its argument the same way.
SELECT tbl, differ_off(format('SELECT s, count(*) FROM %s GROUP BY s', tbl)) AS by_text,
       differ_off(format('SELECT v, i, count(*) FROM %s GROUP BY v, i', tbl)) AS by_varchar_int,
       differ_off(format('SELECT b, count(*) FROM %s GROUP BY b', tbl)) AS by_bytea,
       differ_off(format('SELECT count(DISTINCT s) FROM %s', tbl)) AS distinct_text,
       differ_off(format('SELECT count(DISTINCT b) FROM %s', tbl)) AS distinct_bytea,
       differ_off(format('SELECT count(DISTINCT v), sum(i) FROM %s', tbl)) AS distinct_after_int
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl);
SELECT explain_lines('ANALYZE', 'SELECT s, count(*) FROM t_bytes_dict GROUP BY s',
                     'TessAgg|Decoded');
SELECT explain_lines('ANALYZE', 'SELECT v, i, count(*) FROM t_bytes_plain GROUP BY v, i',
                     'TessAgg|Decoded');
SELECT explain_lines('ANALYZE', 'SELECT count(DISTINCT s) FROM t_bytes_dict',
                     'TessAgg|Decoded');
SELECT explain_lines('ANALYZE', format('SELECT count(DISTINCT b) FROM %s', tbl),
                     'TessAgg|Decoded')
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl);
SELECT explain_lines('ANALYZE', 'SELECT count(DISTINCT v), sum(i) FROM t_bytes_plain',
                     'TessAgg|Decoded');
-- spec: string-bytes/Functions of a Parquet column
-- The string kernels read a text column of ASCII and of two-byte
-- characters, with NULL rows and empty strings, in the bytes form, over
-- plain pages and through a dictionary: each function of a value, each
-- of a piece and each predicate, in a grouping's aggregate, in a filter
-- and in an output column the scan computes for a sort, a predicate's
-- through a CASE, give the rows of the core. The source checks a LIKE,
-- = or <> of a constant itself, so a filter puts each predicate in an OR,
-- which the kernels compute. The patterns with _, with an escape and of
-- more pieces than the kernels' matcher holds go to the core's function
-- a row, with values made from the bytes. The scans make no value of s,
-- for a FILTER alone too: the values decoded are those of i.
SELECT tbl, kind, e AS expression,
       differ_off(format(CASE kind
           WHEN 'value' THEN 'SELECT i %% 4 AS g, sum(%s) FROM %s GROUP BY 1'
           WHEN 'piece' THEN 'SELECT i %% 4 AS g, sum(octet_length(%s)) FROM %s GROUP BY 1'
           ELSE 'SELECT i %% 4 AS g, count(*) FILTER (WHERE %s) FROM %s GROUP BY 1' END,
         e, tbl)) AS in_aggregate,
       differ_off(format(CASE kind
           WHEN 'value' THEN 'SELECT i FROM %2$s WHERE %1$s > 1'
           WHEN 'piece' THEN 'SELECT i FROM %2$s WHERE %1$s <> ''s1'''
           ELSE 'SELECT i FROM %2$s WHERE (%1$s) OR i < 0' END, e, tbl)) AS in_filter,
       differ_off(format(CASE kind
           WHEN 'predicate' THEN 'SELECT * FROM (SELECT i, CASE WHEN %s THEN 1 ELSE 0 END AS x
                                  FROM %s WHERE i > 10 ORDER BY i) q'
           ELSE 'SELECT * FROM (SELECT i, %s AS x FROM %s WHERE i > 10 ORDER BY i) q' END,
         e, tbl)) AS in_output
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl),
     (VALUES ('value', 'length(s)'), ('value', 'char_length(s)'), ('value', 'octet_length(s)'),
             ('piece', 'substr(s, 2)'), ('piece', 'substr(s, 1, 2)'), ('piece', 'left(s, 1)'),
             ('piece', 'right(s, 1)'), ('piece', 'ltrim(s)'), ('piece', 'rtrim(s)'),
             ('piece', 'btrim(s)'),
             ('predicate', 's LIKE ''s1%'''), ('predicate', 's NOT LIKE ''%é%'''),
             ('predicate', 'starts_with(s, ''s1'')'), ('predicate', 's = ''s3'''),
             ('predicate', 's <> ''s3'''), ('predicate', 's IN (''s1'', ''é2'', '''')'),
             ('predicate', 's LIKE ''s_'''), ('predicate', 's LIKE ''\s1%'''),
             ('predicate', 's NOT LIKE repeat(''%s'', 33)')) x(kind, e)
ORDER BY tbl, kind, e;
SELECT explain_lines('ANALYZE', format('SELECT i %% 4, sum(length(s)),
                                               count(*) FILTER (WHERE s LIKE ''s_'')
                                        FROM %s GROUP BY 1', tbl), 'TessAgg|Decoded')
FROM (VALUES ('t_bytes_plain'), ('t_bytes_dict')) t(tbl);
SELECT explain_lines('ANALYZE', 'SELECT i % 4, count(*) FILTER (WHERE s LIKE ''s1%'')
                                 FROM t_bytes_dict GROUP BY 1', 'TessAgg|Decoded');
SELECT explain_lines('ANALYZE', 'SELECT i FROM t_bytes_dict WHERE (s NOT LIKE ''%é%'') OR i < 0',
                     'Custom|Filter|Decoded');
SELECT explain_lines('ANALYZE', 'SELECT i, left(s, 1) FROM t_bytes_plain WHERE i > 10 ORDER BY i',
                     'Custom|Decoded');
-- spec: string-bytes/Ordered by bytes
-- TessSort orders a text key under C and under the builtin C.UTF-8, and
-- a bytea key, by its bytes: its word is its first eight bytes, and the
-- rows of equal words go to the comparison. Over a file of strings that
-- begin others or share their first eight bytes, of two- and three-byte
-- characters, empty strings and NULL rows, and of the bytes 00, 7f, 80
-- and ff, in both directions, NULL rows first and last, with and without
-- LIMIT 10, after a key of its own, in memory and spilled: the rows come
-- in the core's order.
SELECT parquet_test_write(:'dir' || '/bytes_sort.parquet', json_build_object(
    'dictionary', false, 'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32', 'values',
        (SELECT json_agg(n ORDER BY n) FROM generate_series(0, 1999) n)),
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE n % 11 WHEN 0 THEN NULL WHEN 1 THEN '' WHEN 2 THEN 'abcdefgh'
                                     WHEN 3 THEN 'abcdefgh' || n % 17 WHEN 4 THEN 'abcdefg'
                                     WHEN 5 THEN 'abcdefghé' || n % 7 WHEN 6 THEN 'é' || n % 5
                                     WHEN 7 THEN '€' || n % 3 WHEN 8 THEN 'abcdefgh€'
                                     WHEN 9 THEN 'Abcdefgh' || n % 13 ELSE 'z' || n % 19 END
                         ORDER BY n)
         FROM generate_series(0, 1999) n)),
      json_build_object('name', 'b', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE n % 7 WHEN 0 THEN NULL
                                    ELSE json_build_object('hex', (ARRAY['00', '7f', '80',
                                      'ff' || lpad(to_hex(n % 5), 2, '0'), '0000', 'ff'])[n % 7])
                         END ORDER BY n)
         FROM generate_series(0, 1999) n))))::text);
\set bytes_sort :dir '/bytes_sort.parquet'
CREATE FOREIGN TABLE t_bytes_sort (i int, s text, b bytea) SERVER pq OPTIONS (files :'bytes_sort');
-- The rows of a query in order, with Tessera and with the core: 'same, n
-- rows' or the difference.
CREATE FUNCTION order_off(statement text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    saved text := current_setting('tessera.enable');
    with_tessera text[];
    by_core text[];
    wrapped text := format('SELECT array_agg(q::text) FROM (%s) AS q', statement);
BEGIN
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO by_core;
    PERFORM set_config('tessera.enable', saved, true);
    IF with_tessera IS DISTINCT FROM by_core THEN
        RETURN format('MISMATCH %s against %s', with_tessera, by_core);
    END IF;
    RETURN format('same, %s rows', coalesce(cardinality(with_tessera), 0));
END $$;
SELECT o AS ordering,
       order_off(format('SELECT i, s, b FROM t_bytes_sort ORDER BY %s, i', o)) AS in_full,
       order_off(format('SELECT i, s, b FROM t_bytes_sort ORDER BY %s, i LIMIT 10', o)) AS top_10
FROM (VALUES ('s COLLATE "C"'), ('s COLLATE "C" DESC'), ('s COLLATE "C" NULLS FIRST'),
             ('s COLLATE "C" DESC NULLS LAST'), ('s COLLATE pg_c_utf8'),
             ('s COLLATE pg_c_utf8 DESC'), ('b'), ('b DESC NULLS LAST'),
             ('i % 3, s COLLATE "C"'), ('i % 3 DESC, s COLLATE "C" DESC')) x(o);
SET work_mem = '64kB';
SELECT o AS ordering,
       order_off(format('SELECT i, s, b FROM t_bytes_sort ORDER BY %s, i', o)) AS spilled
FROM (VALUES ('s COLLATE "C"'), ('s COLLATE pg_c_utf8 DESC NULLS LAST'), ('b')) x(o);
SELECT explain_lines('ANALYZE, VERBOSE',
                     'SELECT i, s FROM t_bytes_sort ORDER BY s COLLATE "C", i',
                     'TessSort|Sort Method|Abbreviated');
RESET work_mem;
SELECT explain_lines('ANALYZE, VERBOSE', format('SELECT i, s, b FROM t_bytes_sort ORDER BY %s', o),
                     'Abbreviated')
FROM (VALUES ('s COLLATE "C"'), ('s COLLATE pg_c_utf8'), ('b')) x(o);
-- spec: string-bytes/Another collation
-- Under a collation of libc that is not C, and under C by an operator
-- class that orders the other way, the key is sorted as before: by its
-- comparison, without the words of its bytes; and so is a varchar key
-- under C whose type's default class is one that orders the other way.
DO $$ BEGIN
    CREATE COLLATION parquet_sort_libc (provider = libc, locale = 'en_US.UTF-8');
EXCEPTION WHEN OTHERS THEN
    CREATE COLLATION parquet_sort_libc (provider = libc, locale = 'C.UTF-8');
END $$;
CREATE FUNCTION text_reverse_cmp(text, text) RETURNS int LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT bttextcmp($2, $1)';
CREATE FUNCTION text_reverse_lt(text, text) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT $1 > $2';
CREATE FUNCTION text_reverse_le(text, text) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT $1 >= $2';
CREATE FUNCTION text_reverse_ge(text, text) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT $1 <= $2';
CREATE FUNCTION text_reverse_gt(text, text) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT $1 < $2';
CREATE OPERATOR <<< (LEFTARG = text, RIGHTARG = text, FUNCTION = text_reverse_lt);
CREATE OPERATOR <<<= (LEFTARG = text, RIGHTARG = text, FUNCTION = text_reverse_le);
CREATE OPERATOR >>>= (LEFTARG = text, RIGHTARG = text, FUNCTION = text_reverse_ge);
CREATE OPERATOR >>> (LEFTARG = text, RIGHTARG = text, FUNCTION = text_reverse_gt);
CREATE OPERATOR CLASS text_reverse_ops FOR TYPE text USING btree AS
    OPERATOR 1 <<<, OPERATOR 2 <<<=, OPERATOR 3 =, OPERATOR 4 >>>=, OPERATOR 5 >>>,
    FUNCTION 1 text_reverse_cmp(text, text);
-- The same reverse order made the default class of varchar, which has
-- none of its own: a varchar key under C by it, as before.
CREATE FUNCTION varchar_reverse_cmp(varchar, varchar) RETURNS int LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT bttextcmp($2::text, $1::text)';
CREATE FUNCTION varchar_reverse_op(varchar, varchar, int) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT CASE $3 WHEN 1 THEN $1::text > $2::text WHEN 2 THEN $1::text >= $2::text
                       WHEN 3 THEN $1::text = $2::text WHEN 4 THEN $1::text <= $2::text
                       ELSE $1::text < $2::text END';
CREATE FUNCTION varchar_reverse_lt(varchar, varchar) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT varchar_reverse_op($1, $2, 1)';
CREATE FUNCTION varchar_reverse_le(varchar, varchar) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT varchar_reverse_op($1, $2, 2)';
CREATE FUNCTION varchar_reverse_eq(varchar, varchar) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT varchar_reverse_op($1, $2, 3)';
CREATE FUNCTION varchar_reverse_ge(varchar, varchar) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT varchar_reverse_op($1, $2, 4)';
CREATE FUNCTION varchar_reverse_gt(varchar, varchar) RETURNS bool LANGUAGE sql IMMUTABLE STRICT
    AS 'SELECT varchar_reverse_op($1, $2, 5)';
CREATE OPERATOR <<< (LEFTARG = varchar, RIGHTARG = varchar, FUNCTION = varchar_reverse_lt);
CREATE OPERATOR <<<= (LEFTARG = varchar, RIGHTARG = varchar, FUNCTION = varchar_reverse_le);
CREATE OPERATOR === (LEFTARG = varchar, RIGHTARG = varchar, FUNCTION = varchar_reverse_eq);
CREATE OPERATOR >>>= (LEFTARG = varchar, RIGHTARG = varchar, FUNCTION = varchar_reverse_ge);
CREATE OPERATOR >>> (LEFTARG = varchar, RIGHTARG = varchar, FUNCTION = varchar_reverse_gt);
CREATE OPERATOR CLASS varchar_reverse_ops DEFAULT FOR TYPE varchar USING btree AS
    OPERATOR 1 <<< (varchar, varchar), OPERATOR 2 <<<= (varchar, varchar),
    OPERATOR 3 === (varchar, varchar), OPERATOR 4 >>>= (varchar, varchar),
    OPERATOR 5 >>> (varchar, varchar), FUNCTION 1 varchar_reverse_cmp(varchar, varchar);
SELECT o AS ordering,
       order_off(format('SELECT i, s FROM t_bytes_sort ORDER BY %s, i', o)) AS in_full,
       order_off(format('SELECT i, s FROM t_bytes_sort ORDER BY %s, i LIMIT 10', o)) AS top_10
FROM (VALUES ('s COLLATE parquet_sort_libc'), ('s COLLATE "C" USING <<<'),
             ('s::varchar COLLATE "C"')) x(o);
SELECT explain_lines('ANALYZE, VERBOSE', format('SELECT i, s FROM t_bytes_sort ORDER BY %s', o),
                     'Abbreviated')
FROM (VALUES ('s COLLATE parquet_sort_libc'), ('s COLLATE "C" USING <<<'),
             ('s::varchar COLLATE "C"')) x(o);
DROP OPERATOR CLASS varchar_reverse_ops USING btree;
DROP OPERATOR FAMILY varchar_reverse_ops USING btree;
DROP OPERATOR <<< (varchar, varchar), <<<= (varchar, varchar), === (varchar, varchar),
    >>>= (varchar, varchar), >>> (varchar, varchar);
DROP FUNCTION varchar_reverse_cmp(varchar, varchar), varchar_reverse_lt(varchar, varchar),
    varchar_reverse_le(varchar, varchar), varchar_reverse_eq(varchar, varchar),
    varchar_reverse_ge(varchar, varchar), varchar_reverse_gt(varchar, varchar),
    varchar_reverse_op(varchar, varchar, int);
DROP OPERATOR CLASS text_reverse_ops USING btree;
DROP OPERATOR FAMILY text_reverse_ops USING btree;
DROP OPERATOR <<< (text, text), <<<= (text, text), >>>= (text, text), >>> (text, text);
DROP FUNCTION text_reverse_cmp(text, text), text_reverse_lt(text, text),
    text_reverse_le(text, text), text_reverse_ge(text, text), text_reverse_gt(text, text);
DROP COLLATION parquet_sort_libc;
DROP FOREIGN TABLE t_bytes_sort;
-- spec: string-bytes/Top-N makes the kept values
-- ORDER BY a text column under C with LIMIT 10 over 100 000 rows in no
-- order of the key: the top-N sort makes its words from the bytes form,
-- and asks the scan for the values of the rows that may beat the worst
-- kept alone, so that the scan makes values for fewer rows than it reads;
-- the rows are the core's, in both directions and with a column beside.
SELECT parquet_test_write(:'dir' || '/bytes_top.parquet', json_build_object(
    'dictionary', false, 'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32', 'values',
        (SELECT json_agg(n ORDER BY n) FROM generate_series(0, 99999) n)),
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg('k' || lpad((n * 7919 % 100000)::text, 6, '0') ORDER BY n)
         FROM generate_series(0, 99999) n))))::text);
\set bytes_top :dir '/bytes_top.parquet'
CREATE FOREIGN TABLE t_bytes_top (i int, s text) SERVER pq OPTIONS (files :'bytes_top');
SELECT explain_lines('ANALYZE', 'SELECT s FROM t_bytes_top ORDER BY s COLLATE "C" LIMIT 10',
                     'TessSort|Sort Method|TessParquetScan');
SELECT (regexp_match(line, 'Values Decoded: (\d+)'))[1]::int < 100000 AS fewer_values_made
FROM explain_lines('ANALYZE', 'SELECT s FROM t_bytes_top ORDER BY s COLLATE "C" LIMIT 10',
                   'Values Decoded') AS line;
SELECT order_off('SELECT s FROM t_bytes_top ORDER BY s COLLATE "C" LIMIT 10') AS ascending,
       order_off('SELECT s FROM t_bytes_top ORDER BY s COLLATE "C" DESC LIMIT 10') AS descending,
       order_off('SELECT i, s FROM t_bytes_top ORDER BY s COLLATE "C", i LIMIT 10') AS beside;
-- Strings whose first nine bytes are all the same, whose words are thus
-- all equal: the rows are decided by their full bytes, still before any
-- value is made.
SELECT parquet_test_write(:'dir' || '/bytes_prefix.parquet', json_build_object(
    'dictionary', false, 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg('commonpre' || lpad((n * 7919 % 20000)::text, 5, '0') ORDER BY n)
         FROM generate_series(0, 19999) n))))::text);
\set bytes_prefix :dir '/bytes_prefix.parquet'
CREATE FOREIGN TABLE t_bytes_prefix (s text) SERVER pq OPTIONS (files :'bytes_prefix');
SELECT (regexp_match(line, 'Values Decoded: (\d+)'))[1]::int < 20000 AS fewer_values_made
FROM explain_lines('ANALYZE', 'SELECT s FROM t_bytes_prefix ORDER BY s COLLATE "C" LIMIT 10',
                   'Values Decoded') AS line;
SELECT order_off('SELECT s FROM t_bytes_prefix ORDER BY s COLLATE "C" LIMIT 10') AS ascending,
       order_off('SELECT s FROM t_bytes_prefix ORDER BY s COLLATE "C" DESC LIMIT 10') AS descending;
DROP FOREIGN TABLE t_bytes_top, t_bytes_prefix;
-- spec: string-bytes/A nondeterministic collation
-- Under a collation that ignores case, whose equal values may differ in
-- their bytes, the key is read as Datum values, the scan making each
-- row's.
CREATE COLLATION parquet_bytes_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT differ_off('SELECT lower(s COLLATE parquet_bytes_ci), count(*)
                   FROM t_bytes_dict GROUP BY s COLLATE parquet_bytes_ci') AS by_nondeterministic;
SELECT explain_lines('ANALYZE', 'SELECT count(*) FROM t_bytes_dict GROUP BY s COLLATE parquet_bytes_ci',
                     'TessAgg|Decoded');
DROP COLLATION parquet_bytes_ci;
-- spec: string-bytes/Groups that spill
-- A serial grouping by a text column through a dictionary, of more groups
-- than work_mem holds: its groups are the core's, its rows written to
-- disk and read back; the values of the rows written are made by TessAgg
-- from their bytes, so that the scan makes none. Grouped by two text
-- columns, the values both keys make for a batch's rows are written.
SELECT parquet_test_write(:'dir' || '/bytes_many.parquet', json_build_object(
    'dictionary', true, 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE WHEN n % 2 = 0 THEN 'hot' || n % 10 ELSE 'é' || (n * 7919 % 5000) END
                         ORDER BY n) FROM generate_series(1, 20000) n)),
      json_build_object('name', 't', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE WHEN n % 11 = 0 THEN NULL WHEN n % 2 = 0 THEN 'k' || n % 3
                              ELSE 'ü' || n % 4 END
                         ORDER BY n) FROM generate_series(1, 20000) n))))::text);
\set bytes_many :dir '/bytes_many.parquet'
CREATE FOREIGN TABLE t_bytes_many (s text, t text) SERVER pq OPTIONS (files :'bytes_many');
SET work_mem = '64kB';
SELECT differ_off('SELECT s, count(*) FROM t_bytes_many GROUP BY s') AS spilled_groups_differ;
SELECT differ_off('SELECT s, t, count(*) FROM t_bytes_many GROUP BY s, t')
    AS spilled_pairs_differ;
SELECT count(*) FILTER (WHERE line ~ 'Disk Usage') > 0 AS spilled,
       max(substring(line from 'Values Decoded: (\d+)')::int) = 0 AS no_value_of_the_scan
FROM explain_lines('ANALYZE', 'SELECT s, count(*) FROM t_bytes_many GROUP BY s',
                   'Disk Usage|Decoded') line;
SELECT count(*) FILTER (WHERE line ~ 'Disk Usage') > 0 AS spilled,
       max(substring(line from 'Values Decoded: (\d+)')::int) = 0 AS no_value_of_the_scan
FROM explain_lines('ANALYZE', 'SELECT s, t, count(*) FROM t_bytes_many GROUP BY s, t',
                   'Disk Usage|Decoded') line;
RESET work_mem;
DROP FOREIGN TABLE t_bytes_many;
DROP FOREIGN TABLE t_bytes_plain, t_bytes_dict, t_bytes_bad;
DROP TABLE h_bytes;
DROP FUNCTION parquet_test_bytes(text, int, int, bigint, bool, bool, bool);

-- spec: parquet-fdw/What EXPLAIN shows
-- Plain, with VERBOSE and with ANALYZE, through the wrapper's rows and
-- through the source; the parallel plan's counts under "One file in
-- parallel".
SET tessera.enable = off;
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_files', 'Scan|Parquet|Row Groups');
SELECT explain_lines('VERBOSE', 'SELECT count(*) FROM t_files', 'Scan|Parquet|Row Groups');
SELECT explain_lines('ANALYZE', 'SELECT a FROM t_files', 'Scan|Parquet|Row Groups');
RESET tessera.enable;
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_files', 'Scan|Parquet|Row Groups');
SELECT explain_lines('VERBOSE', 'SELECT count(*) FROM t_files', 'Scan|Parquet|Row Groups');
SELECT explain_lines('ANALYZE', 'SELECT a FROM t_files',
                     'Scan|Parquet|Row Groups|Values Decoded');
-- The clauses the source checks, as its node shows them.
SELECT explain_lines('ANALYZE false', 'SELECT a FROM t_files WHERE a > 1 AND a IS NOT NULL',
                     'Scan|Source Exact');

-- spec: parquet-fdw/Rows from the files
-- Three files of four groups of 1000 rows each, every group of a value
-- of g of its own, v of a value a row, planned before ANALYZE and after
-- it: the scan's rows are the files' before; after it, pg_stats holds
-- both columns, the sample taken across the groups, every group's value
-- among g's common values, and a clause on g plans the rows of its group;
-- a file added after ANALYZE counts in the table's rows and in the
-- distinct values of v.
SELECT parquet_test_write(:'dir' || format('/groups/g%s.parquet', f), json_build_object(
    'group_rows', 1000, 'columns', json_build_array(
      json_build_object('name', 'g', 'type', 'int32', 'values',
        (SELECT json_agg(f * 10 + n / 1000 ORDER BY n) FROM generate_series(0, 3999) n)),
      json_build_object('name', 'v', 'type', 'int64', 'values',
        (SELECT json_agg((f - 1) * 4000 + n ORDER BY n) FROM generate_series(0, 3999) n))))::text)
FROM generate_series(1, 3) f;
\set groups :dir '/groups/g*.parquet'
CREATE FOREIGN TABLE t_groups (g int, v bigint) SERVER pq OPTIONS (files :'groups');
ALTER FOREIGN TABLE t_groups ALTER COLUMN g SET STATISTICS 20, ALTER COLUMN v SET STATISTICS 20;
-- The rows and the total cost the planner gives a statement.
CREATE FUNCTION plan_of(statement text, out rows numeric, out cost numeric)
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
    rows := (plan -> 0 -> 'Plan' ->> 'Plan Rows')::numeric;
    cost := (plan -> 0 -> 'Plan' ->> 'Total Cost')::numeric;
END $$;
SELECT (plan_of('SELECT * FROM t_groups')).rows AS files_rows,
       (plan_of('SELECT * FROM t_groups WHERE g = 21')).rows AS one_group_before;
ANALYZE t_groups;
SELECT attname, most_common_vals IS NOT NULL AS common_values,
       histogram_bounds IS NOT NULL AS histogram
FROM pg_stats WHERE tablename = 't_groups' ORDER BY attname;
SELECT (SELECT most_common_vals::text::int[] FROM pg_stats
        WHERE tablename = 't_groups' AND attname = 'g') @>
       (SELECT array_agg(DISTINCT g) FROM t_groups) AS every_group_common;
-- The sample is at random: the rows a clause plans are near those of its
-- groups, within six standard deviations of the sample.
SELECT (plan_of('SELECT * FROM t_groups WHERE g = 21')).rows BETWEEN 800 AND 1200
           AS one_group_after,
       (plan_of('SELECT * FROM t_groups WHERE g < 20')).rows BETWEEN 3600 AND 4400 AS four_groups,
       (plan_of('SELECT v, count(*) FROM t_groups GROUP BY v')).rows AS distinct_v;
SELECT parquet_test_write(:'dir' || '/groups/g4.parquet', json_build_object(
    'group_rows', 1000, 'columns', json_build_array(
      json_build_object('name', 'g', 'type', 'int32', 'values',
        (SELECT json_agg(40 + n / 1000 ORDER BY n) FROM generate_series(0, 3999) n)),
      json_build_object('name', 'v', 'type', 'int64', 'values',
        (SELECT json_agg(12000 + n ORDER BY n) FROM generate_series(0, 3999) n))))::text);
-- A clause on v plans by the distinct values of the files' rows now,
-- 16 000, not those of the last ANALYZE: three values, three rows.
SELECT (plan_of('SELECT * FROM t_groups')).rows AS files_rows,
       (plan_of('SELECT v, count(*) FROM t_groups GROUP BY v')).rows AS distinct_v,
       (plan_of('SELECT * FROM t_groups WHERE v IN (1, 2, 3)')).rows AS three_values;
SELECT parquet_test_remove(:'dir' || '/groups/g4.parquet');

-- ANALYZE of a table of one group of 2500 rows, three windows, the last
-- short, with text holding NULL values and a column dropped, fewer rows
-- than the sample: every row sampled, and the statistics of the columns
-- equal those of a heap table of the same rows.
SELECT parquet_test_write(:'dir' || '/analyzed.parquet', json_build_object(
    'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int32', 'range', 2500),
      json_build_object('name', 'gone', 'type', 'int32', 'range', 2500),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg(CASE WHEN n % 11 = 0 THEN NULL ELSE 'v' || (n % 37) END ORDER BY n)
         FROM generate_series(0, 2499) n)),
      json_build_object('name', 'b', 'type', 'int64', 'values',
        (SELECT json_agg(n * 3 ORDER BY n) FROM generate_series(0, 2499) n))))::text);
\set analyzed :dir '/analyzed.parquet'
CREATE FOREIGN TABLE t_analyzed (i int, gone int, s text, b bigint)
    SERVER pq OPTIONS (files :'analyzed');
ALTER FOREIGN TABLE t_analyzed DROP COLUMN gone;
CREATE TABLE h_analyzed AS
SELECT n AS i, CASE WHEN n % 11 = 0 THEN NULL ELSE 'v' || (n % 37) END AS s, n * 3::bigint AS b
FROM generate_series(0, 2499) n;
ANALYZE t_analyzed;
ANALYZE h_analyzed;
SELECT reltuples FROM pg_class WHERE relname = 't_analyzed';
SELECT count(*) AS stats_differ FROM (
    (SELECT attname, null_frac, n_distinct, most_common_vals::text, most_common_freqs::text,
            histogram_bounds::text FROM pg_stats WHERE tablename = 't_analyzed'
     EXCEPT
     SELECT attname, null_frac, n_distinct, most_common_vals::text, most_common_freqs::text,
            histogram_bounds::text FROM pg_stats WHERE tablename = 'h_analyzed')
    UNION ALL
    (SELECT attname, null_frac, n_distinct, most_common_vals::text, most_common_freqs::text,
            histogram_bounds::text FROM pg_stats WHERE tablename = 'h_analyzed'
     EXCEPT
     SELECT attname, null_frac, n_distinct, most_common_vals::text, most_common_freqs::text,
            histogram_bounds::text FROM pg_stats WHERE tablename = 't_analyzed')) d;
SELECT attname FROM pg_stats WHERE tablename = 't_analyzed' ORDER BY attname;
-- A group of 200 000 rows sampled for 300, many of its windows holding
-- no row of the sample; a table of 301 rows sampled for 300, which leaves
-- out one row; and 600 groups of 100 rows whose values alternate
-- between 0 and 1 a group: both values in the sample, which is taken
-- over every row, not a share a group.
SELECT parquet_test_write(:'dir' || '/sparse.parquet',
    '{"columns": [{"name": "i", "type": "int64", "range": 200000}]}');
SELECT parquet_test_write(:'dir' || '/one_more.parquet',
    '{"columns": [{"name": "i", "type": "int64", "range": 301}]}');
SELECT parquet_test_write(:'dir' || '/alternating.parquet', json_build_object(
    'group_rows', 100, 'columns', json_build_array(
      json_build_object('name', 'a', 'type', 'int32', 'values',
        (SELECT json_agg((n / 100) % 2 ORDER BY n) FROM generate_series(0, 59999) n))))::text);
\set sparse :dir '/sparse.parquet'
\set alternating :dir '/alternating.parquet'
\set one_more :dir '/one_more.parquet'
CREATE FOREIGN TABLE t_sparse (i bigint) SERVER pq OPTIONS (files :'sparse');
CREATE FOREIGN TABLE t_one_more (i bigint) SERVER pq OPTIONS (files :'one_more');
ALTER FOREIGN TABLE t_one_more ALTER COLUMN i SET STATISTICS 1;
ANALYZE t_one_more;
CREATE FOREIGN TABLE t_alternating (a int) SERVER pq OPTIONS (files :'alternating');
ALTER FOREIGN TABLE t_sparse ALTER COLUMN i SET STATISTICS 1;
ALTER FOREIGN TABLE t_alternating ALTER COLUMN a SET STATISTICS 1;
ANALYZE t_sparse;
ANALYZE t_alternating;
SELECT relname, reltuples FROM pg_class
WHERE relname IN ('t_sparse', 't_alternating', 't_one_more') ORDER BY 1;
SELECT tablename, attname, null_frac, n_distinct FROM pg_stats
WHERE tablename IN ('t_sparse', 't_one_more') ORDER BY 1;
-- The sample of 300 of the 301 rows leaves one out: its least and
-- greatest values, the histogram's two bounds, span the rows.
SELECT (histogram_bounds::text::int[])[1] <= 1 AND (histogram_bounds::text::int[])[2] >= 299
       AS spans_the_rows
FROM pg_stats WHERE tablename = 't_one_more';
SELECT n_distinct AS both_values FROM pg_stats WHERE tablename = 't_alternating';

-- spec: parquet-fdw/The source chosen
-- A query the scan planner offers the sources, with Tessera on: its plan
-- reads the table through the source's node, which costs less than the
-- wrapper's rows over the same columns, and checks the query's clause
-- itself.
SELECT scans_of('SELECT g, v FROM t_groups') AS read,
       scans_of('SELECT g, sum(v) FROM t_groups GROUP BY g') AS grouped,
       scans_of('SELECT count(*) FROM t_groups') AS counted,
       scans_of('SELECT g FROM t_groups WHERE v < 10') AS filtered;
SET tessera.enable = off;
SELECT (plan_of('SELECT g, v FROM t_groups')).cost AS wrapper_cost \gset
RESET tessera.enable;
SELECT (plan_of('SELECT g, v FROM t_groups')).cost < :wrapper_cost AS source_cheaper;
SET tessera.enable = off;
SELECT (plan_of('SELECT g FROM t_groups WHERE v < 10')).cost AS wrapper_cost \gset
RESET tessera.enable;
SELECT (plan_of('SELECT g FROM t_groups WHERE v < 10')).cost < :wrapper_cost
    AS source_cheaper_filtered;

-- spec: parquet-fdw/Clauses checked by the source
-- The clauses on integers, dates and times, IN and = ANY, the tests of
-- NULL and the clauses on strings, which the source checks on the stored
-- values: each shown under its node and none in TessFilter, by constants,
-- the smallint's against bigint constants outside its range, a prepared
-- statement's parameter and a subquery's value, each result that of a
-- heap table of the same rows.
SELECT parquet_test_write(:'dir' || '/clauses.parquet', json_build_object(
    'group_rows', 1000, 'columns', json_build_array(
      json_build_object('name', 'i2', 'type', 'int32', 'logical', 'int16', 'values',
        (SELECT json_agg(CASE WHEN i % 7 = 0 THEN NULL ELSE i % 300 - 150 END ORDER BY i)
         FROM generate_series(0, 2999) i)),
      json_build_object('name', 'i4', 'type', 'int32', 'values',
        (SELECT json_agg(CASE WHEN i % 11 = 0 THEN NULL ELSE i END ORDER BY i)
         FROM generate_series(0, 2999) i)),
      json_build_object('name', 'i8', 'type', 'int64', 'values',
        (SELECT json_agg(CASE WHEN i % 13 = 0 THEN NULL ELSE i * 1000000007::bigint END ORDER BY i)
         FROM generate_series(0, 2999) i)),
      json_build_object('name', 'd', 'type', 'int32', 'logical', 'date', 'values',
        (SELECT json_agg(CASE WHEN i % 17 = 0 THEN NULL ELSE 15800 + i % 200 END ORDER BY i)
         FROM generate_series(0, 2999) i)),
      json_build_object('name', 'ts', 'type', 'int64', 'values',
        (SELECT json_agg(CASE WHEN i % 19 = 0 THEN NULL ELSE 1373846400 + i * 37 END ORDER BY i)
         FROM generate_series(0, 2999) i)),
      json_build_object('name', 'tz', 'type', 'int64', 'logical', 'timestamp_us_utc', 'values',
        (SELECT json_agg(CASE WHEN i % 23 = 0 THEN NULL
                              ELSE (1373846400 + i * 41) * 1000000::bigint END ORDER BY i)
         FROM generate_series(0, 2999) i))))::text);
\set clauses :dir '/clauses.parquet'
CREATE FOREIGN TABLE t_clauses (i2 smallint, i4 integer, i8 bigint, d date,
    ts timestamp OPTIONS (unit 'seconds'), tz timestamptz)
    SERVER pq OPTIONS (files :'clauses');
CREATE TABLE h_clauses AS
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i % 300 - 150 END::smallint AS i2,
       CASE WHEN i % 11 = 0 THEN NULL ELSE i END AS i4,
       CASE WHEN i % 13 = 0 THEN NULL ELSE i * 1000000007::bigint END AS i8,
       CASE WHEN i % 17 = 0 THEN NULL ELSE DATE '1970-01-01' + (15800 + i % 200) END AS d,
       CASE WHEN i % 19 = 0 THEN NULL
            ELSE 'epoch'::timestamp + (1373846400 + i * 37) * interval '1 second' END AS ts,
       CASE WHEN i % 23 = 0 THEN NULL ELSE to_timestamp(1373846400 + i * 41) END AS tz
FROM generate_series(0, 2999) i;
SET TIME ZONE 'UTC';
SELECT c AS clause,
       checked_by(format('SELECT * FROM t_clauses WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_clauses WHERE %s', c),
              format('SELECT * FROM h_clauses WHERE %s', c)) AS rows_differ,
       rows_of(format('SELECT * FROM h_clauses WHERE %s', c)) AS heap_rows
FROM unnest(ARRAY[
    'i2 < 10', 'i2 >= -5', 'i2 = 7', 'i2 <> 7', '40000 > i2', 'i2 = 40000::bigint',
    'i2 > -40000::bigint', 'i4 <= 100', 'i4 = ANY (ARRAY[1, 2, 3, 2999])', 'i4 IN (5, NULL, 77)',
    'i8 > 1000000007000::bigint', 'i8 IN (0, 2000000014)', $c$d >= DATE '2013-05-01'$c$,
    $c$d = DATE '2013-04-10'$c$, $c$d <> DATE '2013-04-10'$c$,
    $c$ts > TIMESTAMP '2013-07-15 10:00:00'$c$, $c$ts = TIMESTAMP '2013-07-15 00:00:37'$c$,
    $c$ts < TIMESTAMP '2013-07-15 00:00:37.5'$c$, $c$tz <= TIMESTAMPTZ '2013-07-15 12:00:00+00'$c$,
    'i8 IS NULL', 'd IS NOT NULL', 'i2 < 10 AND ts IS NOT NULL AND i4 > 100',
    'i4 < (SELECT max(i4) / 2 FROM h_clauses)', $c$d < DATE 'infinity'$c$,
    $c$d = DATE 'infinity'$c$, $c$ts <> TIMESTAMP '-infinity'$c$,
    $c$d IN (DATE 'infinity', DATE '2013-04-10')$c$]) c;
SET plan_cache_mode = force_generic_plan;
PREPARE below(int, date) AS SELECT count(*) FROM t_clauses WHERE i4 < $1 AND d >= $2;
SELECT checked_by($$EXECUTE below(500, '2013-05-01')$$) AS checked_by;
EXECUTE below(500, '2013-05-01');
SELECT count(*) AS heap FROM h_clauses WHERE i4 < 500 AND d >= '2013-05-01';
-- A parameter that is NULL, and an array that is: no row.
EXECUTE below(NULL, '2013-05-01');
PREPARE listed(int[]) AS SELECT count(*) FROM t_clauses WHERE i4 = ANY ($1);
SELECT checked_by('EXECUTE listed(ARRAY[1, 2])') AS checked_by;
EXECUTE listed(ARRAY[1, 2, NULL]);
EXECUTE listed(NULL);
DEALLOCATE below;
DEALLOCATE listed;
RESET plan_cache_mode;
-- The clauses on strings, over a file whose strings go through
-- dictionaries and one whose strings are plain: a text column marked as
-- a string, a varchar one over unmarked bytes, the examples of the spec
-- among them, 'aba' and 'abba' for 'ab%ba' and 'a' for 'a%a'. Each group
-- of 500 rows starts its cycle of values at another one, so that its
-- dictionary holds them in another order than the group before it.
CREATE TABLE h_strings (s text, v varchar(10));
INSERT INTO h_strings
SELECT CASE WHEN i % 11 = 0 THEN NULL
            ELSE (ARRAY['', 'a', 'aba', 'abba', 'x', 'X', 'ab', 'ba', 'abc', 'été'])
                 [(i + i / 500 * 3) % 10 + 1]
       END,
       CASE WHEN i % 13 = 0 THEN NULL ELSE (ARRAY['ab', 'b', '', 'aab'])[i % 4 + 1] END
FROM generate_series(0, 1999) i;
SELECT parquet_test_write(:'dir' || '/strings_' || d || '.parquet', json_build_object(
    'group_rows', 500, 'dictionary', d = 'dictionary', 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(s ORDER BY i) FROM (SELECT row_number() OVER () AS i, s FROM h_strings) q)),
      json_build_object('name', 'v', 'type', 'bytes', 'values',
        (SELECT json_agg(v ORDER BY i) FROM (SELECT row_number() OVER () AS i, v FROM h_strings) q))))::text)
FROM unnest(ARRAY['dictionary', 'plain']) d;
\set strings :dir '/strings_dictionary.parquet,' :dir '/strings_plain.parquet'
CREATE FOREIGN TABLE t_strings (s text, v varchar(10)) SERVER pq OPTIONS (files :'strings');
INSERT INTO h_strings SELECT * FROM h_strings;
SELECT c AS clause,
       checked_by(format('SELECT * FROM t_strings WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_strings WHERE %s', c),
              format('SELECT * FROM h_strings WHERE %s', c)) AS rows_differ,
       rows_of(format('SELECT * FROM h_strings WHERE %s', c)) AS heap_rows
FROM unnest(ARRAY[$c$s <> ''$c$, $c$s = ''$c$, $c$s = 'x'$c$, $c$'x' = s$c$, $c$s <> 'x'$c$,
    $c$s = 'été'$c$, $c$s LIKE '%a%b%'$c$, $c$s NOT LIKE 'a%'$c$, $c$s LIKE 'ab%ba'$c$,
    $c$s LIKE 'a%a'$c$, $c$s LIKE '%'$c$, $c$s LIKE 'abc'$c$, $c$v = 'ab'$c$,
    $c$v LIKE 'a%b'$c$, $c$v <> ''$c$, $c$s = (SELECT 'aba')$c$,
    $c$s LIKE repeat('%', 31)$c$]) c;
SET plan_cache_mode = force_generic_plan;
PREPARE named(text) AS SELECT count(*) FROM t_strings WHERE s = $1;
SELECT checked_by($$EXECUTE named('ab')$$) AS checked_by;
EXECUTE named('ab');
EXECUTE named(NULL);
SELECT count(*) AS heap FROM h_strings WHERE s = 'ab';
DEALLOCATE named;
RESET plan_cache_mode;
-- A table of two files whose clause columns differ in type: bigint over
-- INT32 and over INT64, timestamp over milliseconds and microseconds,
-- each file's tests made in its own unit.
SELECT parquet_test_write(:'dir' || '/mixed_a.parquet',
    '{"columns": [{"name": "c", "type": "int32", "values": [1, 5, null, 9]},
                  {"name": "t", "type": "int64", "logical": "timestamp_ms",
                   "values": [1373846400000, 1373846400500, 1373846401000, null]}]}');
SELECT parquet_test_write(:'dir' || '/mixed_b.parquet',
    '{"columns": [{"name": "c", "type": "int64", "values": [5000000000, 5, null]},
                  {"name": "t", "type": "int64", "logical": "timestamp_us",
                   "values": [1373846400000000, 1373846400500000, 1373846400000001]}]}');
\set mixed :dir '/mixed_a.parquet,' :dir '/mixed_b.parquet'
CREATE FOREIGN TABLE t_mixed (c bigint, t timestamp) SERVER pq OPTIONS (files :'mixed');
CREATE TABLE h_mixed (c bigint, t timestamp);
INSERT INTO h_mixed VALUES
    (1, '2013-07-15 00:00:00'), (5, '2013-07-15 00:00:00.5'), (NULL, '2013-07-15 00:00:01'),
    (9, NULL), (5000000000, '2013-07-15 00:00:00'), (5, '2013-07-15 00:00:00.5'),
    (NULL, '2013-07-15 00:00:00.000001');
SELECT c AS clause,
       checked_by(format('SELECT * FROM t_mixed WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_mixed WHERE %s', c),
              format('SELECT * FROM h_mixed WHERE %s', c)) AS rows_differ,
       rows_of(format('SELECT * FROM h_mixed WHERE %s', c)) AS heap_rows
FROM unnest(ARRAY['c = 5', 'c > 4', 'c IN (1, 5000000000)', 'c IS NULL',
    $c$t = TIMESTAMP '2013-07-15 00:00:00.5'$c$, $c$t > TIMESTAMP '2013-07-15 00:00:00'$c$]) c;
-- The tests of NULL on a column of text.
SELECT c AS clause, checked_by(format('SELECT * FROM t_wide WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_wide WHERE %s', c),
              format('SELECT * FROM h_wide WHERE %s', c)) AS rows_differ,
       rows_of(format('SELECT * FROM h_wide WHERE %s', c)) AS heap_rows
FROM unnest(ARRAY['s IS NULL', 's IS NOT NULL AND k < 100']) c;
-- spec: parquet-fdw/Clauses left to the filter
-- Clauses the source leaves to TessFilter: an operator over a column, two
-- columns compared, a function of a column, a timestamp against a
-- timestamptz under a zone with daylight saving.
SET TIME ZONE 'America/New_York';
SELECT c AS clause,
       checked_by(format('SELECT * FROM t_clauses WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_clauses WHERE %s', c),
              format('SELECT * FROM h_clauses WHERE %s', c)) AS rows_differ
FROM unnest(ARRAY['i4 + 0 < 10', 'i4 < i8', 'abs(i2) < 3', 'ts < tz']) c;
-- On strings: a regular expression, LIKE with _ or an escape, or with 32
-- % signs, ILIKE, a function, two columns, and =, <> and LIKE under a
-- case-insensitive nondeterministic collation, over 'x' and 'X', which it
-- holds equal.
CREATE COLLATION parquet_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT c AS clause,
       checked_by(format('SELECT * FROM t_strings WHERE %s', c)) AS checked_by,
       differ(format('SELECT * FROM t_strings WHERE %s', c),
              format('SELECT * FROM h_strings WHERE %s', c)) AS rows_differ,
       rows_of(format('SELECT * FROM h_strings WHERE %s', c)) AS heap_rows
FROM unnest(ARRAY[$c$s ~ '^a.a$'$c$, $c$s LIKE 'a_a'$c$, $c$s LIKE 'a\%'$c$,
    $c$s LIKE repeat('%', 32)$c$,
    $c$s ILIKE 'A%'$c$, $c$upper(s) = 'X'$c$, $c$s = v$c$,
    $c$s COLLATE parquet_ci = 'X'$c$, $c$s COLLATE parquet_ci <> 'x'$c$,
    $c$s COLLATE parquet_ci LIKE 'X%'$c$]) c;
RESET TIME ZONE;

-- spec: parquet-fdw/A clause on a value that changes by rescan
-- A correlated subquery rescans the source's node for each outer row,
-- under a comparison with the outer row's value and an IN of its values.
CREATE TABLE h_outer AS SELECT i * 150 AS x, i * 150 + 1 AS y FROM generate_series(0, 19) i;
SELECT scans_of('SELECT o.x, (SELECT count(*) FROM t_clauses t WHERE t.i4 > o.x) FROM h_outer o')
    AS rescanned;
SELECT differ(
    'SELECT o.x, (SELECT count(*) FROM t_clauses t WHERE t.i4 > o.x),
            (SELECT count(*) FROM t_clauses t WHERE t.i4 IN (o.x, o.y)) FROM h_outer o',
    'SELECT o.x, (SELECT count(*) FROM h_clauses t WHERE t.i4 > o.x),
            (SELECT count(*) FROM h_clauses t WHERE t.i4 IN (o.x, o.y)) FROM h_outer o')
    AS rows_differ;

-- spec: parquet-fdw/Values out of range in a clause
-- Seconds past the range of timestamp: a clause that rejects them counts
-- the other rows, one that passes them counts them too, both without an
-- error, and only a value made of them raises 22008.
SELECT parquet_test_write(:'dir' || '/far_seconds.parquet',
    '{"columns": [{"name": "ts", "type": "int64", "values": [0, 1000000000000000, null]}]}');
\set far_seconds :dir '/far_seconds.parquet'
CREATE FOREIGN TABLE t_far_seconds (ts timestamp OPTIONS (unit 'seconds'))
    SERVER pq OPTIONS (files :'far_seconds');
SELECT checked_by($$SELECT count(*) FROM t_far_seconds WHERE ts < TIMESTAMP '2000-01-01'$$)
    AS checked_by;
SELECT count(*) AS rejected FROM t_far_seconds WHERE ts < TIMESTAMP '2000-01-01';
SELECT count(*) AS passed FROM t_far_seconds WHERE ts > TIMESTAMP '2000-01-01';
SELECT sqlstate_of($$SELECT ts FROM t_far_seconds WHERE ts > TIMESTAMP '2000-01-01'$$)
    AS made;
-- The groups a statement's source read and skipped, by EXPLAIN ANALYZE,
-- and the result of a clause against a heap table of the same rows.
CREATE FUNCTION groups_of(statement text) RETURNS text LANGUAGE sql AS $$
    SELECT string_agg(regexp_replace(line, 'Row Groups ', ''), ', ')
    FROM explain_lines('ANALYZE', statement, 'Row Groups (Read|Skipped)') line
$$;
CREATE FUNCTION skipping(foreign_table text, heap text, clause text,
                         OUT groups text, OUT rows_differ bigint, OUT heap_rows bigint)
LANGUAGE sql AS $$
    SELECT groups_of(format('SELECT * FROM %s WHERE %s', foreign_table, clause)),
           differ(format('SELECT * FROM %s WHERE %s', foreign_table, clause),
                  format('SELECT * FROM %s WHERE %s', heap, clause)),
           rows_of(format('SELECT * FROM %s WHERE %s', heap, clause))
$$;

-- spec: parquet-fdw/Groups skipped
-- Eight groups of a hundred rows: a ascending, n with NULL values in the
-- first two groups alone, m NULL in every row of the last two.
SELECT parquet_test_write(:'dir' || '/eight_groups.parquet', json_build_object(
    'group_rows', 100, 'columns', json_build_array(
      json_build_object('name', 'a', 'type', 'int32', 'range', 800),
      json_build_object('name', 'n', 'type', 'int32', 'values',
        (SELECT json_agg(CASE WHEN i < 200 AND i % 7 = 0 THEN NULL ELSE i END ORDER BY i)
         FROM generate_series(0, 799) i)),
      json_build_object('name', 'm', 'type', 'int32', 'values',
        (SELECT json_agg(CASE WHEN i >= 600 THEN NULL ELSE i END ORDER BY i)
         FROM generate_series(0, 799) i))))::text);
\set eight_groups :dir '/eight_groups.parquet'
CREATE FOREIGN TABLE t_eight_groups (a int, n int, m int)
    SERVER pq OPTIONS (files :'eight_groups');
CREATE TABLE h_eight_groups AS SELECT * FROM t_eight_groups;
SELECT c AS clause, s.*
FROM unnest(ARRAY['a >= 250 AND a <= 349', 'a IN (5, 705)', 'n IS NULL',
                  'm IS NOT NULL']) c,
     LATERAL skipping('t_eight_groups', 'h_eight_groups', c) s;
-- Strings through dictionaries: 'only' is in the fourth group's alone,
-- by = and by LIKE.
SELECT parquet_test_write(:'dir' || '/string_groups.parquet', json_build_object(
    'group_rows', 100, 'dictionary', true, 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE WHEN i / 100 = 3 AND i % 10 = 0 THEN 'only'
                              ELSE 'g' || i / 100 || '-' || i % 5 END ORDER BY i)
         FROM generate_series(0, 799) i))))::text);
\set string_groups :dir '/string_groups.parquet'
CREATE FOREIGN TABLE t_string_groups (s text) SERVER pq OPTIONS (files :'string_groups');
CREATE TABLE h_string_groups AS SELECT * FROM t_string_groups;
SELECT c AS clause, s.*
FROM unnest(ARRAY[$c$s = 'only'$c$, $c$s LIKE 'on%'$c$]) c,
     LATERAL skipping('t_string_groups', 'h_string_groups', c) s;
-- Plain strings whose least and greatest values decide: the third group
-- holds only empty strings, and no other group holds one.
SELECT parquet_test_write(:'dir' || '/empty_groups.parquet', json_build_object(
    'group_rows', 100, 'dictionary', false, 'columns', json_build_array(
      json_build_object('name', 'e', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg(CASE WHEN i / 100 = 2 THEN '' WHEN i % 9 = 0 THEN NULL
                              ELSE 'e' || i END ORDER BY i)
         FROM generate_series(0, 799) i))))::text);
\set empty_groups :dir '/empty_groups.parquet'
CREATE FOREIGN TABLE t_empty_groups (e text) SERVER pq OPTIONS (files :'empty_groups');
CREATE TABLE h_empty_groups AS SELECT * FROM t_empty_groups;
SELECT c AS clause, s.*
FROM unnest(ARRAY[$c$e <> ''$c$, $c$e = ''$c$]) c,
     LATERAL skipping('t_empty_groups', 'h_empty_groups', c) s;
-- Dates through dictionaries, in a file written without least and
-- greatest values: group g holds the days 10 g, 10 g + 2 and 10 g + 4
-- after 2013-07-01, so the 32nd day is the fourth group's alone.
SELECT parquet_test_write(:'dir' || '/dated_groups.parquet', json_build_object(
    'group_rows', 100, 'dictionary', true, 'statistics', false, 'columns', json_build_array(
      json_build_object('name', 'd', 'type', 'int32', 'logical', 'date', 'values',
        (SELECT json_agg(DATE '2013-07-01' - DATE '1970-01-01' + i / 100 * 10 + i % 3 * 2
                         ORDER BY i)
         FROM generate_series(0, 799) i))))::text);
\set dated_groups :dir '/dated_groups.parquet'
CREATE FOREIGN TABLE t_dated_groups (d date) SERVER pq OPTIONS (files :'dated_groups');
CREATE TABLE h_dated_groups AS SELECT * FROM t_dated_groups;
SELECT c AS clause, s.*
FROM unnest(ARRAY[$c$d = DATE '2013-08-02'$c$]) c,
     LATERAL skipping('t_dated_groups', 'h_dated_groups', c) s;
-- 2 between the least and the greatest of a group of 1 and 3: ruled out
-- by its dictionary.
SELECT parquet_test_write(:'dir' || '/odd_values.parquet',
    '{"dictionary": true, "columns": [{"name": "v", "type": "int32", "values": [1, 3, 1, 3]}]}');
\set odd_values :dir '/odd_values.parquet'
CREATE FOREIGN TABLE t_odd_values (v int) SERVER pq OPTIONS (files :'odd_values');
CREATE TABLE h_odd_values AS SELECT * FROM t_odd_values;
SELECT c AS clause, s.*
FROM unnest(ARRAY['v = 2']) c, LATERAL skipping('t_odd_values', 'h_odd_values', c) s;
-- Dictionary pages headed PLAIN_DICTIONARY, as ClickBench's are.
SELECT parquet_test_write(:'dir' || '/headed_groups.parquet', json_build_object(
    'group_rows', 100, 'dictionary', true, 'statistics', false, 'columns', json_build_array(
      json_build_object('name', 'd', 'type', 'int32', 'logical', 'date', 'values',
        (SELECT json_agg(DATE '2013-07-01' - DATE '1970-01-01' + i / 100 * 10 + i % 3 * 2
                         ORDER BY i)
         FROM generate_series(0, 799) i))))::text);
SELECT parquet_test_head_dictionaries(:'dir' || '/headed_groups.parquet');
\set headed_groups :dir '/headed_groups.parquet'
CREATE FOREIGN TABLE t_headed_groups (d date) SERVER pq OPTIONS (files :'headed_groups');
SELECT c AS clause, s.*
FROM unnest(ARRAY[$c$d = DATE '2013-08-02'$c$]) c,
     LATERAL skipping('t_headed_groups', 'h_dated_groups', c) s;

-- spec: parquet-fdw/Groups the footer cannot rule out
-- A value in the plain pages a dictionary fell back to, in a file
-- without least and greatest values.
SELECT parquet_test_write(:'dir' || '/fallen_back.parquet',
    '{"dictionary": true, "dictionary_bytes": 64, "page_rows": 50, "statistics": false,
      "columns": [{"name": "v", "type": "int32", "range": 400}]}');
\set fallen_back :dir '/fallen_back.parquet'
CREATE FOREIGN TABLE t_fallen_back (v int) SERVER pq OPTIONS (files :'fallen_back');
CREATE TABLE h_fallen_back AS SELECT * FROM t_fallen_back;
SELECT c AS clause, s.*
FROM unnest(ARRAY['v = 399']) c, LATERAL skipping('t_fallen_back', 'h_fallen_back', c) s;
-- A value outside the dictionary of a chunk whose footer says nothing of
-- the encodings of its pages.
SELECT parquet_test_write(:'dir' || '/unsaid.parquet',
    '{"dictionary": true, "statistics": false,
      "columns": [{"name": "v", "type": "int32", "values": [1, 3, 1, 3]}]}');
SELECT parquet_test_rewrite(:'dir' || '/unsaid.parquet', false, true, false, -1, -1);
\set unsaid :dir '/unsaid.parquet'
CREATE FOREIGN TABLE t_unsaid (v int) SERVER pq OPTIONS (files :'unsaid');
SELECT c AS clause, s.*
FROM unnest(ARRAY['v = 2']) c, LATERAL skipping('t_unsaid', 'h_odd_values', c) s;
-- Unsigned integers whose least and greatest values are in the older
-- fields, sorted as signed, with and without column orders; signed ones
-- in the newer fields without column orders.
SELECT parquet_test_write(:'dir' || '/unsigned_' || o || '.parquet',
    '{"columns": [{"name": "u", "type": "int32", "logical": "uint32",
                   "values": [1, 4000000000]},
                  {"name": "i", "type": "int32", "values": [1, 2]}]}')
FROM unnest(ARRAY['old', 'old_ordered', 'unordered']) o;
SELECT parquet_test_rewrite(:'dir' || '/unsigned_old.parquet', true, false, false, -1, -1),
       parquet_test_rewrite(:'dir' || '/unsigned_old_ordered.parquet', true, false, false, -1, 1),
       parquet_test_rewrite(:'dir' || '/unsigned_unordered.parquet', false, false, false, -1, 0);
\set unsigned_old :dir '/unsigned_old.parquet'
\set unsigned_old_ordered :dir '/unsigned_old_ordered.parquet'
\set unsigned_unordered :dir '/unsigned_unordered.parquet'
CREATE FOREIGN TABLE t_unsigned_old (u bigint, i int)
    SERVER pq OPTIONS (files :'unsigned_old');
CREATE FOREIGN TABLE t_unsigned_old_ordered (u bigint, i int)
    SERVER pq OPTIONS (files :'unsigned_old_ordered');
CREATE FOREIGN TABLE t_unsigned_unordered (u bigint, i int)
    SERVER pq OPTIONS (files :'unsigned_unordered');
CREATE TABLE h_unsigned (u bigint, i int);
INSERT INTO h_unsigned VALUES (1, 1), (4000000000, 2);
SELECT t AS foreign_table, c AS clause, s.*
FROM unnest(ARRAY['t_unsigned_old', 't_unsigned_old_ordered']) t,
     unnest(ARRAY['u > 2000000000']) c,
     LATERAL skipping(t, 'h_unsigned', c) s;
SELECT c AS clause, s.*
FROM unnest(ARRAY['i > 100']) c, LATERAL skipping('t_unsigned_unordered', 'h_unsigned', c) s;
-- IS NULL on a chunk without a count of NULL values, every page through
-- its dictionary, holding NULL values.
SELECT parquet_test_write(:'dir' || '/uncounted.parquet',
    '{"dictionary": true, "columns": [{"name": "v", "type": "int32", "values": [1, null, 3, null]}]}');
SELECT parquet_test_rewrite(:'dir' || '/uncounted.parquet', false, false, true, -1, -1);
\set uncounted :dir '/uncounted.parquet'
CREATE FOREIGN TABLE t_uncounted (v int) SERVER pq OPTIONS (files :'uncounted');
CREATE TABLE h_uncounted (v int);
INSERT INTO h_uncounted VALUES (1), (NULL), (3), (NULL);
SELECT c AS clause, s.*
FROM unnest(ARRAY['v IS NULL']) c, LATERAL skipping('t_uncounted', 'h_uncounted', c) s;

-- The paths cost the chunks of the columns they read, as the footers
-- give their bytes: a query that reads no column costs no page, a row
-- the core's cost of a tuple; and a column of long strings costs more
-- pages than one of an integer, through either path.
CREATE FUNCTION scan_cost(statement text, node text) RETURNS numeric LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
    RETURN (jsonb_path_query_first(plan,
        '$.** ? (@."Node Type" == $n || @."Custom Plan Provider" == $n)."Total Cost"',
        jsonb_build_object('n', node)))::numeric;
END $$;
SELECT parquet_test_write(:'dir' || '/widths.parquet', json_build_object(
    'columns', json_build_array(
      json_build_object('name', 'small', 'type', 'int32', 'values',
        (SELECT json_agg(1) FROM generate_series(1, 2000))),
      json_build_object('name', 'big', 'type', 'bytes', 'values',
        (SELECT json_agg(md5(n || 'a') || md5(n || 'b') || md5(n || 'c') || md5(n || 'd') ORDER BY n)
         FROM generate_series(1, 2000) n))))::text);
\set widths :dir '/widths.parquet'
CREATE FOREIGN TABLE t_widths (small int, big text) SERVER pq OPTIONS (files :'widths');
SET tessera.enable = off;
SELECT scan_cost('SELECT count(*) FROM t_widths', 'Foreign Scan') =
       2000 * current_setting('cpu_tuple_cost')::numeric AS no_page_for_a_count,
       scan_cost('SELECT big FROM t_widths', 'Foreign Scan') -
       scan_cost('SELECT small FROM t_widths', 'Foreign Scan') > 10 AS big_pages_wrapper;
RESET tessera.enable;
SELECT scan_cost('SELECT big FROM t_widths', 'TessParquetScan') -
       scan_cost('SELECT small FROM t_widths', 'TessParquetScan') > 10 AS big_pages_source;

-- spec: parquet-fdw/One file in parallel
-- A table of one file of eight groups read by a parallel plan, with
-- Tessera off and on: in full, under a count, under a grouping, and as
-- the inner side of a nested loop that rescans the parallel plan for
-- each outer row. Each result equals the serial one, the plan has the
-- workers the core gives the file's size, and the groups read add up to
-- eight a scan. The counts of EXPLAIN ANALYZE are those of the serial
-- plan (the requirement "EXPLAIN").
SELECT parquet_test_write(:'dir' || '/eight.parquet', json_build_object(
    'group_rows', 1000, 'columns', json_build_array(
      json_build_object('name', 'i', 'type', 'int64', 'range', 8000),
      json_build_object('name', 's', 'type', 'bytes', 'values',
        (SELECT json_agg('s' || (n % 13) ORDER BY n) FROM generate_series(0, 7999) n))))::text);
\set eight :dir '/eight.parquet'
CREATE FOREIGN TABLE t_eight (i bigint, s text) SERVER pq OPTIONS (files :'eight');
-- The rows a parallel plan of a query differs by from its serial plan's,
-- both kept in tables, counted both ways.
CREATE FUNCTION differ_serial(statement text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    n bigint;
    saved text := current_setting('max_parallel_workers_per_gather');
BEGIN
    EXECUTE 'CREATE TEMP TABLE parallel_rows AS ' || statement;
    PERFORM set_config('max_parallel_workers_per_gather', '0', true);
    EXECUTE 'CREATE TEMP TABLE serial_rows AS ' || statement;
    PERFORM set_config('max_parallel_workers_per_gather', saved, true);
    SELECT count(*) INTO n FROM ((TABLE parallel_rows EXCEPT ALL TABLE serial_rows)
                                 UNION ALL (TABLE serial_rows EXCEPT ALL TABLE parallel_rows)) d;
    DROP TABLE parallel_rows, serial_rows;
    RETURN n;
END $$;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET enable_material = off;
-- The workers the core gives the file's 14 pages: none under the
-- default threshold of a parallel scan, and more as the threshold falls.
SET max_parallel_workers_per_gather = 4;
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_eight', 'Workers Planned');
SET min_parallel_table_scan_size = '40kB';
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_eight', 'Workers Planned');
SET min_parallel_table_scan_size = '16kB';
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_eight', 'Workers Planned');
SET min_parallel_table_scan_size = 0;
SELECT explain_lines('ANALYZE false', 'SELECT count(*) FROM t_eight', 'Workers Planned');
SET max_parallel_workers_per_gather = 2;
-- The workers alone read, so that they divide the groups.
SET parallel_leader_participation = off;
SET tessera.enable = off;
SELECT parallel_lines('SELECT i, s FROM t_eight', 'Workers Planned|Parallel|Row Groups');
SELECT parallel_lines('SELECT * FROM (SELECT count(*) AS n, sum(i) AS total FROM t_eight) ss
                       RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true',
                      'Nested Loop|Parallel|Row Groups Read');
SELECT workers_launched('SELECT i, s FROM t_eight') > 0 AS workers_read;
SELECT differ_serial('SELECT i, s FROM t_eight') AS in_full,
       differ_serial('SELECT count(*), sum(i) FROM t_eight') AS a_count,
       differ_serial('SELECT s, count(*), sum(i) FROM t_eight GROUP BY s') AS a_grouping,
       differ_serial('SELECT * FROM (SELECT count(*) AS n, sum(i) AS total FROM t_eight) ss
                      RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true') AS rescanned;
RESET tessera.enable;
SELECT parallel_lines('SELECT count(*), sum(i), max(s) FROM t_eight',
                      'Workers Planned|Parallel|Row Groups|Values Decoded');
SELECT parallel_lines('SELECT * FROM (SELECT count(*) AS n, sum(i) AS total FROM t_eight) ss
                       RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true',
                      'Nested Loop|Parallel|Row Groups Read|Values Decoded');
SELECT workers_launched('SELECT count(*), sum(i), max(s) FROM t_eight') > 0 AS workers_read;
-- spec: string-bytes/Ordered by bytes
-- Ordered by a string through an ordered gather: each worker's TessSort
-- by the words of its bytes, the gather's merge by the string's
-- comparison, the generic kind; the rows in the core's order.
SELECT regexp_replace(line, ' \(actual [^)]*\)', '')
FROM explain_lines('ANALYZE, VERBOSE', 'SELECT s, i FROM t_eight ORDER BY s COLLATE "C", i',
                   'Gather|TessSort|Abbreviated|Workers Planned') AS line;
SELECT order_off('SELECT s, i FROM t_eight ORDER BY s COLLATE "C", i') AS ascending,
       order_off('SELECT s, i FROM t_eight ORDER BY s COLLATE "C" DESC, i') AS descending,
       order_off('SELECT s, i FROM t_eight ORDER BY s COLLATE "C", i LIMIT 10') AS top_10;
-- spec: string-bytes/Plain and dictionary chunks
-- A grouping by a text column, and a count of its distinct values, over a
-- table of two files, one of plain pages and one through a dictionary,
-- with NULL rows and empty strings, against Tessera off: the scan makes
-- no value of the column, Values Decoded counting none.
\set bytes_both :bytes_plain ',' :bytes_dict
CREATE FOREIGN TABLE t_bytes_both (s text, i int) SERVER pq OPTIONS (files :'bytes_both');
SET max_parallel_workers_per_gather = 0;
SELECT differ_off('SELECT s, count(*) FROM t_bytes_both GROUP BY s') AS by_text,
       differ_off('SELECT count(DISTINCT s) FROM t_bytes_both') AS distinct_text;
SELECT explain_lines('ANALYZE', 'SELECT s, count(*) FROM t_bytes_both GROUP BY s',
                     'TessAgg|Decoded');
SET max_parallel_workers_per_gather = 2;
DROP FOREIGN TABLE t_bytes_both;
-- spec: string-bytes/A partial stage's tables sent up
-- A parallel grouping by a string of 8000 values of some 65 bytes through
-- the chunks' dictionaries, under a work_mem its workers' tables outgrow,
-- each value's 20 rows together so that a table folds them: each worker's
-- partial stage sends a table up in the middle of a chunk and starts the
-- next over the same chunk's dictionary, its numbers of the indexes
-- dropped with the key dictionary it resets; the groups of the core.
SELECT parquet_test_write(:'dir' || '/eight_many.parquet', json_build_object(
    'group_rows', 20000, 'dictionary', true, 'columns', json_build_array(
      json_build_object('name', 's', 'type', 'bytes', 'logical', 'string', 'values',
        (SELECT json_agg('v' || n / 20 || repeat('ü', 30) ORDER BY n)
         FROM generate_series(0, 159999) n))))::text);
\set eight_many :dir '/eight_many.parquet'
CREATE FOREIGN TABLE t_eight_many (s text) SERVER pq OPTIONS (files :'eight_many');
SET work_mem = '64kB';
\set eight_groups 'SELECT md5(string_agg(s || \':\' || c, \',\' ORDER BY s)) AS groups FROM (SELECT s, count(*) AS c FROM t_eight_many GROUP BY s) q'
:eight_groups \gset tess_
SET tessera.enable = off;
:eight_groups \gset core_
RESET tessera.enable;
SELECT :'tess_groups' = :'core_groups' AS same_as_core;
SELECT count(*) FILTER (WHERE line ~ 'Early Emits') > 0 AS sent_up,
       count(*) FILTER (WHERE line ~ 'Partial Mode: Partial') > 0 AS partial_stage
FROM explain_lines('ANALYZE, VERBOSE', :'eight_groups', 'Early Emits|Partial Mode') line;
RESET work_mem;
DROP FOREIGN TABLE t_eight_many;
-- A clause the source checks, each participant selecting the rows of
-- the groups it takes, the value of a subquery among them.
SELECT parallel_lines('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)',
                      'Gather|TessParquetScan');
SELECT checked_by('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)') AS checked_by;
SELECT workers_launched('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)') > 0
    AS workers_read;
-- The groups each participant skips, summed: the last three, as serial.
SELECT groups_of('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)') AS parallel_groups;
SELECT differ_serial('SELECT i, s FROM t_eight WHERE i % 2 = 0 AND i < 4500') AS filtered,
       differ_serial('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)') AS by_a_subquery;
SELECT differ_serial('SELECT i, s FROM t_eight') AS in_full,
       differ_serial('SELECT count(*), sum(i) FROM t_eight') AS a_count,
       differ_serial('SELECT s, count(*), sum(i) FROM t_eight GROUP BY s') AS a_grouping,
       differ_serial('SELECT * FROM (SELECT count(*) AS n, sum(i) AS total FROM t_eight) ss
                      RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true') AS rescanned;
RESET parallel_leader_participation;
SET max_parallel_workers_per_gather = 0;
SELECT groups_of('SELECT count(*) FROM t_eight WHERE i < (SELECT 4500)') AS serial_groups;
SELECT explain_lines('ANALYZE', 'SELECT count(*), sum(i), max(s) FROM t_eight',
                     'Row Groups|Values Decoded');
RESET max_parallel_workers_per_gather;
RESET min_parallel_table_scan_size;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET enable_material;

-- spec: parquet-fdw/Files replaced, added, removed and damaged
-- A prepared statement over a pattern: its file replaced by one of other
-- rows, a second file added, both removed, one put back that lacks the
-- column; a cursor whose file changes under it; and files damaged. Once
-- with Tessera off and once with it on; the errors name the files' paths,
-- so their SQLSTATE alone is shown.
\set rep :dir '/rep*.parquet'
\set cur :dir '/cur.parquet'
\set cur_spec '{"group_rows": 10000, "page_rows": 100, "columns": [{"name": "i", "type": "int64", "range": 10000}]}'
\set two :dir '/two1.parquet,' :dir '/two2.parquet'
\set cut :dir '/cut.parquet'
\set damaged :dir '/damaged.parquet'
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [1, 2]}]}');
SELECT parquet_test_write(:'dir' || '/cur.parquet', :'cur_spec');
SELECT parquet_test_write(:'dir' || format('/two%s.parquet', n),
    format('{"columns": [{"name": "i", "type": "int64", "values": [%s, %s]}]}', n * 10, n * 10 + 1))
FROM generate_series(1, 2) n;
SELECT parquet_test_write(f, '{"columns": [{"name": "i", "type": "int64", "values": [1, 2]}]}')
FROM unnest(ARRAY[:'cut', :'damaged']) f;
CREATE FOREIGN TABLE t_rep (a int) SERVER pq OPTIONS (files :'rep');
CREATE FOREIGN TABLE t_cur (i bigint) SERVER pq OPTIONS (files :'cur');
CREATE FOREIGN TABLE t_two (i bigint) SERVER pq OPTIONS (files :'two');
CREATE FOREIGN TABLE t_cut (i bigint) SERVER pq OPTIONS (files :'cut');
CREATE FOREIGN TABLE t_damaged (i bigint) SERVER pq OPTIONS (files :'damaged');
SELECT parquet_test_cut(:'cut', 4);
SELECT parquet_test_damage(:'damaged', 4, 16);
SET tessera.enable = off;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [1, 2]}]}');
SET plan_cache_mode = force_generic_plan;
PREPARE read_rep AS SELECT a FROM t_rep;
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [3, 4]}]}');
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep2.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [5]}]}');
EXECUTE read_rep;
SELECT parquet_test_remove(:'dir' || '/rep1.parquet'), parquet_test_remove(:'dir' || '/rep2.parquet');
\set VERBOSITY sqlstate
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "b", "type": "int32", "values": [6]}]}');
EXECUTE read_rep;
\set VERBOSITY default
DEALLOCATE read_rep;
RESET plan_cache_mode;
-- A cursor over a group longer than a window, its file rewritten in place
-- with the same size and a time a minute later.
SELECT parquet_test_write(:'dir' || '/cur.parquet', :'cur_spec');
SELECT parquet_test_write(:'dir' || '/cur_copy.parquet', :'cur_spec');
SELECT scans_of('SELECT i FROM t_cur') AS through;
BEGIN;
DECLARE c CURSOR FOR SELECT i FROM t_cur;
FETCH 2 FROM c;
SELECT parquet_test_replace(:'dir' || '/cur_copy.parquet', :'dir' || '/cur.parquet', 'in place');
\set VERBOSITY sqlstate
FETCH ALL FROM c;
\set VERBOSITY default
ROLLBACK;
-- A serial scan of two files, the second replaced by a rename with a
-- file of the same bytes and time while it reads the first.
BEGIN;
DECLARE c CURSOR FOR SELECT i FROM t_two;
FETCH 1 FROM c;
SELECT parquet_test_replace(:'dir' || '/two2.parquet', :'dir' || '/two2.parquet', 'rename');
\set VERBOSITY sqlstate
FETCH ALL FROM c;
\set VERBOSITY default
ROLLBACK;
-- A footer cut short and a page damaged.
\set VERBOSITY sqlstate
SELECT i FROM t_cut;
SELECT i FROM t_damaged;
\set VERBOSITY default
RESET tessera.enable;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [1, 2]}]}');
SET plan_cache_mode = force_generic_plan;
PREPARE read_rep AS SELECT a FROM t_rep;
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [3, 4]}]}');
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep2.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [5]}]}');
EXECUTE read_rep;
SELECT parquet_test_remove(:'dir' || '/rep1.parquet'), parquet_test_remove(:'dir' || '/rep2.parquet');
\set VERBOSITY sqlstate
EXECUTE read_rep;
SELECT parquet_test_write(:'dir' || '/rep1.parquet',
    '{"columns": [{"name": "b", "type": "int32", "values": [6]}]}');
EXECUTE read_rep;
\set VERBOSITY default
DEALLOCATE read_rep;
RESET plan_cache_mode;
-- A cursor over a group longer than a window, its file rewritten in place
-- with the same size and a time a minute later.
SELECT parquet_test_write(:'dir' || '/cur.parquet', :'cur_spec');
SELECT parquet_test_write(:'dir' || '/cur_copy.parquet', :'cur_spec');
SELECT scans_of('SELECT i FROM t_cur') AS through;
BEGIN;
DECLARE c CURSOR FOR SELECT i FROM t_cur;
FETCH 2 FROM c;
SELECT parquet_test_replace(:'dir' || '/cur_copy.parquet', :'dir' || '/cur.parquet', 'in place');
\set VERBOSITY sqlstate
FETCH ALL FROM c;
\set VERBOSITY default
ROLLBACK;
-- A serial scan of two files, the second replaced by a rename with a
-- file of the same bytes and time while it reads the first.
BEGIN;
DECLARE c CURSOR FOR SELECT i FROM t_two;
FETCH 1 FROM c;
SELECT parquet_test_replace(:'dir' || '/two2.parquet', :'dir' || '/two2.parquet', 'rename');
\set VERBOSITY sqlstate
FETCH ALL FROM c;
\set VERBOSITY default
ROLLBACK;
-- A footer cut short and a page damaged.
\set VERBOSITY sqlstate
SELECT i FROM t_cut;
SELECT i FROM t_damaged;
\set VERBOSITY default

DROP FOREIGN TABLE t_files, t_owned, t_plain, t_none, t_gzip, t_types, t_drop, t_clickbench,
    t_mappings, t_check_plain, t_check_dict, t_trust, t_bytes, t_vc_three, t_vc_four, t_vc_trust,
    t_rep, t_cur, t_two, t_cut, t_damaged, t_far, t_windows, t_empty, t_wide, t_asks,
    t_damaged_pages, t_eight, t_groups, t_analyzed, t_sparse, t_alternating, t_widths,
    t_one_more, t_clauses, t_far_seconds, t_mixed, t_eight_groups, t_dated_groups, t_odd_values,
    t_headed_groups, t_fallen_back, t_unsaid, t_unsigned_old, t_unsigned_old_ordered,
    t_unsigned_unordered, t_uncounted, t_strings, t_string_groups, t_empty_groups, t_leaves,
    t_leaves_bytes;
DROP TABLE h_files, h_clickbench, h_mappings, h_windows, h_wide, h_keys, h_asks, h_analyzed,
    h_clauses, h_outer, h_mixed, h_eight_groups, h_dated_groups, h_odd_values, h_fallen_back,
    h_unsigned, h_uncounted, h_strings, h_string_groups, h_empty_groups, h_leaves,
    leaves_queries;
DROP COLLATION parquet_ci;
DROP FUNCTION parquet_test_write(text, text), parquet_test_rewrite(text, bool, bool, bool, int, int),
    parquet_test_head_dictionaries(text), parquet_test_replace(text, text, text),
    parquet_test_remove(text), parquet_test_cut(text, int), parquet_test_damage(text, int, int),
    sqlstate_of(text), differ(text, text), differ_off(text), scans_of(text),
    explain_lines(text, text, text), parquet_test_asks(text, int, int, bool), differ_serial(text),
    parallel_lines(text, text), workers_launched(text), plan_of(text), scan_cost(text, text),
    checked_by(text), rows_of(text), groups_of(text), skipping(text, text, text),
    order_off(text), leaves_value(int, boolean), leaves_short(int), leaves_json(bytea),
    leaves_order(text);
DROP SERVER pq;
REVOKE CREATE ON SCHEMA public FROM parquet_reader, parquet_other;
DROP ROLE parquet_reader, parquet_other;
DROP EXTENSION tessera_parquet;
DROP EXTENSION tessera;
