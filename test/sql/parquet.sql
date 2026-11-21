-- The foreign data wrapper tessera_parquet over files its test module
-- writes: the options and their privileges, the files and patterns, and
-- the columns refused, at planning and when a scan begins.
\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set parquet_test :libdir '/tessera_parquet_test' :dlsuffix
CREATE EXTENSION tessera;
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
DROP DATABASE parquet_latin1;
SELECT set_config('parquet.dir', :'dir', false) IS NOT NULL AS dir_set_again;

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
    t_rep, t_cur, t_two, t_cut, t_damaged, t_far, t_windows, t_empty;
DROP TABLE h_files, h_clickbench, h_mappings, h_windows;
DROP FUNCTION parquet_test_write(text, text), parquet_test_rewrite(text, bool, bool, bool, int),
    parquet_test_head_dictionaries(text), parquet_test_replace(text, text, text),
    parquet_test_remove(text), parquet_test_cut(text, int), parquet_test_damage(text, int, int),
    sqlstate_of(text), differ(text, text);
DROP SERVER pq;
REVOKE CREATE ON SCHEMA public FROM parquet_reader, parquet_other;
DROP ROLE parquet_reader, parquet_other;
DROP EXTENSION tessera_parquet;
DROP EXTENSION tessera;
