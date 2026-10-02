#!/bin/sh
# Build PostgreSQL at a revision and install it into a prefix, for CI:
#   build-postgres.sh <revision> <prefix> [asan]
# With assertions, as in development, and with ICU, which the default
# configuration takes. With asan, also with AddressSanitizer and
# UndefinedBehaviorSanitizer, which then stop at the first error; the
# extensions built against it take the flags from its pg_config.
set -eu
REVISION=$1
PREFIX=$2
FLAVOUR=${3:-assert}
SOURCE=$(mktemp -d)
git -C "$SOURCE" init -q
git -C "$SOURCE" fetch -q --depth 1 https://github.com/postgres/postgres.git "$REVISION"
git -C "$SOURCE" checkout -q FETCH_HEAD
cd "$SOURCE"
case "$FLAVOUR" in
assert)
	./configure -q --prefix="$PREFIX" --enable-cassert --enable-debug
	;;
asan)
	./configure -q --prefix="$PREFIX" --enable-cassert --enable-debug \
		CFLAGS="-O1 -g -fno-omit-frame-pointer -fsanitize=address,undefined -fno-sanitize-recover=all" \
		LDFLAGS="-fsanitize=address,undefined"
	;;
*)
	echo "unknown flavour $FLAVOUR" >&2
	exit 1
	;;
esac
make -s -j"$(getconf _NPROCESSORS_ONLN)"
make -s install
