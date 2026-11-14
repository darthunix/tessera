#!/bin/sh
# PostgreSQL-level benchmarks: a temporary cluster with the Tessera modules
# preloaded into the postmaster, a data set, and one family of queries
# measured with Tessera on and off, serially or with parallel workers in
# both modes, over the data set at its base size or a multiple of it.
# measure restarts the server first, so that it runs the modules installed
# last: the postmaster holds the ones it loaded, and every backend is
# forked from it. See README.md. Run from the repository root:
#   bench/pg/run.sh setup [scale] | measure <family> [workers] | stop
# measure keeps the PGOPTIONS it is given, such as a statement_timeout
# that bounds every statement of a run, and takes CASES, a regular
# expression of the case names to time
# (every case by default; the plans are written for those timed), and
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
# Every start names the port the queries go to: a restart without it
# takes the options of the last start (postmaster.opts), whatever port
# they named, and the queries would reach another cluster on PGPORT.
SERVER_OPTIONS="-p $PGPORT -k $PGHOST"

# Stops unless the server on PGPORT runs this cluster.
check_server() {
    reached=$("$BIN/psql" -X -tAc 'SHOW data_directory') || exit 1
    if [ "$(cd "$reached" 2>/dev/null && pwd -P)" != "$(cd "$DATA" && pwd -P)" ]; then
        echo "the server on port $PGPORT runs $reached, not $DATA" >&2
        exit 1
    fi
}

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
    "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -o "$SERVER_OPTIONS" -w start
    check_server
    "$BIN/psql" -X -c "CREATE EXTENSION IF NOT EXISTS tessera"
    "$BIN/psql" -X -v scale="$SCALE" -f "$HERE/setup.sql"
    ;;
restart)
    # A bound of its own, whatever PGCTLTIMEOUT says: 30 seconds a wait.
    "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -o "$SERVER_OPTIONS" -m fast -w -t 30 \
        restart > /dev/null
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
    # A caller that restarted the server itself, outside its deadline, says
    # so; a deadline must not stop a restart halfway, the server stopped.
    if [ -z "${BENCH_RESTARTED:-}" ]; then
        "$BIN/pg_ctl" -D "$DATA" -l "$RUNS/server.log" -o "$SERVER_OPTIONS" -m fast -w \
            restart > /dev/null
    fi
    check_server
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
        echo "status:"; git -C "$ROOT" status --short --untracked-files=no
        echo "sha256:"
        $SHA256 "$LIB/tessera$DLSUFFIX" "$LIB/tessera_nodes$DLSUFFIX" \
            "$LIB/tessera_kernels$DLSUFFIX" \
            "$LIB/libtessera_runtime.a" "$BIN/postgres"
    } > "$OUT/source.txt"
    { pmset -g batt 2>/dev/null | head -2; date; } > "$OUT/power.txt"
    # A query whose psql is gone, stopped by a deadline, ends within a
    # second instead of running on.
    (cd "$OUT" && PGOPTIONS="${PGOPTIONS:-} -c bench.cases=${CASES:-} \
        -c client_connection_check_interval=1000" "$BIN/psql" -X \
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
    echo "usage: $0 setup [scale] | restart | measure <family> [workers] | stop" >&2
    exit 2
    ;;
esac
