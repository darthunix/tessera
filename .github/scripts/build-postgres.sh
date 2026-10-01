#!/bin/sh
# Build PostgreSQL at a revision and install it into a prefix, for CI:
#   build-postgres.sh <revision> <prefix>
# With assertions, as in development, and with ICU, which the default
# configuration takes.
set -eu
REVISION=$1
PREFIX=$2
SOURCE=$(mktemp -d)
git -C "$SOURCE" init -q
git -C "$SOURCE" fetch -q --depth 1 https://github.com/postgres/postgres.git "$REVISION"
git -C "$SOURCE" checkout -q FETCH_HEAD
cd "$SOURCE"
./configure -q --prefix="$PREFIX" --enable-cassert --enable-debug
make -s -j"$(getconf _NPROCESSORS_ONLN)"
make -s install
