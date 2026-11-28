-- The like family: patterns of LIKE whose pieces between % signs the
-- kernels search for in each string, over comments of English words as
-- TPC-H's, URLs as ClickBench's and short strings; and guards whose
-- patterns the kernels match at one place or leave to the core. A ratio
-- below one is the win.
\set ON_ERROR_STOP on
\if :{?repetitions}
\else
\set repetitions 31
\endif
\if :{?workers}
\else
\set workers 0
\endif
SET jit = off;
-- Parallel workers per Gather, for both modes; none unless asked for.
SET max_parallel_workers_per_gather = :workers;

-- The family's own table, made once: 2 million rows of a comment c of
-- four to twelve words of TPC-H's kind, each drawn by a hash of the row
-- and its place, cut to 19 to 78 bytes as TPC-H's o_comment is; a URL u
-- of 40 to 95 bytes, a host, three words and a hash, one in about a
-- hundred of Google's; and a string x of 8 to 16 hexadecimal digits.
CREATE TABLE IF NOT EXISTS bench_like AS
SELECT left(array_to_string((SELECT array_agg(w[1 + abs(hashint8(g::bigint * 16 + k)) % 40]
                                              ORDER BY k)
                             FROM generate_series(1, 4 + g % 9) AS k), ' '),
            19 + g % 60) AS c,
       left('https://' ||
            CASE WHEN g % 97 = 0 THEN 'www.google.com' ELSE 'www.site' || g % 5003 || '.ru' END ||
            '/' || w[1 + abs(hashint8(g::bigint * 16 + 13)) % 40] ||
            '/' || w[1 + abs(hashint8(g::bigint * 16 + 14)) % 40] ||
            '-' || w[1 + abs(hashint8(g::bigint * 16 + 15)) % 40] ||
            '/' || md5(g::text) || '.html', 40 + g % 56) AS u,
       left(md5((g * 31)::text), 8 + g % 9) AS x
FROM generate_series(1, 2000000) AS g,
     LATERAL (SELECT ARRAY['furiously', 'special', 'requests', 'carefully', 'quickly',
                           'deposits', 'accounts', 'packages', 'ironic', 'final', 'regular',
                           'pending', 'express', 'bold', 'slyly', 'blithely', 'even',
                           'unusual', 'theodolites', 'instructions', 'foxes', 'pinto',
                           'beans', 'dependencies', 'asymptotes', 'excuses', 'platelets',
                           'sleep', 'nag', 'haggle', 'wake', 'cajole', 'detect',
                           'integrate', 'boost', 'use', 'are', 'about', 'among',
                           'across'] AS w) AS words;
VACUUM (ANALYZE) bench_like;

\ir reading.sql

/*
 * A prepared statement without parameters is planned at its first
 * execution and cached, so the mode set here decides its plan for good:
 * every statement is prepared twice, once per mode.
 */
CREATE FUNCTION pg_temp.measure(test_name text, mode text,
                                statement_name text, repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    PERFORM set_config('tessera.enable', mode, false);
    FOR warmup IN 1..5 LOOP
        PERFORM pg_temp.reading(format('EXECUTE %I', statement_name));
    END LOOP;
    FOR sample IN 1..repetitions LOOP
        INSERT INTO timings
        VALUES (test_name, mode, sample,
                pg_temp.reading(format('EXECUTE %I', statement_name)));
    END LOOP;
END
$function$;

CREATE FUNCTION pg_temp.measure_pair(test_name text, sql text,
                                     repetitions integer)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    EXECUTE format('PREPARE on_%I AS %s', test_name, sql);
    EXECUTE format('PREPARE off_%I AS %s', test_name, sql);
    -- run.sh measure with CASES: only the cases it matches are timed.
    IF test_name !~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') THEN
        RETURN;
    END IF;
    PERFORM pg_temp.measure(test_name, 'on', 'on_' || test_name, repetitions);
    PERFORM pg_temp.measure(test_name, 'off', 'off_' || test_name, repetitions);
END
$function$;

-- Two inner pieces of common first bytes over the comments: the predicate
-- of TPC-H's query 13.
SELECT pg_temp.measure_pair('like_two_pieces',
    $q$SELECT count(*) FROM bench_like WHERE c NOT LIKE '%special%requests%'$q$, :repetitions);
-- One inner piece over the URLs, rare.
SELECT pg_temp.measure_pair('like_url_google',
    $q$SELECT count(*) FROM bench_like WHERE u LIKE '%google%'$q$, :repetitions);
-- One inner piece found near the start of every URL.
SELECT pg_temp.measure_pair('like_near_start',
    $q$SELECT count(*) FROM bench_like WHERE u LIKE '%://%'$q$, :repetitions);
-- One inner piece of one byte, frequent.
SELECT pg_temp.measure_pair('like_one_byte',
    $q$SELECT count(*) FROM bench_like WHERE c LIKE '%e%'$q$, :repetitions);
-- One inner piece over the short strings: the cost of making a search
-- once a call.
SELECT pg_temp.measure_pair('like_short',
    $q$SELECT count(*) FROM bench_like WHERE x LIKE '%ab%'$q$, :repetitions);
-- Guards that do not search: a prefix, an exact pattern, a suffix, and a
-- pattern with _, whose rows the core's function matches.
SELECT pg_temp.measure_pair('guard_prefix',
    $q$SELECT count(*) FROM bench_like WHERE u LIKE 'https://www.g%'$q$, :repetitions);
SELECT pg_temp.measure_pair('guard_exact',
    $q$SELECT count(*) FROM bench_like WHERE x LIKE 'c4ca4238'$q$, :repetitions);
SELECT pg_temp.measure_pair('guard_suffix',
    $q$SELECT count(*) FROM bench_like WHERE u LIKE '%.html'$q$, :repetitions);
SELECT pg_temp.measure_pair('guard_underscore',
    $q$SELECT count(*) FROM bench_like WHERE c LIKE '%sp_cial%'$q$, :repetitions);

\copy timings TO 'timings.csv' CSV HEADER

\o summary.txt
SELECT test, mode, count(*) AS runs,
       round(min(reading), 3) AS min,
       round(percentile_disc(0.5) WITHIN GROUP (ORDER BY reading), 3) AS median,
       round(percentile_disc(0.1) WITHIN GROUP (ORDER BY reading), 3) AS p10,
       round(percentile_disc(0.9) WITHIN GROUP (ORDER BY reading), 3) AS p90
FROM timings
GROUP BY test, mode
ORDER BY test, mode DESC;
\o

-- The cached plans of both modes.
\o plans.txt
SET tessera.enable = on;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE on_%s', name)
FROM unnest(ARRAY['like_two_pieces', 'like_url_google', 'like_near_start', 'like_one_byte', 'like_short', 'guard_prefix', 'guard_exact', 'guard_suffix', 'guard_underscore']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
SET tessera.enable = off;
SELECT format('EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE off_%s', name)
FROM unnest(ARRAY['like_two_pieces', 'like_url_google', 'like_near_start', 'like_one_byte', 'like_short', 'guard_prefix', 'guard_exact', 'guard_suffix', 'guard_underscore']) AS name
WHERE name ~ coalesce(nullif(current_setting('bench.cases', true), ''), '.') \gexec
\o
DEALLOCATE ALL;
