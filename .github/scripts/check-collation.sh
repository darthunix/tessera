#!/bin/sh
# The check of the rule "No collation library of Tessera's own" of the
# contract of string-bytes: a string's order under a collation is made in
# a backend by the one library the server loaded, so that a node of
# Tessera inside a plan of the core orders rows as the core's nodes do.
#   check-collation.sh [file...]   (in the root, after make)
# 1. No module of Tessera built links ICU (libicu) or another library of
#    collations, as ldd on Linux or otool -L on macOS lists them; given
#    files, those instead, so that CI shows it fails on the server's
#    binary, which links ICU.
# 2. No crate the Rust code of the modules depends on, the kernels' and
#    the Parquet reader's, is a library of collations.
set -eu
status=0

files=
if [ $# -gt 0 ]; then
	files="$*"
else
	for module in bridge/tessera nodes/tessera_nodes kernels/tessera_kernels \
		parquet/tessera_parquet; do
		found=
		for suffix in so dylib; do
			[ -f "$module.$suffix" ] && found=$module.$suffix
		done
		if [ -z "$found" ]; then
			echo "check-collation: $module is not built" >&2
			status=1
		fi
		files="$files $found"
	done
fi

for file in $files; do
	if command -v ldd > /dev/null 2>&1; then
		libraries=$(ldd "$file")
	else
		libraries=$(otool -L "$file")
	fi
	if printf '%s\n' "$libraries" | grep -Eiq 'libicu|libunistring'; then
		echo "check-collation: $file links a library of collations:" >&2
		printf '%s\n' "$libraries" | grep -Ei 'libicu|libunistring' >&2
		status=1
	fi
done

crates=$(cargo tree --quiet --locked -e normal -p tessera-capi -p tessera-parquet \
	--prefix none)
if printf '%s\n' "$crates" |
	grep -Eiq '^(icu|icu_[a-z_]+|rust_icu[a-z_]*|feruca|unicode[-_]collation) v'; then
	echo "check-collation: the Rust code depends on a library of collations:" >&2
	printf '%s\n' "$crates" |
		grep -Ei '^(icu|icu_[a-z_]+|rust_icu[a-z_]*|feruca|unicode[-_]collation) v' |
		sort -u >&2
	status=1
fi

[ "$status" -eq 0 ] && echo "check-collation: no library of collations of Tessera's own"
exit "$status"
