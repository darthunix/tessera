#!/bin/sh
# cycles.sh <side> <against>: the median cycles of each query, in
# billions, of <against> and <side>, and their ratio.
HERE=$(cd "$(dirname "$0")" && pwd)
med() { awk -F, -v c=$2 'NR>1 && $6=="ok" {print $c}' "$1/times.csv" | sort -n | awk '{a[NR]=$1} END {if (NR) print a[int((NR+1)/2)]/1e9; else print "-"}'; }
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
for q in $(grep "^$1 " "$HERE/runs.txt" | awk '{print $2}'); do
    a=$(grep "^$2 $q " "$HERE/runs.txt" | path); b=$(grep "^$1 $q " "$HERE/runs.txt" | path)
    [ -n "$a" ] && [ -n "$b" ] || continue
    echo "$q $(med "$a" 5) $(med "$b" 5)" | awk '{if ($2 == "-" || $3 == "-") printf "%-4s %9s %9s\n", $1, $2, $3; else printf "%-4s %9.2f %9.2f %6.2f\n", $1, $2, $3, $3/$2}'
done
