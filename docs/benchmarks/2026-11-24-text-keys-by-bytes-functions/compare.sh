#!/bin/sh
# compare.sh <side> [<base side>]: the median instructions of each query
# of <side> in runs.txt against the base's, by default `start`, in
# billions, and the change.
HERE=$(cd "$(dirname "$0")" && pwd)
base=${2:-start}
med() { awk -F, 'NR>1 && $6=="ok" {print $4}' "$1/times.csv" | sort -n | awk '{a[NR]=$1} END {if (NR) print a[int((NR+1)/2)]/1e9}'; }
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
for q in $(grep "^$1 " "$HERE/runs.txt" | awk '{print $2}'); do
    a=$(grep "^$base $q " "$HERE/runs.txt" | path); b=$(grep "^$1 $q " "$HERE/runs.txt" | path)
    [ -n "$a" ] && [ -n "$b" ] || continue
    echo "$q $(med "$a") $(med "$b")" | awk '{printf "%-4s %9.3f %9.3f %+6.2f %%\n", $1, $2, $3, ($3/$2-1)*100}'
done
