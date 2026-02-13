# Plan: `exec` command — atomic stdin batch execution

## Summary

Add a `kbtz exec` command that reads lines from stdin and executes them all within a single SQLite transaction (`BEGIN IMMEDIATE` ... `COMMIT`). If any line fails, the entire batch is rolled back. Each line is parsed as if it were the arguments to a `kbtz` invocation (subcommand + args).

Example usage:
```bash
echo 'add setup-db "Design schema"
add build-api "Implement API" -p setup-db
block setup-db build-api' | kbtz exec
```

## Design decisions

1. **Line format**: Each line is the subcommand portion of a kbtz invocation (e.g., `add foo "desc"`, not `kbtz add foo "desc"`). Lines are tokenized with shell-like quoting via the `shlex` crate.

2. **Transaction semantics**: The entire batch runs inside `BEGIN IMMEDIATE` / `COMMIT`. On any error, `ROLLBACK` is issued and the error is reported with the failing line number and content.

3. **Disallowed subcommands in batch**: `exec` (no recursion), `watch` (TUI), `wait` (blocks). These will produce a clear error.

4. **Note command restriction**: `note <name>` without inline content normally reads from stdin. Inside `exec`, stdin is consumed for batch lines, so `note` without content will error with a message to provide content explicitly.

5. **Output**: Commands that produce stdout output (`show`, `list`, `notes`, `claim-next`) work normally — their output appears in sequence during execution (but is only durable if the transaction commits).

6. **Blank lines and comments**: Blank lines are skipped. Lines starting with `#` are skipped (comments).

## Changes

### 1. `Cargo.toml` — add `shlex` dependency
Add `shlex = "1"` for shell-like line tokenization.

### 2. `src/cli.rs` — add `Exec` variant
```rust
/// Execute commands from stdin atomically
Exec,
```

### 3. `src/main.rs` — refactor and add exec handler

- **Extract `dispatch(conn, command) -> Result<()>`**: Move the body of each match arm in `run()` into a standalone function. This function takes a `&Connection` and a `Command` and executes it. The `Exec`, `Watch`, and `Wait` variants are handled specially (exec errors if nested; watch/wait are not dispatched through this path).

- **Add `Exec` handler in `run()`**:
  1. Open DB connection
  2. Read all lines from stdin
  3. Parse each non-blank, non-comment line: prepend `["kbtz"]` to the shlex-split tokens, parse with `Cli::try_parse_from`
  4. Reject disallowed commands (`exec`, `watch`, `wait`)
  5. `BEGIN IMMEDIATE`
  6. Dispatch each parsed command via `dispatch()`
  7. `COMMIT` on full success, `ROLLBACK` on any error
  8. On error, report the line number and content that failed

### 4. `src/ops.rs` — tests for atomic behavior
Add tests that verify:
- Successful batch: multiple operations commit together
- Failed batch: partial operations are rolled back
- Disallowed commands are rejected

## Non-goals
- No streaming/incremental execution — all lines are read before execution begins
- No support for piping stdin into inner `note` commands — content must be inline
- No dry-run mode (could be added later)
