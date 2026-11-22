#!/bin/sh
# runs.sh resumed (runs3.sh: no wait where Kani did not run; runs4.sh:
# no Tessera off over hits.parquet): the runs already in runs.txt are skipped, and each
# timed or counted run waits until no other session's Kani (cbmc) has
# run for a minute; the waits go to waits.txt.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HOME/git/tessera-parquet-fdw" || exit 1
export PG_CONFIG=$HOME/git/tessera-duckdb/target/pg19/bin/pg_config
DATA=$HOME/git/tessera-duckdb/target
QUERIES="1 2 3 4 5 7 8 9 10 16 20 30 33 36 41 42 43"
busy() { pgrep -x cbmc > /dev/null || pgrep -f cargo-kani > /dev/null; }
idle() {
    waited=0
    # A minute without Kani after it ran, since it runs harness after
    # harness; no wait where it did not run.
    while busy; do
        while busy; do sleep 20; waited=$((waited + 20)); done
        sleep 60
        waited=$((waited + 60))
    done
    [ $waited -gt 0 ] && echo "$(date '+%H:%M:%S') waited $waited s before: $*" >> "$HERE/waits.txt"
    return 0
}
one() {
    if grep -qF -- "$* -> " "$HERE/runs.txt" 2>/dev/null; then return; fi
    out=$(cargo clickbench "$@" 2>&1)
    dir=$(printf '%s\n' "$out" | sed -n 's/^results: //p')
    echo "$* -> ${dir:-$(printf '%s\n' "$out" | tail -1)}" >> "$HERE/runs.txt"
    printf '%s\n' "$out" >> "$HERE/runs.log"
}
for files in clickbench-data clickbench-one; do
    for w in 0 8; do
        for q in $QUERIES; do
            one check --no-install --keep-running --queries $q --workers $w --files "$DATA/$files"
        done
    done
done
grep -q "^done check" "$HERE/runs.txt" || echo "done check" >> "$HERE/runs.txt"
for files in clickbench-data clickbench-one; do
    # The maintainer's word: over hits.parquet, Tessera and DuckDB alone.
    case $files in
        clickbench-data) modes="off on duckdb" ;;
        *) modes="on duckdb" ;;
    esac
    for mode in $modes; do
        for w in 0 8; do
            for q in $QUERIES; do
                set -- run --no-install --keep-running --timeout 90 --mode $mode \
                    --queries $q --workers $w --files "$DATA/$files"
                grep -qF -- "$* -> " "$HERE/runs.txt" 2>/dev/null || idle "$*"
                one "$@"
            done
        done
    done
done
echo "done times" >> "$HERE/runs.txt"
for mode in on duckdb; do
    for q in $QUERIES; do
        set -- run --no-install --keep-running --counters --tries 5 --mode $mode \
            --queries $q --workers 0 --files "$DATA/clickbench-one"
        grep -qF -- "$* -> " "$HERE/runs.txt" 2>/dev/null || idle "$*"
        one "$@"
    done
done
echo "done counts" >> "$HERE/runs.txt"
cargo clickbench stop >> "$HERE/runs.log" 2>&1
echo "stopped" >> "$HERE/runs.txt"
