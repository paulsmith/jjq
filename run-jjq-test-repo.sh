#!/bin/bash
# ABOUTME: Test runner script for jjq merge queue that handles known conflicts.
# ABOUTME: Processes pr1-pr4 from test repo, resolving conflicts deterministically.
set -euo pipefail

export NON_INTERACTIVE=1

echo "=== Pushing PRs onto the merge queue ==="
jjq push pr1
jjq push pr2
jjq push pr3
jjq push pr4
jjq status

echo ""
echo "=== Running the merge queue ==="

# Resolve jj conflict markers in a file using sed
# Conflict format: <<<<<<< ... +++++++ (side1) ... %%%%%%% (diff) ... >>>>>>> ends
# - Lines starting with just tab: side1 content (trunk) - KEEP
# - Lines starting with space+tab: unchanged base context - DELETE
# - Lines starting with +tab: additions from side2 - KEEP (remove + prefix)
resolve_jj_conflict() {
    local file="$1"
    sed -i '' '/^<<<<<<</d; /^+++++++/d; /^%%%%%%%/d; /^\\\\\\\\/d; /^>>>>>>>/d; /^ 	/d; s/^+	/	/' "$file"
}

# Process the queue until empty
while true; do
    next_id=$(jj bookmark list -r 'bookmarks(glob:"jjq/queue/??????")' -T'name ++"\n"' 2>/dev/null | grep -E '^jjq/queue/[0-9]{6}$' | cut -f3 -d'/' | sort -n | head -1 || true)

    if [ -z "$next_id" ]; then
        echo "Queue is empty - all done!"
        break
    fi

    echo "Processing queue item $((10#$next_id))..."

    if jjq run; then
        echo "Item $((10#$next_id)) merged successfully"
    else
        padded_id=$(printf "%06d" "$((10#$next_id))")
        failed_rev="jjq/failed/$padded_id"

        # Check if this is a conflict failure (vs check failure)
        fail_reason=$(jj log -r "$failed_rev" --no-graph -T'description.first_line()' 2>/dev/null || true)
        if [[ "$fail_reason" != *"(conflicts)"* ]]; then
            echo "Item $((10#$next_id)) failed check (not conflicts) - cannot auto-resolve"
            exit 1
        fi

        echo "Item $((10#$next_id)) has conflicts - resolving..."
        fix_dir=$(mktemp -d)
        trap "rm -rf $fix_dir" EXIT

        jj workspace add -r "$failed_rev" --name "jjq-fix-$$" "$fix_dir" >/dev/null 2>&1
        pushd "$fix_dir" >/dev/null

        if [ -f main.go ] && grep -q '<<<<<<' main.go 2>/dev/null; then
            echo "  Resolving conflict in main.go..."
            resolve_jj_conflict main.go
            # Fix comment position: "// say hi" should precede the Println call
            if grep -q '// say hi' main.go 2>/dev/null; then
                sed -i '' '/\/\/ say hi/d' main.go
                sed -i '' 's/fmt\.Println(say\.Greet/\/\/ say hi\'$'\n\tfmt.Println(say.Greet/' main.go
            fi
            go fmt ./... >/dev/null 2>&1 || true
        fi

        if [ -f main_test.go ] && grep -q '<<<<<<' main_test.go 2>/dev/null; then
            echo "  Resolving conflict in main_test.go..."
            resolve_jj_conflict main_test.go
        fi

        jj desc -m "Resolved conflict" >/dev/null 2>&1
        popd >/dev/null

        echo "  Retrying with resolved revision..."
        jjq retry "$((10#$next_id))" "jjq-fix-$$@"

        jj workspace forget "jjq-fix-$$" >/dev/null 2>&1
        rm -rf "$fix_dir"
        trap - EXIT
    fi
    echo ""
done

echo ""
echo "=== Final status ==="
jjq status
