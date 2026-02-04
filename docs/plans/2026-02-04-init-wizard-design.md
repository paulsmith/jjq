# Init Wizard Design

## Goal

Replace jjq's silent auto-initialization with an explicit `jjq init` command
that walks the user through setup. This makes the entry point clear, ensures
required config (especially `check_command`) is set before anything runs, and
supports both interactive and scripted workflows.

## Command Interface

```
jjq init [--trunk <bookmark>] [--check <command>]
```

- `--trunk`: Sets `trunk_bookmark`. If omitted, prompt interactively.
- `--check`: Sets `check_command`. If omitted, prompt interactively.
- If both flags are provided, no prompts — fully scriptable.
- If stdin is not a TTY and flags are missing, error immediately:
  `error: --trunk and --check are required in non-interactive mode.`
- If the repo is already initialized (`jjq/_/_` bookmark exists), error:
  `error: jjq is already initialized. Use 'jjq config' to change settings.`

## Interactive Flow

```
$ jjq init

Initializing jjq in this repository.

Trunk bookmark [main]:
Check command: make test

Initialized jjq:
  trunk_bookmark = main
  check_command  = make test

Running doctor...
  jj repository         ok
  jjq initialized       ok
  trunk bookmark exists  ok
  check command set      ok
  run lock               ok (free)
  id lock                ok (free)
  orphaned workspaces    ok (0)

Ready to go! Queue revisions with 'jjq push <revset>'.
```

**Trunk bookmark prompt:** Default shown in `[brackets]`. Determined by scanning
existing bookmarks — `main` if it exists, `master` as fallback, no default
otherwise. Empty input with no default re-prompts.

**Check command prompt:** No default. Empty input re-prompts with hint:
`A check command is required (e.g., 'make test', 'cargo test').`

After config is written, `jjq doctor` runs automatically and results are
displayed.

## Implementation Changes

### New code

- `Init` variant added to the CLI enum in `main.rs` with optional `--trunk`
  and `--check` args.
- `init()` function in `commands.rs` orchestrating the wizard flow.

### Changes to existing code

- `config::ensure_initialized()` splits into:
  - `config::is_initialized()` — returns `bool`, checks if `jjq/_/_` exists.
  - `config::initialize()` — creates the metadata branch. Only called from
    `init()`.
- Every other command checks `is_initialized()` on entry and errors with
  `"jjq is not initialized. Run 'jjq init' first."` if false. Replaces silent
  auto-init.

### Interactive prompting

- `std::io::stdin()` with `BufRead` for reading input.
- `std::io::IsTerminal` on stdin for TTY detection (stable since Rust 1.70).
- Bookmark scanning via existing jj helpers to find default candidates.

### No new dependencies

Everything needed is in std or already in the project.

## Doc Updates

- **`README.md`** — Quickstart shows `jjq init` as the first step. Update
  command list.
- **`docs/jjq.1`** — Add `init` subcommand docs (synopsis, flags, interactive
  behavior). Update language that implies auto-initialization.
- **`docs/jjq.1.txt`** — Regenerate or update to match.
- **`AGENTS.md`** — Update if it references setup steps or command list.
