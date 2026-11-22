#!/bin/sh
# The runs of the protocol, in its order, each one command of
# cargo clickbench; the directory of each run goes to runs.txt.
#   runs.sh times|counts
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HOME/git/tessera-parquet-fdw" || exit 1
export PG_CONFIG=$HOME/git/tessera-duckdb/target/pg19/bin/pg_config
DATA=$HOME/git/tessera-duckdb/target
one() {
    out=$(cargo clickbench run --no-install "$@" 2>&1)
    dir=$(printf '%s\n' "$out" | sed -n 's/^results: //p')
    echo "$* -> ${dir:-FAILED}" >> "$HERE/runs.txt"
    printf '%s\n' "$out" >> "$HERE/runs.log"
}
for files in clickbench-data clickbench-one; do
    for w in 0 2 4 8; do
        if [ "$1" = times ]; then
            for mode in off on duckdb; do
                one --mode $mode --queries 1-7 --workers $w --files "$DATA/$files"
            done
        elif [ "$w" = 0 ] || [ "$w" = 8 ]; then
            for mode in on duckdb; do
                one --counters --tries 5 --mode $mode --queries 1-7 --workers $w \
                    --files "$DATA/$files"
            done
        fi
    done
done
echo "done $1" >> "$HERE/runs.txt"
