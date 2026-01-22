#!/bin/bash
set -euo pipefail

export NON_INTERACTIVE=1

echo "Pushing PRs onto the merge queue"
jjq push pr1
jjq push pr2
jjq push pr3
jjq push pr4

echo "Running the merge queue"
jjq run
