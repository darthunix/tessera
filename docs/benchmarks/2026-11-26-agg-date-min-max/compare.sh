#!/bin/sh
# compare.sh <side> <base>: per query, the median instructions and cycles of
# <base> and <side> in clickbench-runs.txt, in billions, and the changes; the spread
# of each side's cycles, (max - min) / median.
HERE=$(cd "$(dirname "$0")" && pwd)
col() { awk -F, -v c=$2 'NR>1 && $6=="ok" {print $c}' "$1/times.csv" | sort -n; }
med() { col "$1" $2 | awk '{a[NR]=$1} END {if (NR) printf "%.4f", a[int((NR+1)/2)]/1e9; else print "-"}'; }
spr() { col "$1" 5 | awk '{a[NR]=$1} END {if (NR) printf "%.1f", (a[NR]-a[1])/a[int((NR+1)/2)]*100}'; }
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
printf '%-4s %9s %9s %7s | %8s %8s %7s | %5s %5s\n' q instr0 instr1 change cyc0 cyc1 change spr0 spr1
for q in $(grep "^$1 " "$HERE/clickbench-runs.txt" | awk '{print $2}' | sort -t Q -k2 -n); do
    a=$(grep "^$2 $q " "$HERE/clickbench-runs.txt" | path); b=$(grep "^$1 $q " "$HERE/clickbench-runs.txt" | path)
    [ -n "$a" ] && [ -n "$b" ] || continue
    echo "$q $(med "$a" 4) $(med "$b" 4) $(med "$a" 5) $(med "$b" 5) $(spr "$a") $(spr "$b")" |
        awk '{printf "%-4s %9.3f %9.3f %+6.2f%% | %8.3f %8.3f %+6.2f%% | %4s%% %4s%%\n", $1, $2, $3, ($3/$2-1)*100, $4, $5, ($5/$4-1)*100, $6, $7}'
done
