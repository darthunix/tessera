\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set heap_deform_test :libdir '/tessera_heap_deform_test' :dlsuffix
LOAD :'heap_deform_test';

CREATE FUNCTION tessera_test_heap_deform(regclass)
RETURNS bigint
AS :'heap_deform_test', 'tessera_test_heap_deform'
LANGUAGE C STRICT;

-- A guaranteed int4 first, NULLs in every position, short and long text,
-- a bigint aligned after text, and a text at the end.
CREATE TABLE deform_t (a int NOT NULL, b text, c bigint, d int, e text);
INSERT INTO deform_t
SELECT i,
       CASE WHEN i % 3 = 0 THEN NULL
            WHEN i % 5 = 0 THEN repeat('x', 200)
            ELSE 'r' || i END,
       CASE WHEN i % 4 = 0 THEN NULL ELSE i * 100000000000 END,
       CASE WHEN i % 7 = 0 THEN NULL ELSE i END,
       CASE WHEN i % 2 = 0 THEN NULL ELSE 'e' || i END
FROM generate_series(1, 300) AS i;
INSERT INTO deform_t (a) VALUES (1000);
SELECT tessera_test_heap_deform('deform_t');

-- Tuples shorter than the descriptor: a missing value and a missing NULL.
ALTER TABLE deform_t ADD COLUMN f int DEFAULT 42;
ALTER TABLE deform_t ADD COLUMN g text;
INSERT INTO deform_t VALUES (2000, 'new', 1, 2, 'x', 7, 'g');
SELECT tessera_test_heap_deform('deform_t');

-- Every attribute guaranteed: fixed width, by value, NOT NULL.
CREATE TABLE deform_fixed (a int NOT NULL, b int NOT NULL, c bigint NOT NULL);
INSERT INTO deform_fixed SELECT i, -i, i * 3 FROM generate_series(1, 200) AS i;
SELECT tessera_test_heap_deform('deform_fixed');

-- Nothing guaranteed: a nullable text first.
CREATE TABLE deform_text (a text, b int, c text);
INSERT INTO deform_text
SELECT CASE WHEN i % 2 = 0 THEN NULL ELSE 'a' || i END, i,
       CASE WHEN i % 3 = 0 THEN NULL ELSE 'c' || i END
FROM generate_series(1, 100) AS i;
SELECT tessera_test_heap_deform('deform_text');

DROP TABLE deform_t, deform_fixed, deform_text;
DROP FUNCTION tessera_test_heap_deform(regclass);
