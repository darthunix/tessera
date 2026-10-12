#!/bin/sh
# The check of the rule "The masks of rows are the crate's" of the
# contract of relhash: no crate but relhash defines a mask of rows.
#   check-masks.sh
# A mask of rows is a Rust type whose name ends in Mask or MaskView, in
# the sources of the crates of the product (crates/*/src); tests,
# benchmarks and tools are not the product. The one other is the C API's
# mirror of TessRowMask in tessera-capi, which turns into the crate's type
# at its edge. Run from the root of the repository.
set -eu

found=$(git grep -n -E \
    '^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(struct|enum|union|type)[[:space:]]+[A-Za-z0-9_]*Mask(View)?([^A-Za-z0-9_]|$)' \
    -- 'crates/*/src/*.rs' ':!crates/relhash/' \
    ':!crates/tessera-capi/src/c/mask.rs' || true)
if test -n "$found"; then
    echo "check-masks: no crate but relhash defines a mask of rows:" >&2
    echo "$found" >&2
    exit 1
fi
