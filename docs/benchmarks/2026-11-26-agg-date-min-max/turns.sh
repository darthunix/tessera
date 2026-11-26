#!/bin/sh
# turns.sh: per query, the median cycles of each turn of the base and of
# the end in clickbench-runs.txt, in billions, and each side's spread as the
# design's plan defines it: (max - min) / min of its turns.
HERE=$(cd "$(dirname "$0")" && pwd)
med() { awk -F, 'NR>1 && $6=="ok" {print $5}' "$1/times.csv" | sort -n |
    awk '{a[NR]=$1} END {if (NR) printf "%.3f", a[int((NR+1)/2)]/1e9}'; }
side() {
    grep -E "^$1(-[a-z])? $2 " "$HERE/clickbench-runs.txt" | while read -r tag q arrow run; do
        med "$(echo "$run" | sed "s|^~|$HOME|")"; echo
    done
}
spread() { awk 'NR==1 || $1<lo {lo=$1} NR==1 || $1>hi {hi=$1} END {printf "%.1f%%", (hi-lo)/lo*100}'; }
for q in Q1 Q3 Q7; do
    b=$(side base $q); e=$(side end $q)
    printf '%-4s base %s (%s)  end %s (%s)\n' $q "$(echo $b)" "$(echo "$b" | tr ' ' '\n' | spread)" \
        "$(echo $e)" "$(echo "$e" | tr ' ' '\n' | spread)"
done
