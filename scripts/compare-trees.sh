#!/usr/bin/env bash
# Compare two installed package directories file by file: same set of paths,
# same type, same bytes, same symlink targets. Ignores .forge/ (the manifest
# names the package, so it differs by design).
set -euo pipefail
a=$1; b=$2
list() { (cd "$1" && find . -not -path './.forge' -not -path './.forge/*' \( -type f -o -type l -o -type d \) | sort); }
la=$(list "$a"); lb=$(list "$b")
only_a=$(comm -23 <(echo "$la") <(echo "$lb") | wc -l)
only_b=$(comm -13 <(echo "$la") <(echo "$lb") | wc -l)
diff_n=0; same_n=0
while IFS= read -r p; do
    [ -n "$p" ] || continue
    pa="$a/$p"; pb="$b/$p"
    [ -e "$pb" ] || [ -L "$pb" ] || continue
    if [ -L "$pa" ] || [ -L "$pb" ]; then
        if [ "$(readlink "$pa" 2>/dev/null || echo A)" = "$(readlink "$pb" 2>/dev/null || echo B)" ]; then same_n=$((same_n+1)); else echo "SYMLINK DIFFERS: $p"; diff_n=$((diff_n+1)); fi
    elif [ -f "$pa" ]; then
        if cmp -s "$pa" "$pb"; then same_n=$((same_n+1)); else echo "CONTENT DIFFERS: $p"; diff_n=$((diff_n+1)); fi
    fi
done <<< "$la"
echo "paths only in the first: $only_a, only in the second: $only_b, files/links identical: $same_n, different: $diff_n"
[ "$only_a" = 0 ] && [ "$only_b" = 0 ] && [ "$diff_n" = 0 ]
