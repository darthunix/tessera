#!/bin/sh
# duckdb.sh <side>: per query, the median cycles of pg_duckdb's run and of
# <side> in runs.txt, in billions, and their ratio.
HERE=$(cd "$(dirname "$0")" && pwd)
med() { awk -F, 'NR>1 && $6=="ok" {print $5}' "$1/times.csv" | sort -n | awk '{a[NR]=$1} END {if (NR) printf "%.3f", a[int((NR+1)/2)]/1e9}'; }
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
for q in $(grep "^$1 " "$HERE/runs.txt" | awk '{print $2}' | sort -t Q -k2 -n); do
    a=$(grep "^duckdb $q " "$HERE/runs.txt" | path); b=$(grep "^$1 $q " "$HERE/runs.txt" | path)
    [ -n "$a" ] && [ -n "$b" ] || continue
    echo "$q $(med "$a") $(med "$b")" | awk '{printf "%-4s %8.3f %8.3f %5.2f\n", $1, $2, $3, $3/$2}'
done
