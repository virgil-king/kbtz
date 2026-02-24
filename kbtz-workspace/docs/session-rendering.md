# kbtz-workspace Session Rendering Architecture

## Overview

kbtz-workspace is a terminal multiplexer that manages child sessions (Claude
Code, shells, etc.). Each session runs in a PTY (direct) or connects to a
persistent shepherd process (reconnectable). The workspace presents two views:

- **Tree view**: ratatui-rendered task list (tree.rs)
- **Passthrough view**: child session terminal output (session.rs, main.rs)

Both render to the same alternate screen, entered once at startup.

## Rendering Pipeline

```
Child process
    │
    ▼
PTY (or shepherd socket)
    │
    ▼
Reader thread ──► vt100::Parser (Passthrough.vte)
                       │
                       ├── dirty flag (AtomicBool)
                       │
                       ▼
Main thread polls has_new_output()
    │
    ├── render_diff(prev): screen.state_diff(prev) → stdout
    │   Returns new Screen for next diff
    │
    └── render_full(): screen.state_formatted() → stdout
        Used after mode switches (scroll exit, tree→passthrough)
```

The main loop in `passthrough_loop` polls at 16ms (~60fps). On each tick:
1. Check `has_new_output()` (AtomicBool set by reader thread)
2. If dirty: `render_screen(prev)` writes `state_diff` to stdout
3. Track `prev_screen: Option<vt100::Screen>` for efficient diffs
4. `prev_screen = None` forces a full re-render (used after scroll mode exit)

## Alternate Screen Lifecycle

The workspace enters the alternate screen once at startup
(`EnterAlternateScreen`) and leaves it on exit (`LeaveAlternateScreen`).
Both tree view and passthrough view render to this same alternate screen.

When switching from passthrough to tree view, the screen is cleared
(`\x1b[r` reset scroll region + `Clear(ClearType::All)`) before ratatui
renders the tree. When switching back to passthrough, a full VTE render
restores the child's screen state.

## Session Types

### Direct PTY sessions (Session)

- `portable_pty` creates a PTY pair
- Reader thread reads from PTY master, feeds `Passthrough.vte`
- Writer sends keyboard input to PTY master
- Process lifecycle tied to child PID

### Shepherd sessions (ShepherdSession)

- `kbtz-shepherd` binary manages the PTY and persists across workspace restarts
- Communication via Unix socket with a framed protocol (protocol.rs)
- Messages: `PtyOutput`, `PtyInput`, `Resize`, `InitialState`, `Shutdown`
- On connect, shepherd sends `InitialState` (full output buffer) to
  reconstruct the VTE state
- Reader thread reads `PtyOutput` messages, feeds `Passthrough.vte`

## Scroll Mode

Scroll mode lets users browse terminal history. Activated by `^B [`,
Shift+Up, PgUp, or mouse scroll.

### Current approach (live VTE scrollback + DECRST 47 trick)

The live VTE is created with `SCROLLBACK_ROWS` (10,000). The vt100 crate
stores scrollback as structured line data on the **main screen grid**,
with automatic reflow on resize.

**Problem**: The `vt100` API only exposes the active screen via `screen()`.
When the child is on the alternate screen, `scrollback()` returns 0 because
the alt screen has no scrollback. The main screen grid (with all the history)
is inaccessible through the public API.

**Solution**: DECRST/DECSET 47 toggle. Looking at `vt100`'s source:

- `Screen` has `grid` (always main) and `alternate_grid` (always alt)
- `grid()` returns whichever is active based on `MODE_ALTERNATE_SCREEN`
- `\x1b[?47l` (DECRST 47): just clears the mode flag. No data modification.
- `\x1b[?47h` (DECSET 47): sets the mode flag + `allocate_rows()` (no-op if
  rows exist). Does NOT clear the alt screen (unlike DECSET 1049 which does).

So we can temporarily toggle the mode flag to access the main grid:

```rust
fn enter_scroll_mode(&mut self) -> usize {
    let was_alt = self.vte.screen().alternate_screen();
    if was_alt {
        self.vte.process(b"\x1b[?47l"); // expose main grid
    }
    let mut snapshot = self.vte.screen().clone();
    if was_alt {
        self.vte.process(b"\x1b[?47h"); // restore alt grid
    }
    // probe scrollback depth from snapshot...
}
```

This mirrors tmux's approach: tmux can read from the main screen grid at any
time because it owns the data structures. We achieve the same by briefly
flipping the flag, cloning the screen, and flipping back. The escape sequences
are processed by the in-memory `vt100::Parser` and never reach the real
terminal.

### Scroll rendering

`render_scrollback(offset, cols)` uses the cloned `Screen`:
- `screen.set_scrollback(offset)` shifts the viewport
- `screen.rows_formatted(0, cols)` returns styled row data
- Each row is written with `\x1b[0m` reset before `\x1b[K` to prevent
  attribute leaking between rows

## UX Requirements

1. **No scrollback duplication**: Resizing the terminal must not cause
   duplicate content in scroll mode
2. **Works for all CLI apps**: Main screen apps (Claude Code, shells) and
   alt screen apps (vim, less, htop)
3. **Correct tmux-like behavior**: Scroll mode shows main screen history,
   including content from before an alt screen switch
4. **Proper reflow on resize**: Content should reflow to the new width,
   not show mixed-width artifacts
5. **Flicker-free rendering**: Use `state_diff` for efficient updates
6. **Background output tracking**: Sessions accumulate output in their VTE
   even when the user is viewing a different session or tree view

## Approaches Tried and Why They Failed

