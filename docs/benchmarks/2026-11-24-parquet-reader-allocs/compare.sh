#!/bin/sh
# compare.sh <side> [<runs of the base> <base side>]: the median
# instructions of each query of <side> in runs.txt against the base's,
# by default stage 2's end, `keys` of 2026-11-24-text-keys-by-bytes-keys,
# in billions, and the change.
HERE=$(cd "$(dirname "$0")" && pwd)
base_runs=${2:-$HERE/../2026-11-24-text-keys-by-bytes-keys/runs.txt}
base=${3:-keys}
med() { awk -F, 'NR>1 && $6=="ok" {print $4}' "$1/times.csv" | sort -n | awk '{a[NR]=$1} END {if (NR) print a[int((NR+1)/2)]/1e9}'; }
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
for q in $(grep "^$1 " "$HERE/runs.txt" | awk '{print $2}'); do
    a=$(grep "^$base $q " "$base_runs" | path); b=$(grep "^$1 $q " "$HERE/runs.txt" | path)
    [ -n "$a" ] && [ -n "$b" ] || continue
    echo "$q $(med "$a") $(med "$b")" | awk '{printf "%-4s %9.3f %9.3f %+6.2f %%\n", $1, $2, $3, ($3/$2-1)*100}'
done
