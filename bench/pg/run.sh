#!/bin/sh
# PostgreSQL-level benchmarks: a temporary cluster with the Tessera modules
# preloaded, a data set, and one family of queries measured with Tessera
# on and off, serially or with parallel workers in both modes. See
# README.md. Run from the repository root:
#   bench/pg/run.sh setup | measure <family> [workers] | stop
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/bench/pg
PG_CONFIG=${PG_CONFIG:-pg_config}
BIN=$("$PG_CONFIG" --bindir)
LIB=$("$PG_CONFIG" --pkglibdir)
RUNS=$ROOT/target/bench-runs
DATA=$RUNS/pgdata-bench
export PGPORT=${PGPORT:-5433} PGHOST=/tmp PGDATABASE=postgres

case "$1" in
setup)
    mkdir -p "$RUNS"
    rm -rf "$DATA"
    "$BIN/initdb" -D "$DATA" -A trust > "$RUNS/initdb.log" 2>&1
    cat >> "$DATA/postgresql.conf" <<CONF
session_preload_libraries = 'tessera, tessera_nodes, tessera_kernels, tessera_limit'
shared_buffers = 2GB
jit = off
max_parallel_workers_per_gather = 0
autovacuum = off
track_io_timing = off
CONF
    "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -o "-p $PGPORT -k /tmp" -w start
    "$BIN/psql" -X -c "CREATE EXTENSION IF NOT EXISTS tessera"
    "$BIN/psql" -X -f "$HERE/setup.sql"
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
    mkdir -p "$OUT/source"
    cp "$HERE/README.md" "$OUT/protocol.md"
    cp "$HERE/setup.sql" "$HERE/$FAMILY.sql" "$OUT/source/"
    {
        echo "HEAD $(git -C "$ROOT" rev-parse HEAD)"
        echo "workers $WORKERS"
        echo "status:"; git -C "$ROOT" status --short
        echo "sha256:"
        shasum -a 256 "$LIB/tessera.dylib" "$LIB/tessera_nodes.dylib" \
            "$LIB/tessera_kernels.dylib" "$LIB/tessera_limit.dylib" \
            "$LIB/libtessera_runtime.a" "$BIN/postgres"
    } > "$OUT/source.txt"
    { pmset -g batt 2>/dev/null | head -2; date; } > "$OUT/power.txt"
    (cd "$OUT" && "$BIN/psql" -X -v workers="$WORKERS" -f "$HERE/$FAMILY.sql" \
        > run.log 2>&1)
    cat "$OUT/summary.txt"
    echo "results: $OUT"
    ;;
stop)
    "$BIN/pg_ctl" -D "$DATA" -m fast stop
    rm -rf "$DATA"
    ;;
*)
    echo "usage: $0 setup | measure <family> [workers] | stop" >&2
    exit 2
    ;;
esac