### 1. Raw byte forwarding (original architecture)

**How it worked**: Reader thread wrote child output directly to stdout.
`start_passthrough()` / `stop_passthrough()` toggled whether the reader
thread wrote to stdout.

**Why it failed**: Every byte the child wrote went to the real terminal's
scrollback buffer. When viewing a session, its entire history accumulated
in the terminal's scrollback. Switching sessions left stale scrollback.
"Scrollback shows many copies of the same content" — the terminal kept
every screen redraw.

**Root cause**: The terminal's scrollback is append-only. There's no way
to prevent it from accumulating output that happens to scroll off screen.

### 2. VTE rendering + raw output buffer replay

**How it worked**: Reader threads only feed `vt100::Parser` (no stdout
writes). Main thread renders from VTE state using `state_diff`. A raw byte
`output_buffer` (capped at 16MB) accumulated all child output. Scroll mode
replayed the buffer into a temporary VTE with `SCROLLBACK_ROWS` to
reconstruct history.

**Why it failed**: After terminal resize, the buffer contained bytes
rendered at the old width AND the child's re-render at the new width.
Replaying into a VTE at the new width showed both versions — duplication.

**Root cause**: Raw bytes encode a specific terminal width. Mixed-width
bytes in a single buffer cannot be replayed correctly at a different width.

**Example**: Terminal at 120 cols, child outputs long lines. Resize to 80
cols. Child re-renders at 80 cols. Buffer has `[120-col bytes][80-col bytes]`.
Replay at 80 cols: 120-col lines wrap incorrectly, AND the 80-col re-render
shows the same content again.

### 3. Clear output buffer on resize

**How it worked**: On resize, clear the output buffer and seed it with the
VTE's `state_formatted()` at the new size.

**Why it failed**: Loses all scrollback history. The VTE had `scrollback=0`,
so `state_formatted()` only captured the visible screen.

**User feedback**: "Wait! We need to keep scrollback history!!"

### 4. Resize log tracking

**How it worked**: Track `(buffer_offset, rows, cols)` at each resize.
When replaying, chunk the buffer at resize boundaries and replay each
chunk at its original size.

**Why it failed**: Never fully implemented — too complex, and the
conversation led to a simpler direction. The approach would have required
creating multiple temporary VTEs at different sizes and somehow merging
their scrollback.

### 5. Enable SCROLLBACK_ROWS on live VTE, drop buffer entirely

**How it worked**: Give the live VTE 10,000 rows of scrollback. Scroll
mode reads directly from the live VTE. No raw byte buffer at all.

**Why it almost worked**: Proper reflow on resize (vt100 handles it
natively). No duplication. Simpler code.

**Why it was insufficient**: When the child is on the alternate screen, the
VTE's scrollback belongs to the main screen grid. The `vt100` API only
exposes the active screen — on alt screen, `scrollback()` returns 0.
For Claude Code (which doesn't use alt screen) this would work fine, but
not for alt-screen apps like vim.

**Key insight from user**: "I want to support all CLI apps, not just
Claude Code." tmux handles this with one mechanism because it has direct
access to both screen grids.

### 6. Main-screen-only buffer (hybrid)

**How it worked**: Live VTE with scrollback for main screen apps. Buffer
only accumulates bytes when NOT on alt screen. Scroll mode branches:
main screen → live VTE, alt screen → buffer replay.

**Why it was rejected**: Still two code paths. User wanted one mechanism
like tmux: "tmux seems to do fine with all of them."

### 7. Live VTE scrollback + DECRST 47 trick (current)

**How it works**: Live VTE with `SCROLLBACK_ROWS`. Scroll mode temporarily
toggles DECRST/DECSET 47 to access the main grid, clones the Screen.

**Why it works**: One mechanism for all apps. DECSET 47 doesn't clear the
alt screen (unlike DECSET 1049). The escape sequences are processed by the
in-memory VTE parser, never reaching the real terminal. vt100 handles
resize reflow natively. No raw byte buffer needed.

**Status**: Implemented, tests passing, awaiting user testing.

## Key vt100 Internals

Understanding the `vt100` crate structure is essential for this system:

```
vt100::Parser
  └── Screen
        ├── grid: Grid          (always main screen, has scrollback)
        ├── alternate_grid: Grid (always alt screen, scrollback=0)
        └── modes: u8           (includes MODE_ALTERNATE_SCREEN bit)

grid() → if MODE_ALTERNATE_SCREEN set → &alternate_grid
         else → &grid
```

- `grid()` is `pub(crate)` — not accessible from outside the crate
- All public methods (`scrollback()`, `rows_formatted()`, `set_scrollback()`,
  etc.) go through `grid()`, so they always operate on the active screen
- `Screen` derives `Clone`, enabling the snapshot approach
- DECSET 47 vs 1049: both toggle MODE_ALTERNATE_SCREEN, but 1049 also
  clears the alt grid. 47 does not clear. This is why we use 47.

## File Map

- `session.rs`: `Passthrough` struct, `SessionHandle` trait, `Session` impl,
  reader thread, scroll mode logic
- `shepherd_session.rs`: `ShepherdSession` impl of `SessionHandle`, shepherd
  protocol reader thread
- `main.rs`: Main event loop, `passthrough_loop`, scroll mode UI,
  tree/passthrough view switching, alternate screen management
- `app.rs`: `App` struct managing sessions, tree view orchestration
- `tree.rs`: ratatui tree view rendering
- `protocol.rs`: Shepherd wire protocol (framed messages over Unix socket)
- `bin/kbtz-shepherd.rs`: Persistent PTY manager process
