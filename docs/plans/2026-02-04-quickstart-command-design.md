# Quickstart Command Design

## Summary

Add a `jjq quickstart` subcommand that prints static help text optimized
for LLM coding agents. The output teaches an agent what jjq is and how
to use it effectively, in ~500 words.

## Design

### Command

`jjq quickstart` — no arguments, no flags, no repo required. Prints
static text to stdout and exits with code 0.

### Implementation

- New `Quickstart` variant in the `Commands` enum (clap derive).
- Routes to a function that prints a const string.
- Text stored in `src/quickstart.txt`, loaded via `include_str!`.

### Output Content

Sections:
1. **Header** — one-line description of jjq
2. **WHAT IT DOES** — mental model (queue → merge → check → advance trunk)
3. **CORE COMMANDS** — init, push, run, status with brief descriptions
4. **AS AN AGENT** — what agents need to do (push @, check status)
5. **RESOLVING FAILURES** — the rebase-resolve-repush cycle (recipe)
6. **THINGS TO KNOW** — behavioral rules (FIFO, re-push clears failed, exit codes)
7. **TROUBLESHOOTING** — doctor, delete, clean

### Stress Test Learnings (informing content)

From running a full stress test with 3 parallel agents + conflict
resolution + bug fix cycle:

- Agents only need `push` and `status`. They never run the queue.
- Conflict resolution is the hardest workflow — needs a step-by-step recipe.
- `jjq push` on an already-failed change clears the failed entry
  automatically — this idempotent behavior is a key detail.
- `jjq check @` as pre-flight is valuable but agents don't discover it
  without being told.
- Exit codes (0=success, 1=failure, 2=partial) are used programmatically.
- `jjq clean` prevents orphaned workspace accumulation.

### Non-goals

- No dynamic content (no repo config inspection).
- No full command reference (omits config subcommand).
- No markdown formatting (plain text only).
