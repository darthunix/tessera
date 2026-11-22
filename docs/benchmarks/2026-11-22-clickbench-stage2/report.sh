#!/bin/sh
# The reports of the runs, by group, from runs.txt.
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HOME/git/tessera-parquet-fdw" || exit 1
dirs() { grep -E -- "$1" "$HERE/runs.txt" | sed 's/.* -> //'; }
timed() { dirs "^run .*--timeout 90 --mode $1 --queries $2 --workers $3 --files [^ ]*/$4 -> "; }
for w in 0 8; do
    echo "== times, the ten files, $w workers"
    cargo clickbench report $(timed on '[0-9]+' $w clickbench-data) \
        $(timed off '[0-9]+' $w clickbench-data) $(timed duckdb '[0-9]+' $w clickbench-data) 2>/dev/null
done
for w in 0 8; do
    echo "== times, hits.parquet, $w workers"
    cargo clickbench report $(timed on '[0-9]+' $w clickbench-one) \
        $(timed duckdb '[0-9]+' $w clickbench-one) 2>/dev/null
done
echo "== times, hits.parquet, 0 workers, the queries Tessera off was timed on"
cargo clickbench report $(timed on '(1|2|3|4|5|7)' 0 clickbench-one) \
    $(timed off '(1|2|3|4|5|7)' 0 clickbench-one) 2>/dev/null
echo "== counts, hits.parquet, 0 workers"
cargo clickbench report $(dirs "^run .*--counters .*--mode on ") $(dirs "^run .*--counters .*--mode duckdb ") 2>/dev/null
