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
                       ├── if active: raw bytes → stdout (child controls rendering)
                       │
                       └── VTE always updated (for scroll mode, reconnection)
```

The reader thread has two responsibilities:
1. **Always** feed every byte into the VTE parser (for scroll mode and reconnection)
2. **When `active`**: also write those raw bytes directly to stdout

The main loop in `passthrough_loop` handles only input, status bar, resize
detection, and scroll mode. It does NOT render child output — that's the
reader thread's job via raw byte forwarding.

On mode transitions (entering passthrough, exiting scroll mode, resize/wake),
`render_screen_positioned()` does a one-shot VTE-to-terminal sync using
explicit cursor positioning per row (`CSI row;1 H` + `CSI K` + content).
This never causes terminal scrolling.

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
  reconstruct the VTE state (see "Shepherd Reconnection and Scrollback" below)
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
5. **Flicker-free rendering**: Raw byte forwarding is inherently flicker-free
   (the child writes its own escape sequences). Transition renders use cursor
   positioning, not screen clears.
6. **Background output tracking**: Sessions accumulate output in their VTE
   even when the user is viewing a different session or tree view

## Approaches Tried and Why They Failed

### 1. Raw byte forwarding (original architecture)

**How it worked**: Reader thread wrote child output directly to stdout.
`start_passthrough()` / `stop_passthrough()` toggled whether the reader
thread wrote to stdout.

**What this doc previously claimed**: "Every byte the child wrote went to
the real terminal's scrollback buffer. Switching sessions left stale
scrollback."

**Correction**: This failure analysis was wrong for kbtz-workspace's actual
setup. The workspace enters `EnterAlternateScreen` at startup. The
terminal's alternate screen has **no scrollback** — content that scrolls
off the top of a scroll region in alt screen simply disappears. Raw byte
forwarding within the alt screen does not accumulate terminal scrollback.
This is exactly how tmux works: tmux panes render within the alt screen,
and raw PTY output never enters the terminal emulator's scrollback buffer.

The approach's actual limitation was not about terminal scrollback pollution
but about **our own scroll mode**: with `scrollback=0` on the VTE and a raw
`output_buffer` for scroll mode reconstruction, resizing caused duplication
(see approach #2). Raw byte forwarding itself was fine for live rendering.

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

### 7. VTE-mediated rendering + DECRST 47 trick

**How it worked**: Live VTE with `SCROLLBACK_ROWS`. Reader threads only
feed the VTE (no stdout writes). Main thread renders from VTE state using
`state_diff(prev)` on a 16ms poll loop. Scroll mode uses the DECRST 47
trick to access main screen scrollback.

**What worked**: The DECRST 47 trick for scroll mode. One mechanism for all
apps. DECSET 47 doesn't clear the alt screen (unlike DECSET 1049). vt100
handles resize reflow natively. No raw byte buffer needed.

**Why it failed**: The vt100 crate's `state_diff()` and `state_formatted()`
emit sequential `\r\n` between rows when rendering large screen changes.
Within a scroll region (`\x1b[1;{rows-1}r`, used to protect the status
bar), these `\r\n` sequences cause content to scroll within the region.
Since the workspace is on the terminal's alternate screen, this scrolling
doesn't pollute terminal scrollback, but it causes **visible duplication
within the session viewport**: content rendered by `state_diff` scrolls up
and is immediately re-rendered by the child's next output frame.

The fundamental problem is that `state_diff` is designed for full-screen
rendering without scroll regions. It assumes it owns the entire terminal
and uses newlines to move between rows. This is incompatible with a
terminal multiplexer that uses scroll regions for status bars. tmux avoids
this by implementing its own cell-by-cell diff with explicit cursor
positioning — it never emits `\r\n` during rendering.

**Root cause**: Continuous VTE-mediated rendering with `state_diff()` is
architecturally incompatible with scroll regions.

### 8. Raw forwarding + VTE scrollback + DECRST 47 trick (current)

**How it works**: Combines raw byte forwarding (approach #1) for live
output with live VTE scrollback + DECRST 47 trick (approach #7) for scroll
mode. The reader thread does two things: (1) always feeds bytes into the
VTE parser, and (2) when `active`, also writes those raw bytes directly to
stdout. The main loop handles only input, status bar, resize, and scroll
mode — it never renders child output.

On transitions (entering passthrough, exiting scroll mode, resize/wake),
`render_screen_positioned()` does a one-shot VTE-to-terminal sync using
explicit cursor positioning per row (`CSI row;1 H` + `CSI K` + content).
This never emits `\r\n` and is safe within scroll regions.

**Why this is different from approach #1**: Approach #1 had `scrollback=0`
on the VTE and used a raw `output_buffer` for scroll mode, which caused
duplication on resize. This approach keeps the live VTE with
`SCROLLBACK_ROWS` and the DECRST 47 trick from approach #7, giving proper
scroll mode with structured line data and native reflow.

**Why this is different from approach #7**: Approach #7 rendered ALL child
output through the VTE using `state_diff()`, which emitted `\r\n` within
the scroll region causing visible duplication. This approach lets the child
render its own output via raw forwarding (the child's escape sequences
handle cursor positioning correctly), and only uses the VTE for transitions
and scroll mode.

**Key insight**: The workspace is on the terminal's alternate screen
(entered at startup via `EnterAlternateScreen`). The alt screen has no
terminal scrollback, so raw byte forwarding cannot pollute it. This is
exactly how tmux works: panes render raw PTY output within the alt screen.

**CSI 3 J handling**: The vt100 crate does not implement CSI 3 J (Erase
Saved Lines), which Claude Code sends to clear scrollback during context
compaction. tmux handles this via `screen_write_clearhistory()`. We
intercept CSI 3 J in `Passthrough::process()` by creating a fresh VTE
and replaying only the visible screen state, discarding all scrollback.

**Status**: Implemented, confirmed working.

## Shepherd Reconnection and Scrollback

The shepherd maintains its own VTE with `SCROLLBACK_ROWS` — it is the
authoritative scrollback store, like tmux's server. On reconnect, the
workspace sends a `Resize` message first (size-first handshake), then the
shepherd uses `build_restore_sequence()` (in `lib.rs`) to build a
synthetic byte stream from structured VTE data:

1. Scrollback rows (oldest first), each followed by `\r\n` — these scroll
   off the top of the receiving VTE into its scrollback buffer.
2. `state_formatted()` — restores the visible screen (starts with
   ClearScreen, so scrollback is not affected).
3. If the child was on the alt screen: DECSET 47 + alt screen
   `state_formatted()`.

This preserves scrollback across reconnections and eliminates the
mixed-width duplication that plagued raw byte replay.

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
