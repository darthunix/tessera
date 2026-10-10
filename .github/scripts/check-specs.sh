#!/bin/sh
# Check the OpenSpec folder against the repository, for CI and before a
# push (CONTRIBUTING.md, "Specs"):
#   check-specs.sh
# 1. `openspec validate --all --strict`: the form of every spec and change.
# 2. Every scenario ends with one bullet "- **Verified by:**", which may
#    continue on the next lines, indented. Each `path::text` it names is
#    a file of the repository that holds the text; "review only" is
#    counted and shown; "pending" is allowed only in an open change.
# 3. Every capability has spec.md and design.md, and each path under
#    "## Files" of design.md is in the repository. A design.md without a
#    spec.md needs an open change that adds the capability.
# 4. No closed change is kept: nothing is tracked under
#    openspec/changes/archive/ (CONTRIBUTING.md, "Closing a change").
# 5. No spec or design cites the working plan, the roadmap or a pull
#    request, or gives a date: they are self-contained.
# 6. No line of AGENTS.md, of CONTRIBUTING.md or of the Markdown under
#    openspec/ runs past 80 columns, but in a code block or where it
#    cannot be broken.
# 7. The architecture contracts, by check-contracts.sh: against the base
#    of a pull request when SPECS_BASE names it.
# Run from the root of the repository.
set -eu

fail=0
error() {
    echo "check-specs: $*" >&2
    fail=1
}

# A file or a directory of the repository: in the index, not only on disk.
tracked() {
    test -n "$(git ls-files -- "$1" | head -n 1)"
}

openspec validate --all --strict

# One line a scenario: file, line, name, the text after "Verified by:"
# (MISSING without the bullet, MULTIPLE with more than one).
scenarios() {
    awk -v file="$1" '
        function flush() {
            if (name != "") {
                if (count == 0)
                    value = "MISSING"
                else if (count > 1)
                    value = "MULTIPLE"
                printf "%s\t%d\t%s\t%s\n", file, line, name, value
            }
            name = ""
        }
        /^#### Scenario:/ {
            flush()
            name = $0
            sub(/^#### Scenario:[ \t]*/, "", name)
            line = NR
            count = 0
            next
        }
        /^#/ { flush(); taking = 0 }
        name != "" && /^- \*\*Verified by:\*\*/ {
            value = $0
            sub(/^- \*\*Verified by:\*\*[ \t]*/, "", value)
            count++
            taking = 1
            next
        }
        # The indented lines after the bullet belong to it.
        name != "" && taking && /^[ \t]+[^ \t]/ {
            more = $0
            sub(/^[ \t]+/, "", more)
            value = (value == "" ? more : value " " more)
            next
        }
        { taking = 0 }
        END { flush() }
    ' "$1"
}

tab=$(printf '\t')
total=0
review=0
pending=0
list=$(mktemp)
trap 'rm -f "$list"' EXIT

find openspec/specs openspec/changes -name spec.md \
    -not -path 'openspec/changes/archive/*' | sort |
    while read -r spec; do scenarios "$spec"; done > "$list"

while IFS="$tab" read -r file line name value; do
    total=$((total + 1))
    where="$file:$line: scenario \"$name\""
    case "$value" in
    MISSING)
        error "$where has no \"Verified by\" bullet"
        ;;
    MULTIPLE)
        error "$where has more than one \"Verified by\" bullet"
        ;;
    pending)
        pending=$((pending + 1))
        case "$file" in
        openspec/specs/*) error "$where is still pending in a main spec" ;;
        esac
        ;;
    "review only"*)
        review=$((review + 1))
        echo "review only: $where"
        ;;
    *)
        items=$(printf '%s\n' "$value" | grep -o '`[^`]*`' || true)
        if test -z "$items"; then
            error "$where names no test"
            continue
        fi
        while read -r item; do
            item=${item#\`}
            item=${item%\`}
            path=${item%%::*}
            if ! tracked "$path"; then
                error "$where: no file $path in the repository"
                continue
            fi
            case "$item" in
            *::*)
                text=${item#*::}
                grep -F -q -e "$text" -- "$path" ||
                    error "$where: \"$text\" is not in $path"
                ;;
            esac
        done <<EOF
$items
EOF
        ;;
    esac
done < "$list"

for directory in openspec/specs/*/; do
    test -d "$directory" || continue
    capability=$(basename "$directory")
    design=openspec/specs/$capability/design.md
    spec=openspec/specs/$capability/spec.md
    if ! test -f "$spec"; then
        added=$(find openspec/changes -path "*/specs/$capability/spec.md" \
            -not -path 'openspec/changes/archive/*' | head -n 1)
        test -n "$added" ||
            error "$capability has no spec.md and no open change adds it"
    fi
    if ! test -f "$design"; then
        error "$capability has no design.md"
        continue
    fi
    files=$(awk '
        /^## / { inside = ($0 ~ /^## Files[ \t]*$/) ; next }
        inside { print }
    ' "$design" | grep -o '`[^`]*`' || true)
    if test -z "$files"; then
        error "$design names no file under \"## Files\""
        continue
    fi
    while read -r item; do
        item=${item#\`}
        item=${item%\`}
        tracked "$item" || error "$design: no $item in the repository"
    done <<EOF
$files
EOF
done

if tracked openspec/changes/archive; then
    error "openspec/changes/archive/ keeps a closed change: delete it"
fi

# What a spec or a design must not lean on: it outlives the plan, the
# roadmap and the pull requests.
git ls-files -- 'openspec/specs/*.md' | while read -r document; do
    grep -n -i -E \
        'docs/plan|working plan|plan item|roadmap|pull request|[0-9]{4}-[0-9]{2}-[0-9]{2}' \
        "$document" | sed "s|^|$document:|"
done > "$list"
while read -r line; do
    error "$line: a spec or a design cites the plan, the roadmap, a pull request or a date"
done < "$list"

# A line of more than 80 columns: allowed in a code block, and where
# what follows the indent or the list marker is one word, a link or a
# path, which cannot be broken. Columns are characters, not bytes: a
# dash or a letter outside ASCII is one.
git ls-files -- AGENTS.md CONTRIBUTING.md 'openspec/*.md' |
    while read -r document; do
    perl -CSD -ne '
        if (/^\s*(```|~~~)/) { $fence = !$fence; next }
        next if $fence;
        chomp;
        next unless length($_) > 80;
        (my $rest = $_) =~ s/^\s*(?:[-*+]|\d+\.)?\s*//;
        printf "%s:%d: a line of %d columns\n", $ARGV, $., length($_)
            if $rest =~ /\s/;
    ' "$document"
done > "$list"
while read -r wide; do
    error "$wide"
done < "$list"

.github/scripts/check-contracts.sh ${SPECS_BASE:+"$SPECS_BASE"} || fail=1

echo "check-specs: $total scenarios, $review review only, $pending pending"
exit "$fail"
