#!/bin/sh
# Check the architecture contracts (CONTRIBUTING.md, "Architecture
# contracts"):
#   check-contracts.sh [BASE]
# A contract is openspec/specs/<part>/contract.md, or the contract.md of
# an open change that makes a part. Its first line names the part,
# "# <part>: ...", and each rule is a "### " heading under "## Rules":
# its words, then one bullet "- **Checked by:**", which may continue on
# the next lines, indented.
# 1. Each contract names its part, no other contract names the same
#    part, and it holds a rule; no two of its rules share a name. Each rule is checked by `path::text` of
#    a file of the repository that holds the text, as a "Verified by"
#    names a test; by "probe — ...", "agent review — ..." or
#    "maintainer — ..."; or it is "pending", only in an open change.
# 2. With BASE, a git revision, the contracts of the merge base of BASE
#    and HEAD bind HEAD. A rule keeps its words, and its check keeps its
#    kind and its words, or grows: pending becomes any check, a probe,
#    a review or the maintainer becomes a check in CI, and a check in
#    CI keeps its items or gains items. Anything else loosens the rule, as does a rule or a
#    contract that is gone. A pull request that changes nothing but
#    contracts and designs may loosen a rule: it is how a rule is
#    changed, and the maintainer decides by merging it.
# A line that starts with "#" inside a fenced code block is text, not a
# heading. Lines may end in CR.
# Run from the root of the repository.
set -eu

fail=0
error() {
    echo "check-contracts: $*" >&2
    fail=1
}

# A notice the pull request must show: a warning on GitHub.
notice() {
    if test "${GITHUB_ACTIONS:-}" = true; then
        echo "::warning::check-contracts: $*"
    else
        echo "check-contracts: $*" >&2
    fi
}

tracked() {
    test -n "$(git ls-files -- "$1" | head -n 1)"
}

# The contracts of the repository among the paths on stdin.
contracts() {
    grep -E '^openspec/(specs|changes)/[^/]+/contract\.md$' |
        grep -v '^openspec/changes/archive/' || true
}

# One line a rule of the contract on stdin: part, line, rule, the text
# after "Checked by:" (MISSING without the bullet, MULTIPLE with more),
# the rule's words; runs of blanks and tabs are one space. A contract
# that names no part gives one line with the part empty and the rule
# "NO PART"; one that holds no rule, one with the rule "NO RULE".
rules() {
    awk '
        function squeeze(text) {
            gsub(/[ \t]+/, " ", text)
            sub(/^ /, "", text)
            sub(/ $/, "", text)
            return text
        }
        function flush() {
            if (name != "") {
                if (count == 0)
                    value = "MISSING"
                else if (count > 1)
                    value = "MULTIPLE"
                printf "%s\037%d\037%s\037%s\037%s\n", part, line, name,
                    squeeze(value), squeeze(words)
                found++
            }
            name = ""
        }
        { sub(/\r$/, "") }
        NR == 1 {
            if ($0 ~ /^# [^:]+:/) {
                part = $0
                sub(/^# /, "", part)
                sub(/:.*/, "", part)
                gsub(/\t/, " ", part)
            }
            next
        }
        /^[ \t]*(```|~~~)/ { fence = !fence }
        fence || /^[ \t]*(```|~~~)/ {
            if (name != "") { words = words " " $0; taking = 0 }
            next
        }
        /^## / {
            flush()
            inside = ($0 ~ /^## Rules[ \t]*$/)
            taking = 0
            next
        }
        inside && /^### / {
            flush()
            name = $0
            sub(/^### [ \t]*/, "", name)
            gsub(/\t/, " ", name)
            line = NR
            count = 0
            words = ""
            taking = 0
            next
        }
        /^#/ { flush(); taking = 0; next }
        name != "" && /^- \*\*Checked by:\*\*/ {
            value = $0
            sub(/^- \*\*Checked by:\*\*[ \t]*/, "", value)
            count++
            taking = 1
            next
        }
        name != "" && taking && /^[ \t]+[^ \t]/ {
            value = value " " $0
            next
        }
        name != "" { taking = 0; words = words " " $0; next }
        { taking = 0 }
        END {
            flush()
            if (part == "")
                printf "\0370\037NO PART\037\037\n"
            else if (!found)
                printf "%s\0370\037NO RULE\037\037\n", part
        }
    '
}

# The rules of the contracts at the paths on stdin, each read by the
# command given with the path last: path, part, line, rule, value,
# words.
rules_of() {
    while read -r path; do
        "$@" "$path" | rules | sed "s|^|$path$us|"
    done
}

# A file as it is in a revision.
show_at() {
    git show "$1:$2"
}

# The rank of a check: 3 in CI, 2 a probe, a review or the maintainer,
# 1 pending, 0 none.
rank() {
    case "$1" in
    pending) echo 1 ;;
    MISSING | MULTIPLE) echo 0 ;;
    "probe — "?* | "agent review — "?* | "maintainer — "?*) echo 2 ;;
    *) echo 3 ;;
    esac
}

