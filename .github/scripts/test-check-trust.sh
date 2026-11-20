#!/bin/sh
# The cases of the part of check-trust.sh for relspill, each over a copy
# of the files it reads, in a repository made in a temporary folder: the
# copy as it is passes, and each fault put into it fails the check with
# its message.
#   test-check-trust.sh
# Run from the root of the repository.
set -eu

script=$(pwd)/.github/scripts/check-trust.sh
root=$(mktemp -d)
trap 'rm -rf "$root" "$root.out" "$root.saved"' EXIT
failures=0

git ls-files -- crates/relhash/src crates/relspill/src openspec/specs/relhash \
    openspec/specs/relspill |
    tar -cf - -T - | tar -xf - -C "$root"
(cd "$root" && git init -q && git add -A)

# The check over the copy, its messages in $root.out.
check() {
    (cd "$root" && sh "$script" > "$root.out" 2>&1)
}

# Run the check with the fault of a Perl expression put into a file, and
# expect it to fail with the message.
fault() {
    name=$1 file=$2 edit=$3 message=$4
    cp "$root/$file" "$root.saved"
    perl -0pi -e "$edit" "$root/$file"
    if cmp -s "$root/$file" "$root.saved"; then
        echo "test-check-trust: $name: the fault changed nothing"
        failures=$((failures + 1))
    elif check; then
        echo "test-check-trust: $name: the check passed"
        failures=$((failures + 1))
    elif ! grep -q "$message" "$root.out"; then
        echo "test-check-trust: $name: no \"$message\" in:"
        cat "$root.out"
        failures=$((failures + 1))
    fi
    cp "$root.saved" "$root/$file"
}

if ! check; then
    echo "test-check-trust: the copy as it is fails:"
    cat "$root.out"
    exit 1
fi

fault "a read without a mark" crates/relspill/src/pack.rs \
    's{(read::u32_at\(packed, 0\) as usize;) // trust: packed-records}{$1}' \
    "a read of the layer without a trust mark"
fault "a mark of no point" crates/relspill/src/set/reader.rs \
    's{// trust: packed-counts}{// trust: packed-count}' \
    "is not in"
fault "a function the script does not know" crates/relspill/src/read.rs \
    's{\z}{\npub(crate) fn other() {}\n}' \
    "is no read check-trust.sh knows"
fault "a word made around the layer" crates/relspill/src/plan.rs \
    's{^(pub const LEAST_RECORDS)}{fn sneak(b: [u8; 8]) -> u64 { u64::from_ne_bytes(b) }\n$1}m' \
    "reads a word of bytes around the layer"
fault "a converter named to a map" crates/relspill/src/plan.rs \
    's{^(pub const LEAST_RECORDS)}{fn sneak(b: &[[u8; 8]]) -> u64 { b.iter().copied().map(u64::from_ne_bytes).sum() }\n$1}m' \
    "reads a word of bytes around the layer"
fault "a page read in place" crates/relspill/src/set/reader.rs \
    's{read::entry\(page, index \+ 1\)\.offset // trust: entry}{page[index + 1].offset}' \
    "reads a page around the layer"
fault "an import of the layer" crates/relspill/src/plan.rs \
    's{^(use crate::error::ensure;)}{$1\nuse crate::read::u32_at;}m' \
    "imports the layer"
fault "a word made past a module of tests" crates/relspill/src/plan.rs \
    's{\z}{\nfn sneak(b: [u8; 8]) -> u64 { u64::from_ne_bytes(b) }\n}' \
    "reads a word of bytes around the layer"
fault "a file of tests that is not marked one" crates/relspill/src/set/tests/damage.rs \
    's{^#!\[cfg\(test\)\]\n}{}m' \
    "reads a word of bytes around the layer"

if [ "$failures" -ne 0 ]; then
    echo "test-check-trust: $failures failed"
    exit 1
fi
echo "test-check-trust: all passed"
