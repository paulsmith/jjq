#!/bin/bash
set -euo pipefail

# CONFIG
MAIN_BOOKMARK=main
CHECK_COMMAND="make ci"

usage() {
    echo "usage: jjq <cmd>" >&2
    echo "" >&2
    echo "commands:" >&2
    echo "" >&2
    echo "  push <revset>  - enqueue a revision" >&2
    echo "  run            - process next queue item" >&2
    echo "  status         - show queue and failed items" >&2
    echo "  cancel <id>    - remove item from queue" >&2
    echo "  retry <id>     - re-queue a failed item" >&2
    echo "  clean          - clean up stale state" >&2
    exit 1
}

if [ $# -lt 1 ]; then
    usage
fi

cmd="$1"
shift

# push enqueues a revision for the merge queue runner
push() {
    set -x

    local revset="$1"

    if ! jj log -r "$revset" >/dev/null 2>&1; then
        echo "revset ${revset} not found"
        exit 1
    fi

    # Make sure it's not a descendent of main
    if jj log -r "${revset} & ${MAIN_BOOKMARK}::" --no-graph -T '""' 2>/dev/null | grep -q .; then
        echo "${revset} is already a descendent of ${MAIN_BOOKMARK}" >&2
        exit 1
    fi

    # Get the next sequence ID from persistent counter
    mkdir -p .jjq
    last_id_file=".jjq/last_id"
    if [ -f "$last_id_file" ]; then
        last_id=$(cat "$last_id_file")
    else
        last_id=0
    fi
    id=$((last_id + 1))
    echo "$id" > "$last_id_file"

    jj bookmark create -r "$revset" "jjq/queue/$(printf "%06d" "$id")"

    set +x
}

# run takes the next lowest item in the queue, creates a new commit with two
# parents, main and the candidate revset, and runs the check command on it.
run() {
    set -x

    # TODO: acquire a lock here

    id=$(jj bookmark list -r 'bookmarks(jjq/queue/*)' -T 'name ++ "\n"' | cut -f3 -d'/' | sort -n | head -1)

    if [ -z "$id" ]; then
        echo "Queue is empty"
        exit 0
    fi

    mkdir -p .jjq/workspaces
    runner_workspace=".jjq/workspaces/run-${id}"
    jj workspace add --name "jjq/run/$id" -r "bookmarks(exact:${MAIN_BOOKMARK})" -r "bookmarks(exact:jjq/queue/${id})" "$runner_workspace"

    old_pwd="$PWD"
    cd "$runner_workspace"
    mkdir -p .jjq

    # Check for merge conflicts before running CI
    if [ -n "$(jj log -r '@' --no-graph -T 'if(conflict, "has conflicts")')" ]; then
        jj bookmark delete "jjq/queue/$id"
        jj bookmark create "jjq/failed/$id"
        jj desc -m "Failed: merge $id (conflicts)"
        cd "$old_pwd"
        echo "Merge has conflicts, marked as failed"
        set +x
        return
    fi

    jj desc -m "WIP: trying to merge $id"
    set +e
    if ! $CHECK_COMMAND >".jjq/$id.log" 2>&1; then
        set -e
        jj bookmark delete "jjq/queue/$id"
        jj bookmark create "jjq/failed/$id"
        (echo "Failed: merge $id"; cat ".jjq/$id.log") | jj desc --stdin
        # TODO: add more commit info via trailers (key-value pairs at bottom of the commit message)
        cd "$old_pwd"
    else
        set -e
        jj bookmark delete "jjq/queue/$id"
        jj bookmark create "jjq/passed/$id"
        (echo "Success: merge $id"; cat ".jjq/$id.log") | jj desc --stdin
        jj bookmark move $MAIN_BOOKMARK
        cd "$old_pwd"
        jj workspace forget "jjq/run/$id"
        rm -rf "$runner_workspace"
        jj bookmark delete "jjq/passed/$id"
    fi

    set +x
}

status() {
    jj bookmark list -r 'bookmarks(glob:jjq/*)'
}

# cancel removes an item from the queue
cancel() {
    local id="$1"
    local bookmark="jjq/queue/$id"

    if ! jj bookmark list -r "bookmarks(exact:$bookmark)" -T 'name' | grep -q .; then
        echo "Queue item $id not found" >&2
        exit 1
    fi

    jj bookmark delete "$bookmark"
    echo "Cancelled queue item $id"
}

# retry re-queues a failed item
retry() {
    local id="$1"
    local failed_bookmark="jjq/failed/$id"
    local workspace_name="jjq/run/$id"
    local workspace_dir=".jjq/workspaces/run-$id"

    if ! jj bookmark list -r "bookmarks(exact:$failed_bookmark)" -T 'name' | grep -q .; then
        echo "Failed item $id not found" >&2
        exit 1
    fi

    # Get the original revision (second parent of the failed merge)
    original_rev=$(jj log -r "bookmarks(exact:$failed_bookmark)-" --no-graph -T 'change_id' | tail -1)

    if [ -z "$original_rev" ]; then
        echo "Could not find original revision for $id" >&2
        exit 1
    fi

    # Clean up failed state
    jj bookmark delete "$failed_bookmark"
    if jj workspace list | grep -q "^${workspace_name}:"; then
        jj workspace forget "$workspace_name"
    fi
    rm -rf "$workspace_dir"

    # Re-queue the original revision
    push "$original_rev"
    echo "Re-queued $original_rev (was failed item $id)"
}

# clean removes stale workspaces and bookmarks
clean() {
    local cleaned=0

    # Clean up any jjq/run workspaces that don't have corresponding queue items
    for workspace in $(jj workspace list | grep '^jjq/run/' | cut -d: -f1); do
        id=$(echo "$workspace" | cut -d/ -f3)
        if ! jj bookmark list -r "bookmarks(exact:jjq/queue/$id)" -T 'name' | grep -q .; then
            if ! jj bookmark list -r "bookmarks(exact:jjq/failed/$id)" -T 'name' | grep -q .; then
                echo "Cleaning up orphaned workspace: $workspace"
                jj workspace forget "$workspace"
                rm -rf ".jjq/workspaces/run-$id"
                cleaned=$((cleaned + 1))
            fi
        fi
    done

    # Clean up workspace directories without corresponding jj workspaces
    if [ -d ".jjq/workspaces" ]; then
        for dir in .jjq/workspaces/run-*; do
            [ -d "$dir" ] || continue
            id=$(basename "$dir" | sed 's/run-//')
            workspace_name="jjq/run/$id"
            if ! jj workspace list | grep -q "^${workspace_name}:"; then
                echo "Cleaning up orphaned directory: $dir"
                rm -rf "$dir"
                cleaned=$((cleaned + 1))
            fi
        done
    fi

    if [ $cleaned -eq 0 ]; then
        echo "Nothing to clean"
    else
        echo "Cleaned $cleaned items"
    fi
}

case $cmd in
    push)
        if [ $# -lt 1 ]; then
            echo "push requires revset argument" >&2
            usage
        fi
        revset="$1"
        shift
        push "$revset"
        ;;
    run)
        run
        ;;
    status)
        status
        ;;
    cancel)
        if [ $# -lt 1 ]; then
            echo "cancel requires id argument" >&2
            usage
        fi
        cancel "$1"
        ;;
    retry)
        if [ $# -lt 1 ]; then
            echo "retry requires id argument" >&2
            usage
        fi
        retry "$1"
        ;;
    clean)
        clean
        ;;
    *)
        echo "unknown command '$cmd'" >&2
        usage
        ;;
esac

