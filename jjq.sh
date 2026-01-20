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
    echo "  push <revset>               - enqueue a revision" >&2
    echo "  run                         - process next queue item" >&2
    echo "  status                      - show queue and failed items" >&2
    echo "  cancel <id>                 - remove item from queue" >&2
    echo "  retry [-r <revset>] <id>    - re-queue a failed item" >&2
    echo "  clean                       - clean up stale state" >&2
    exit 1
}

if [ $# -lt 1 ]; then
    usage
fi

cmd="$1"
shift

# push enqueues a revision for the merge queue runner
push() {
    local revset="$1"

    if ! jj log -r "$revset" >/dev/null 2>&1; then
        echo "jjq: revset ${revset} not found"
        exit 1
    fi

    # Make sure it's not a descendent of main
    if jj log -r "${revset} & ::${MAIN_BOOKMARK}" --no-graph -T '""' 2>/dev/null | grep -q .; then
        echo "jjq: ${revset} is already a descendent of ${MAIN_BOOKMARK}" >&2
        exit 1
    fi

    # Get the next sequence ID from persistent counter
    mkdir -p .jjq
    local last_id_file=".jjq/last_id"
    if [ -f "$last_id_file" ]; then
        local last_id=$(cat "$last_id_file")
    else
        local last_id=0
    fi
    local id=$((last_id + 1))
    echo "$id" > "$last_id_file"

    jj bookmark create -r "$revset" "jjq/queue/$(printf "%06d" "$id")"
}

# run takes the next lowest item in the queue, creates a new commit with two
# parents, main and the candidate revset, and runs the check command on it.
run() {
    # TODO: acquire a lock here

    local id=$(jj bookmark list -r 'bookmarks(jjq/queue/*)' -T 'name ++ "\n"' | cut -f3 -d'/' | sort -n | head -1)

    if [ -z "$id" ]; then
        echo "jjq: queue is empty"
        exit 0
    fi

    local runlog=".jjq/run-${id}.log"
    exec 3>>"$runlog"
    echo "=== starting run for ${id} ===" >&3 2>&1

    if [ -z "$(jj bookmark list -r "bookmarks(exact:${MAIN_BOOKMARK})")" ]; then
        echo "jjq: '${MAIN_BOOKMARK}' bookmark does not exist"
        exit 0
    fi

    mkdir -p .jjq/workspaces
    local runner_workspace=".jjq/workspaces/run-${id}"
    jj workspace add --name "jjq/run/$id" -r "bookmarks(exact:${MAIN_BOOKMARK})" -r "bookmarks(exact:jjq/queue/${id})" "$runner_workspace" >&3 2>&1

    local old_pwd="$PWD"
    cd "$runner_workspace"
    mkdir -p .jjq

    # Check for merge conflicts before running CI
    if [ -n "$(jj log -r '@' --no-graph -T 'if(conflict, "has conflicts")')" ]; then
        jj bookmark delete "jjq/queue/$id" >&3 2>&1
        jj bookmark create "jjq/failed/$id" >&3 2>&1
        jj desc -m "Failed: merge $id (conflicts)" >&3 2>&1
        cd "$old_pwd"
        echo "jjq: merge has conflicts, marked as failed"
        exit 1
    fi

    jj desc -m "WIP: trying to merge $id" >&3 2>&1
    set +e
    if ! $CHECK_COMMAND >".jjq/$id.log" 2>&1; then
        set -e
        jj bookmark delete "jjq/queue/$id" >&3 2>&1
        jj bookmark create "jjq/failed/$id" >&3 2>&1
        (echo "Failed: merge $id"; cat ".jjq/$id.log") | jj desc --stdin >&3 
        # TODO: add more commit info via trailers (key-value pairs at bottom of the commit message)
        cd "$old_pwd"
    else
        set -e
        jj bookmark delete "jjq/queue/$id" >&3 2>&1
        jj bookmark create "jjq/passed/$id" >&3 2>&1
        (echo "Success: merge $id"; cat ".jjq/$id.log") | jj desc --stdin >&3 2>&1
        jj bookmark move $MAIN_BOOKMARK >&3 2>&1
        cd "$old_pwd"
        jj workspace forget "jjq/run/$id" >&3 2>&1
        rm -rf "$runner_workspace"
        jj bookmark delete "jjq/passed/$id" >&3 2>&1
    fi
}

status() {
    jj bookmark list -r 'bookmarks(glob:jjq/*)'
}

# cancel removes an item from the queue
cancel() {
    local id="$1"
    local bookmark="$(printf "jjq/queue/%06d" "$id")"

    if ! jj bookmark list -r "bookmarks(exact:$bookmark)" -T 'name' | grep -q .; then
        echo "jjq: queue item $id not found" >&2
        exit 1
    fi

    jj bookmark delete "$bookmark" >&3 2>&1
    echo "jjq: cancelled queue item $id"
}

confirm() {
    local prompt="${1:-Are you sure?}"
    read -rn1 -p "$prompt (y/N) " answer
    echo
    [[ $answer == [yY] ]]
}

# retry re-queues a failed item
# NOTE: assumes that the user has made some fix at the revision marked with the
# failed bookmark, perhaps squashing a new rev into it.
retry() {
    local revset

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -r|--revset)    revset="$2"; shift 2 ;;
            -*)             echo "Unknown option: $1" >&2; return 1 ;;
            --)             shift; break ;; # end of options
            *)              break ;; # first non-option, stop parsing
        esac
    done

    local id="$(printf "%06d" "$1")"
    local failed_bookmark="jjq/failed/$id"
    local workspace_name="jjq/run/$id"
    local workspace_dir=".jjq/workspaces/run-$id"

    if [ -z "$revset" ]; then
        revset="$failed_bookmark"
    fi

    if ! jj bookmark list -r "bookmarks(exact:$failed_bookmark)" -T 'name' | grep -q .; then
        echo "jjq: failed item $id not found" >&2
        exit 1
    fi

    # If there are descendents of the revset, warn the user if they are sure,
    # they might want to squash them down or retry this ID from a different rev
    if [ -n "$(jj log -r "${revset}:: ~ ${revset}" --no-graph -T 'change_id.short() ++ "\n"')" ]; then
        echo "jjq: WARNING: ${revset} has descendent commits. Are you sure you want to retry from here?" >&2
        confirm "Retry ${id} from ${revset}" || exit 1
    fi

    # Get the original revision
    local original_rev=$(jj log -r "bookmarks(exact:$failed_bookmark)" --no-graph -T 'change_id.short() ++ "\n"')

    if [ -z "$original_rev" ]; then
        echo "jjq: could not find original revision for $id" >&2
        exit 1
    fi

    local logfile=".jjq/retry-${id}.log"
    exec 3>>$logfile

    # Clean up failed state
    jj bookmark delete "$failed_bookmark" >&3 2>&1
    if jj workspace list | grep -q "^${workspace_name}:"; then
        jj workspace forget "$workspace_name" >&3 2>&1
    fi
    rm -rf "$workspace_dir"

    # Re-queue the original revision
    push "$original_rev"
    echo "jjq: re-queued $original_rev (was failed item $id)"
}

