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

-- The SQLSTATE a statement raises, or ok: the paths of the files, which
-- the messages name, differ from one machine to another.
CREATE FUNCTION sqlstate_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE statement;
    RETURN 'ok';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE;
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

-- A column compressed by a codec other than Snappy is refused, its
-- neighbours planned.
SELECT parquet_test_write(current_setting('parquet.dir') || '/gzip.parquet',
    '{"columns": [{"name": "a", "type": "int32", "values": [1, 2]},
                  {"name": "b", "type": "int32", "values": [3, 4]}]}');
SELECT parquet_test_rewrite(current_setting('parquet.dir') || '/gzip.parquet', false, false, false, 1);
CREATE FOREIGN TABLE t_gzip (a int, b int) SERVER pq OPTIONS (files :'gzip');
SELECT sqlstate_of('EXPLAIN SELECT b FROM t_gzip') AS gzip_column;
SELECT sqlstate_of('EXPLAIN SELECT a FROM t_gzip') AS snappy_beside_gzip;

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
SELECT sqlstate_of('EXPLAIN SELECT s FROM t_latin') AS text_in_latin1;
-- The reader's message, of a name it quotes, in the database's encoding.
EXPLAIN SELECT "è" FROM t_latin;
\c :main_db
DROP DATABASE parquet_latin1;
SELECT set_config('parquet.dir', :'dir', false) IS NOT NULL AS dir_set_again;

-- The columns are checked again when a scan begins: a plan kept by a
-- prepared statement, its file replaced after planning by one that lacks
-- the column. The rows themselves are not read yet.
SET plan_cache_mode = force_generic_plan;
PREPARE read_a AS SELECT a FROM t_plain;
SELECT sqlstate_of('EXECUTE read_a') AS planned_and_begun;
SELECT parquet_test_write(current_setting('parquet.dir') || '/one.parquet',
    '{"columns": [{"name": "b", "type": "int32", "values": [1]}]}');
SELECT sqlstate_of('EXECUTE read_a') AS checked_when_the_scan_begins;
DEALLOCATE read_a;
RESET plan_cache_mode;

DROP FOREIGN TABLE t_files, t_owned, t_plain, t_none, t_gzip, t_types, t_drop;
DROP SERVER pq;
REVOKE CREATE ON SCHEMA public FROM parquet_reader, parquet_other;
DROP ROLE parquet_reader, parquet_other;
DROP EXTENSION tessera_parquet;
DROP EXTENSION tessera;
