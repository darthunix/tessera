#!/bin/sh
# The cases of check-contracts.sh, each over a repository of its own
# made in a temporary folder: a base with one contract, then a change
# of it, and whether the check passes and what it says.
#   test-check-contracts.sh
set -eu

script=$(cd "$(dirname "$0")" && pwd)/check-contracts.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
failures=0

# A contract of part "box" with a rule checked in CI and one by review.
contract() {
    cat <<'EOF'
# box: the architecture contract

## Rules

### Alone
The box needs nothing else, so that it can be used alone.
- **Checked by:** `tests/alone.sh::alone check`

### Small
The box stays small, so that it can be read.
- **Checked by:** agent review — count the lines of the box and report
  them
EOF
}

# A repository at its base, on a branch "change" from "main": the
# contract with $1 appended, and the test it names.
base() {
    dir=$root/$case_number
    mkdir -p "$dir/openspec/specs/box" "$dir/tests"
    cd "$dir"
    git init -q -b main
    git config user.email test@example.org
    git config user.name test
    git config commit.gpgsign false
    { contract; printf '%s' "${1:-}"; } > openspec/specs/box/contract.md
    echo 'echo alone check' > tests/alone.sh
    git add -A
    git commit -q -m base
    git checkout -q -b change
}

commit() {
    git add -A
    git commit -q -m change
}

# Replace text in the contract.
swap() {
    sed -i.bak "$1" openspec/specs/box/contract.md
    rm openspec/specs/box/contract.md.bak
}

code() {
    echo code > code.txt
}

# Run the check against main: the case passes when the exit code and,
# for a failure, a word of the message are as expected.
expect() {
    want=$1
    word=${2:-}
    if sh "$script" main > out 2>&1; then got=0; else got=1; fi
    if test "$got" != "$want" ||
        { test -n "$word" && ! grep -q -F -e "$word" out; }; then
        echo "FAIL $case: exit $got, wanted $want ${word:+and \"$word\"}"
        cat out
        failures=$((failures + 1))
    else
        echo "ok   $case"
    fi
}

case_number=0
next() {
    case=$1
    case_number=$((case_number + 1))
}

next "a change of code keeps the contract"
base
code
commit
expect 0 "2 rules"

next "a check names a text its file lacks"
base
swap 's/alone check/no such check/'
commit
expect 1 "is not in tests/alone.sh"

next "a check names a file that is not there"
base
swap 's|tests/alone.sh|tests/gone.sh|'
commit
expect 1 "no file tests/gone.sh"

next "a check names no test"
base
swap 's|`tests/alone.sh::alone check`|look at it|'
commit
expect 1 "names no check"

next "a rule with two checks"
base
printf '\n### Fast\nThe box is fast.\n- **Checked by:** pending\n- **Checked by:** pending\n' \
    >> openspec/specs/box/contract.md
commit
expect 1 "more than one"

next "a rule without a check"
base
printf '\n### Fast\nThe box is fast.\n' >> openspec/specs/box/contract.md
commit
expect 1 "has no \"Checked by\" bullet"

next "a probe, the maintainer and a check over two lines"
base
printf '\n### Fast\nThe box is fast.\n- **Checked by:** probe — make it slow\n\n### Kept\nThe box is kept.\n- **Checked by:** maintainer — is it kept?\n\n### Split\nThe box is split.\n- **Checked by:** `tests/alone.sh::alone\n  check`\n' \
    >> openspec/specs/box/contract.md
commit
expect 0 "5 rules"

next "pending in a part's contract"
base
printf '\n### Fast\nThe box is fast.\n- **Checked by:** pending\n' \
    >> openspec/specs/box/contract.md
commit
expect 1 "still pending"

next "pending in the contract of an open change"
base
mkdir -p openspec/changes/new
printf '# cube: the architecture contract\n\n## Rules\n\n### Round\nThe cube is round.\n- **Checked by:** pending\n' \
    > openspec/changes/new/contract.md
commit
expect 0 "1 pending"

next "a contract that names no part"
base
swap '1s/.*/# The box/'
commit
expect 1 "names no part"

next "two contracts name one part"
base
mkdir -p openspec/changes/new
contract > openspec/changes/new/contract.md
commit
expect 1 "two contracts name the part box"

next "a contract with CR at the ends of its lines"
base
perl -pi -e 's/$/\r/' openspec/specs/box/contract.md
code
commit
expect 0 "2 rules"

next "a heading inside a code block is text"
base '
### Drawn
The box is drawn so:
```
# not a heading
```
- **Checked by:** maintainer — is it drawn?
'
code
commit
expect 0 "3 rules"

next "a rule with a backslash in its name, kept"
base '
### Back\slash
The box keeps a backslash.
- **Checked by:** maintainer — is it kept?
'
code
commit
expect 0 "3 rules"

next "a rule is dropped beside a change of code"
base
awk '/^### Small/ { exit } { print }' openspec/specs/box/contract.md > c
mv c openspec/specs/box/contract.md
code
commit
expect 1 "is dropped"

next "the contract is gone"
base
git rm -q openspec/specs/box/contract.md
code
commit
expect 1 "is gone"

next "the words of a rule are loosened"
base
swap 's/needs nothing else/may need anything/'
code
commit
expect 1 "the words of the rule \"Alone\" of box changed"

