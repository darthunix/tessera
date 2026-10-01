#!/bin/sh
# PostgreSQL-level benchmarks: a temporary cluster with the Tessera modules
# preloaded into the postmaster, a data set, and one family of queries
# measured with Tessera on and off, serially or with parallel workers in
# both modes, over the data set at its base size or a multiple of it.
# measure restarts the server first, so that it runs the modules installed
# last: the postmaster holds the ones it loaded, and every backend is
# forked from it. See README.md. Run from the repository root:
#   bench/pg/run.sh setup [scale] | measure <family> [workers] | stop
# measure takes CASES, a regular expression of the case names to time
# (every case by default; the plans are written for all of them), and
# REPETITIONS, the runs of each case in each mode (the family's own, 31,
# by default): a development A/B times the cases a change touches, fewer
# times, to stay within minutes.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/bench/pg
PG_CONFIG=${PG_CONFIG:-pg_config}
BIN=$("$PG_CONFIG" --bindir)
LIB=$("$PG_CONFIG" --pkglibdir)
# The suffix of loadable modules and a SHA-256 tool, by platform.
case $(uname) in
Darwin) DLSUFFIX=.dylib ;;
*) DLSUFFIX=.so ;;
esac
if command -v sha256sum > /dev/null; then SHA256=sha256sum; else SHA256="shasum -a 256"; fi
RUNS=$ROOT/target/bench-runs
DATA=$RUNS/pgdata-bench
export PGPORT=${PGPORT:-5433} PGHOST=/tmp PGDATABASE=postgres

case "$1" in
setup)
    SCALE=${2:-1}
    mkdir -p "$RUNS"
    rm -rf "$DATA"
    "$BIN/initdb" -D "$DATA" -A trust > "$RUNS/initdb.log" 2>&1
    cat >> "$DATA/postgresql.conf" <<CONF
shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'
shared_buffers = ${SHARED_BUFFERS:-2GB}
jit = off
max_parallel_workers_per_gather = 0
autovacuum = off
track_io_timing = off
CONF
    "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -o "-p $PGPORT -k /tmp" -w start
    "$BIN/psql" -X -c "CREATE EXTENSION IF NOT EXISTS tessera"
    "$BIN/psql" -X -v scale="$SCALE" -f "$HERE/setup.sql"
    ;;
measure)
    FAMILY=$2
    WORKERS=${3:-0}
    ID=$(LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 6)
    if [ "$WORKERS" -gt 0 ]; then
        OUT=$RUNS/pg-$FAMILY-w$WORKERS-$ID
    else
        OUT=$RUNS/pg-$FAMILY-$ID
    fi
    "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -m fast -w restart > /dev/null
    mkdir -p "$OUT/source"
    cp "$HERE/README.md" "$OUT/protocol.md"
    cp "$HERE/setup.sql" "$HERE/$FAMILY.sql" "$OUT/source/"
    {
        echo "HEAD $(git -C "$ROOT" rev-parse HEAD)"
        echo "workers $WORKERS"
        echo "cases ${CASES:-all}"
        echo "repetitions ${REPETITIONS:-default}"
        echo "scale $("$BIN/psql" -X -tAc 'SELECT scale FROM bench_scale')"
        echo "shared_buffers $("$BIN/psql" -X -tAc 'SHOW shared_buffers')"
        echo "status:"; git -C "$ROOT" status --short
        echo "sha256:"
        $SHA256 "$LIB/tessera$DLSUFFIX" "$LIB/tessera_nodes$DLSUFFIX" \
            "$LIB/tessera_kernels$DLSUFFIX" \
            "$LIB/libtessera_runtime.a" "$BIN/postgres"
    } > "$OUT/source.txt"
    { pmset -g batt 2>/dev/null | head -2; date; } > "$OUT/power.txt"
    (cd "$OUT" && PGOPTIONS="-c bench.cases=${CASES:-}" "$BIN/psql" -X \
        -v workers="$WORKERS" ${REPETITIONS:+-v repetitions="$REPETITIONS"} \
        -f "$HERE/$FAMILY.sql" > run.log 2>&1)
    cat "$OUT/summary.txt"
    echo "results: $OUT"
    ;;
stop)
    "$BIN/pg_ctl" -D "$DATA" -m fast stop
    rm -rf "$DATA"
    ;;
*)
    echo "usage: $0 setup [scale] | measure <family> [workers] | stop" >&2
    exit 2
    ;;
esac