# The `path::text` items of a check, one a line.
items() {
    printf '%s\n' "$1" | grep -o '`[^`]*::[^`]*`' || true
}

us=$(printf '\037')
head_list=$(mktemp)
base_list=$(mktemp)
trap 'rm -f "$head_list" "$base_list"' EXIT

git ls-files -- openspec/specs openspec/changes | contracts |
    rules_of cat > "$head_list"

total=0
pending=0
while IFS="$us" read -r file part line name value words; do
    case "$name" in
    "NO PART")
        error "$file names no part: its first line is \"# <part>: ...\""
        continue
        ;;
    "NO RULE")
        error "$file holds no rule under \"## Rules\""
        continue
        ;;
    esac
    total=$((total + 1))
    where="$file:$line: rule \"$name\""
    case "$value" in
    MISSING) error "$where has no \"Checked by\" bullet" ;;
    MULTIPLE) error "$where has more than one \"Checked by\" bullet" ;;
    pending)
        pending=$((pending + 1))
        case "$file" in
        openspec/specs/*) error "$where is still pending in a part's contract" ;;
        esac
        ;;
    "probe — "?* | "agent review — "?* | "maintainer — "?*) ;;
    *)
        list=$(items "$value")
        if test -z "$list"; then
            error "$where names no check"
            continue
        fi
        while read -r item; do
            item=${item#\`}
            item=${item%\`}
            path=${item%%::*}
            text=${item#*::}
            if ! tracked "$path"; then
                error "$where: no file $path in the repository"
            elif ! grep -F -q -e "$text" -- "$path"; then
                error "$where: \"$text\" is not in $path"
            fi
        done <<EOF
$list
EOF
        ;;
    esac
done < "$head_list"

same=$(awk -F "$us" '$4 != "NO PART" && $4 != "NO RULE" { print $1 FS $4 }' \
    "$head_list" | sort | uniq -d)
if test -n "$same"; then
    while IFS="$us" read -r file name; do
        error "$file: two rules named \"$name\""
    done <<EOF
$same
EOF
fi

twice=$(cut -d "$us" -f 1,2 "$head_list" | sort -u | cut -d "$us" -f 2 |
    grep -v '^$' |
    sort | uniq -d)
if test -n "$twice"; then
    while read -r part; do
        error "two contracts name the part $part: a contract moves, it is not copied"
    done <<EOF
$twice
EOF
fi

if test $# -gt 0; then
    base=$(git merge-base "$1" HEAD)
    git ls-tree -r --name-only "$base" -- openspec/specs openspec/changes |
        contracts | rules_of show_at "$base" > "$base_list"
    loosened=0
    gone=""
    while IFS="$us" read -r file part line name value words; do
        case "$name" in "NO PART" | "NO RULE") continue ;; esac
        mine=$(part="$part" name="$name" awk -F "$us" '
            $2 == ENVIRON["part"] && $4 == ENVIRON["name"] {
                print $5 "\037" $6; found = 1; exit
            }
            END { if (!found) print "\001" }' "$head_list")
        if test "$mine" = "$(printf '\001')"; then
            if cut -d "$us" -f 2 "$head_list" | grep -q -x -F -e "$part"; then
                echo "check-contracts: the rule \"$name\" of $part is dropped" >&2
            elif ! printf '%s\n' "$gone" | grep -q -x -F -e "$part"; then
                echo "check-contracts: the contract of $part is gone" >&2
                gone="$gone
$part"
            fi
            loosened=1
            continue
        fi
        check=${mine%%"$us"*}
        said=${mine#*"$us"}
        if test "$said" != "$words"; then
            echo "check-contracts: the words of the rule \"$name\" of $part changed" >&2
            loosened=1
        fi
        was=$(rank "$value")
        now=$(rank "$check")
        if test "$now" -lt "$was"; then
            echo "check-contracts: the rule \"$name\" of $part is checked less than before" >&2
            loosened=1
        elif test "$now" -eq "$was" && test "$now" -eq 3; then
            mine_items=$(items "$check")
            while read -r item; do
                test -n "$item" || continue
                printf '%s\n' "$mine_items" | grep -q -x -F -e "$item" || {
                    echo "check-contracts: the rule \"$name\" of $part is no longer checked by $item" >&2
                    loosened=1
                }
            done <<EOF
$(items "$value")
EOF
        elif test "$now" -eq "$was" && test "$check" != "$value"; then
            echo "check-contracts: the check of the rule \"$name\" of $part changed" >&2
            loosened=1
        fi
    done < "$base_list"
    if test "$loosened" = 1; then
        others=$(git diff --name-only "$base" HEAD |
            grep -v -E '^openspec/(specs|changes)/[^/]+/(contract|design)\.md$' || true)
        if test -n "$others"; then
            error "a contract is loosened beside other changes: a rule changes only in a pull request of contracts and designs alone"
        else
            notice "a pull request of contracts and designs alone loosens a rule: the maintainer decides"
        fi
    fi
fi

echo "check-contracts: $total rules, $pending pending"
exit "$fail"
