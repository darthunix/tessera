#!/bin/sh
# abturns.sh <base run> <base run b> <end run> <end run b>: per case, the
# median count of mode on in each run, in millions, each side's spread
# (max - min) / min of its two turns, and the change of the medians'
# means.
med() { awk -F, -v t="$2" '$1==t && $2=="on" {print $4}' "$1/timings.csv" | sort -n |
    awk '{a[NR]=$1} END {if (NR) printf "%.3f", a[int((NR+1)/2)]/1e6}'; }
plan() { awk -F, -v t="$2" '$1==t && $2=="on" {print $6}' "$1/timings.csv" | sort -u | tr '\n' ' '; }
for t in $(awk -F, 'NR>1 {print $1}' "$1/timings.csv" | sort -u); do
    echo "$t $(med "$1" $t) $(med "$2" $t) $(med "$3" $t) $(med "$4" $t)" | awk '{
        sb = ($2>$3 ? $2/$3 : $3/$2) - 1; se = ($4>$5 ? $4/$5 : $5/$4) - 1;
        printf "%-30s base %9.3f %9.3f (%4.1f%%)  end %9.3f %9.3f (%4.1f%%)  change %+6.1f%%\n",
            $1, $2, $3, sb*100, $4, $5, se*100, (($4+$5)/($2+$3)-1)*100 }'
done
