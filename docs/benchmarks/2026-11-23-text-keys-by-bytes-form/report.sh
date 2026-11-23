#!/bin/sh
# A count of runs.txt against the base, side by side, by ClickBench's
# report: report.sh <head|lazy|inline|end>, the base's runs of the same
# queries first.
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HOME/git/tessera-text-keys-by-bytes" || exit 1
path() { sed "s|.*-> ||; s|^~|$HOME|"; }
dirs=""
for query in $(grep "^$1 " "$HERE/runs.txt" | awk '{print $2}'); do
    dirs="$dirs $(grep "^base $query " "$HERE/runs.txt" | path)"
done
dirs="$dirs $(grep "^$1 " "$HERE/runs.txt" | path)"
# shellcheck disable=SC2086
cargo clickbench report $dirs 2>/dev/null
