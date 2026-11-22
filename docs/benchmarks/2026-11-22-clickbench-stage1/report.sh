#!/bin/sh
# The reports of the protocol's runs: lines a-b of runs.txt side by side.
#   report.sh <first line> <last line>
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HOME/git/tessera-parquet-fdw" || exit 1
export PG_CONFIG=$HOME/git/tessera-duckdb/target/pg19/bin/pg_config
dirs=$(sed -n "$1,$2p" "$HERE/runs.txt" | sed 's|.*-> ||')
# shellcheck disable=SC2086
cargo clickbench report $dirs 2>/dev/null