next "a rule is only rewrapped"
base
swap 's/The box needs nothing else, so that it can be used alone./The box needs nothing\
else, so that it can be used alone./'
code
commit
expect 0 "2 rules"

next "a check in CI is replaced by a review beside a change of code"
base
swap 's/`tests\/alone.sh::alone check`/agent review — look/'
code
commit
expect 1 "checked less than before"

next "a check in CI becomes a review that quotes it"
base
swap 's/`tests\/alone.sh::alone check`/agent review — glance at `tests\/alone.sh::alone check`/'
code
commit
expect 1 "checked less than before"

next "a review becomes pending"
base
mkdir -p openspec/changes/new
git mv openspec/specs/box/contract.md openspec/changes/new/contract.md
git commit -q -m "into a change"
git checkout -q main
git merge -q --ff-only change
git checkout -q -b later
sed -i.bak 's/agent review — count the lines of the box and report/pending/; /^  them$/d' \
    openspec/changes/new/contract.md
rm openspec/changes/new/contract.md.bak
code
commit
expect 1 "checked less than before"

next "the procedure of a review is loosened"
base
swap 's/count the lines of the box and report/look at the box/'
code
commit
expect 1 "the check of the rule \"Small\" of box changed"

next "pending becomes a review, and a review a check in CI"
base '
### Fast
The box is fast.
- **Checked by:** maintainer — is it fast?
'
mkdir -p openspec/changes/new
printf '# cube: the architecture contract\n\n## Rules\n\n### Round\nThe cube is round.\n- **Checked by:** pending\n' \
    > openspec/changes/new/contract.md
git add -A
git commit -q -m "a change"
git checkout -q main
git merge -q --ff-only change
git checkout -q -b later
sed -i.bak 's/- \*\*Checked by:\*\* pending/- **Checked by:** agent review — measure it/' \
    openspec/changes/new/contract.md
rm openspec/changes/new/contract.md.bak
swap 's/maintainer — is it fast?/`tests\/alone.sh::alone check`/'
code
commit
expect 0 "4 rules"

next "a check in CI is replaced, in a pull request of contracts alone"
base
swap 's/`tests\/alone.sh::alone check`/agent review — look/'
commit
expect 0 "the maintainer decides"

next "a pull request of contracts alone that deletes the test"
base
swap 's/`tests\/alone.sh::alone check`/agent review — look/'
git rm -q tests/alone.sh
commit
expect 1 "beside other changes"

next "a check in CI swapped for another beside code"
base
swap 's/`tests\/alone.sh::alone check`/`code.txt::code`/'
code
commit
expect 1 "no longer checked by \`tests/alone.sh::alone check\`"

next "a check in CI gains an item beside code"
base
swap 's/`tests\/alone.sh::alone check`/`tests\/alone.sh::alone check` and `code.txt::code`/'
code
commit
expect 0 "2 rules"

next "a contract with no rule"
base
printf '# box: the architecture contract\n\n## Rules\n\nNothing.\n' \
    > openspec/specs/box/contract.md
commit
expect 1 "holds no rule"

next "two rules of one contract share a name"
base
printf '\n### Alone\nThe box is alone.\n- **Checked by:** maintainer — is it?\n' \
    >> openspec/specs/box/contract.md
commit
expect 1 "two rules named \"Alone\""

next "two parts share a rule name"
base
mkdir -p openspec/specs/cube
printf '# cube: the architecture contract\n\n## Rules\n\n### Alone\nThe cube is alone.\n- **Checked by:** maintainer — is it?\n' \
    > openspec/specs/cube/contract.md
git add -A
git commit -q -m "a cube"
git checkout -q main
git merge -q --ff-only change
git checkout -q -b later
sed -i.bak 's/The cube is alone./The cube may be anything./' \
    openspec/specs/cube/contract.md
rm openspec/specs/cube/contract.md.bak
code
commit
expect 1 "the words of the rule \"Alone\" of cube changed"

next "two parts share a rule name, and code changes"
base
mkdir -p openspec/specs/cube
printf '# cube: the architecture contract\n\n## Rules\n\n### Alone\nThe cube is alone.\n- **Checked by:** maintainer — is it?\n' \
    > openspec/specs/cube/contract.md
git add -A
git commit -q -m "a cube"
git checkout -q main
git merge -q --ff-only change
git checkout -q -b later
code
commit
expect 0 "3 rules"

next "a contract and a design loosen a rule"
base
swap 's/needs nothing else/may need anything/'
echo '# box: how it is built' > openspec/specs/box/design.md
commit
expect 0 "the maintainer decides"

next "the base moves on after the branch began"
base
code
commit
git checkout -q main
printf '\n### Fast\nThe box is fast.\n- **Checked by:** maintainer — is it fast?\n' \
    >> openspec/specs/box/contract.md
git commit -q -a -m "a rule on main"
git checkout -q change
expect 0 "2 rules"

next "the contract moves from its change to its part"
base
mkdir -p openspec/changes/new
git mv openspec/specs/box/contract.md openspec/changes/new/contract.md
git commit -q -m "into the change"
git checkout -q main
git merge -q --ff-only change
git checkout -q -b close
mkdir -p openspec/specs/box
git mv openspec/changes/new/contract.md openspec/specs/box/contract.md
code
commit
expect 0 "2 rules"

if test "$failures" -gt 0; then
    echo "test-check-contracts: $failures failed"
    exit 1
fi
echo "test-check-contracts: all passed"
