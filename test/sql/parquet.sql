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
CREATE FUNCTION parquet_test_rewrite(text, bool, bool, bool, int) RETURNS void
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
SELECT parquet_test_write(:'dir' || format('/bad_%s.parquet', d), format('{"dictionary": %s, "columns": [
      {"name": "m_bad", "type": "bytes", "logical": "string", "values": ["a", {"hex": "ff"}]},
      {"name": "u_bad", "type": "bytes", "values": ["a", {"hex": "ff"}]},
      {"name": "m_zero", "type": "bytes", "logical": "string", "values": ["a", {"hex": "6100"}]},
      {"name": "u_zero", "type": "bytes", "values": ["a", {"hex": "6100"}]}]}', d))
FROM unnest(ARRAY['false', 'true']) d;
\set bad_plain :dir '/bad_false.parquet'
\set bad_dict :dir '/bad_true.parquet'
CREATE FOREIGN TABLE t_check_plain (m_bad text, u_bad text, m_zero text, u_zero text)
    SERVER pq OPTIONS (files :'bad_plain');
CREATE FOREIGN TABLE t_check_dict (m_bad text, u_bad text, m_zero text, u_zero text)
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

-- spec: parquet-fdw/The length of a varchar
-- Into varchar(3): three characters of two bytes each are returned, four
-- characters raise 22001, and a trusted varchar(3) is refused, 42804.
SELECT parquet_test_write(:'dir' || '/vc_three.parquet',
    '{"columns": [{"name": "s", "type": "bytes", "logical": "string", "values": ["абв", "ab"]}]}');
SELECT parquet_test_write(:'dir' || '/vc_four.parquet',
    '{"columns": [{"name": "s", "type": "bytes", "values": ["абв", "abcd"]}]}');
\set vc_three :dir '/vc_three.parquet'
\set vc_four :dir '/vc_four.parquet'
CREATE FOREIGN TABLE t_vc_three (s varchar(3)) SERVER pq OPTIONS (files :'vc_three');
CREATE FOREIGN TABLE t_vc_four (s varchar(3)) SERVER pq OPTIONS (files :'vc_four');
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

-- A column compressed by a codec other than Snappy is refused, its
-- neighbours planned.
SELECT parquet_test_write(current_setting('parquet.dir') || '/gzip.parquet',
    '{"compression": "snappy", "columns": [{"name": "a", "type": "int32", "values": [1, 2]},
                  {"name": "b", "type": "int32", "values": [3, 4]}]}');
SELECT parquet_test_rewrite(current_setting('parquet.dir') || '/gzip.parquet', false, false, false, 1);
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
-- A filter keeps eight rows, in windows after gaps, two in one batch and
-- across a group's end: the nine other columns are made for those alone,
-- each read for the rows asked where its reader passed no window.
SELECT scans_of('SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)')
    AS filtered;
SELECT explain_lines('ANALYZE',
    'SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)',
    'TessParquetScan|Values Decoded');
SELECT differ_off('SELECT * FROM t_wide WHERE k IN (5, 2100, 2900, 2901, 29999, 30000, 61000, 99999)')
    AS rows_differ;
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
CREATE FUNCTION parquet_test_asks(text, int, int)
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
SELECT differ_serial('SELECT i, s FROM t_eight') AS in_full,
       differ_serial('SELECT count(*), sum(i) FROM t_eight') AS a_count,
       differ_serial('SELECT s, count(*), sum(i) FROM t_eight GROUP BY s') AS a_grouping,
       differ_serial('SELECT * FROM (SELECT count(*) AS n, sum(i) AS total FROM t_eight) ss
                      RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true') AS rescanned;
RESET parallel_leader_participation;
SET max_parallel_workers_per_gather = 0;
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
    t_damaged_pages, t_eight;
DROP TABLE h_files, h_clickbench, h_mappings, h_windows, h_wide, h_keys, h_asks;
DROP FUNCTION parquet_test_write(text, text), parquet_test_rewrite(text, bool, bool, bool, int),
    parquet_test_head_dictionaries(text), parquet_test_replace(text, text, text),
    parquet_test_remove(text), parquet_test_cut(text, int), parquet_test_damage(text, int, int),
    sqlstate_of(text), differ(text, text), differ_off(text), scans_of(text),
    explain_lines(text, text, text), parquet_test_asks(text, int, int), differ_serial(text),
    parallel_lines(text, text), workers_launched(text);
DROP SERVER pq;
REVOKE CREATE ON SCHEMA public FROM parquet_reader, parquet_other;
DROP ROLE parquet_reader, parquet_other;
DROP EXTENSION tessera_parquet;
DROP EXTENSION tessera;
