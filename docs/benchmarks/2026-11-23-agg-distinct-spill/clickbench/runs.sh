#!/bin/sh
# The ClickBench runs of agg-distinct-spill's measurement plan, item 3:
# Q9 and Q10 counted (instructions and cycles of pg_pmu) with Tessera
# on, off and pg_duckdb, at 0 and 8 workers, over the ten files and over
# hits.parquet, on a local merge of parquet-fdw with this branch that is
# never pushed. Each run is one query in one mode, five minutes at most
# by the tool's deadline; the answers are checked first. Runs already
# in runs.txt are skipped, so the script resumes.
set -u
OUT=$1
cd "$HOME/git/tessera-ads-clickbench" || exit 1
export PG_CONFIG=$HOME/git/tessera-duckdb/target/pg19/bin/pg_config
# PATH began with a wrapper of make that passes PG_SYSROOT, the SDK
# that this PostgreSQL's Makefile.global names being gone.
DATA=$HOME/git/tessera-duckdb/target
PORT=55493
mkdir -p "$OUT"
one() {
    if grep -qF -- "$* -> " "$OUT/runs.txt" 2>/dev/null; then return; fi
    out=$(cargo clickbench "$@" 2>&1)
    dir=$(printf '%s\n' "$out" | sed -n 's/^results: //p')
    echo "$* -> ${dir:-$(printf '%s\n' "$out" | tail -1)}" >> "$OUT/runs.txt"
    printf '%s\n' "$out" >> "$OUT/runs.log"
}
one setup --keep-running --port $PORT --files "$DATA/clickbench-data"
for files in clickbench-data clickbench-one; do
    for w in 0 8; do
        one check --no-install --keep-running --port $PORT --queries 9,10 --workers $w \
            --files "$DATA/$files"
    done
done
for files in clickbench-data clickbench-one; do
    for mode in on off duckdb; do
        for w in 0 8; do
            for q in 9 10; do
                one run --no-install --keep-running --port $PORT --counters --tries 5 \
                    --mode $mode --queries $q --workers $w --files "$DATA/$files"
            done
        done
    done
done
one stop --port $PORT
echo "done" >> "$OUT/runs.txt"