# clean removes stale workspaces and bookmarks
clean() {
    local cleaned=0

    # Clean up any jjq/run workspaces that don't have corresponding queue items
    for workspace in $(jj workspace list | grep '^jjq/run/' | cut -d: -f1); do
        local id=$(echo "$workspace" | cut -d/ -f3)
        if ! jj bookmark list -r "bookmarks(exact:jjq/queue/$id)" -T 'name' | grep -q .; then
            if ! jj bookmark list -r "bookmarks(exact:jjq/failed/$id)" -T 'name' | grep -q .; then
                echo "Cleaning up orphaned workspace: $workspace"
                jj workspace forget "$workspace" >&3 2>&1
                rm -rf ".jjq/workspaces/run-$id"
                cleaned=$((cleaned + 1))
            fi
        fi
    done

    # Clean up workspace directories without corresponding jj workspaces
    if [ -d ".jjq/workspaces" ]; then
        local dir
        for dir in .jjq/workspaces/run-*; do
            [ -d "$dir" ] || continue
            id=$(basename "$dir" | sed 's/run-//')
            workspace_name="jjq/run/$id"
            if ! jj workspace list | grep -q "^${workspace_name}:"; then
                echo "jjq: cleaning up orphaned directory: $dir"
                rm -rf "$dir"
                cleaned=$((cleaned + 1))
            fi
        done
    fi

    if [ $cleaned -eq 0 ]; then
        echo "jjq: nothing to clean"
    else
        echo "jjq: cleaned $cleaned items"
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
        retry "$@"
        ;;
    clean)
        clean
        ;;
    *)
        echo "unknown command '$cmd'" >&2
        usage
        ;;
esac

