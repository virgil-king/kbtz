# Rendering Design Rationale

Why each design element exists, what alternatives were rejected, and
what tradeoffs remain. See `session-rendering.md` for architecture
overview and the history of approaches tried.

## Raw byte forwarding for live output

The child process already knows how to render itself — it emits escape
sequences for cursor positioning, colors, screen clearing, etc.
Forwarding these bytes directly to stdout is zero-cost, flicker-free,
and correct.

**Why not render from VTE state**: We tried continuous VTE-mediated
rendering (approach #7 in session-rendering.md). The vt100 crate's
`state_diff()` emits `\r\n` between rows. Within the scroll region
that protects our status bar, `\r\n` causes content to scroll up,
creating visible duplication. Fixing this would require reimplementing
cell-by-cell diffing with cursor positioning — essentially what tmux
does. Raw forwarding sidesteps the problem entirely.

**Why raw forwarding is safe**: The workspace enters
`EnterAlternateScreen` at startup. The terminal's alternate screen has
no scrollback buffer. Raw bytes forwarded within it cannot pollute the
terminal emulator's scrollback. This is exactly how tmux renders pane
content.

## VTE always updated, even when not displaying

The reader thread feeds every byte into the VTE parser regardless of
whether the session is currently visible. Two consumers depend on this:

1. **Scroll mode** reads scrollback from the VTE.
2. **`render_screen_positioned()`** syncs the terminal with VTE state
   on transitions (entering passthrough, exiting scroll mode,
   resize/wake).

**Why not update on demand**: That would require buffering raw bytes
and replaying them when needed — which is approach #2, which failed
because raw bytes encode a specific terminal width. After a resize,
the buffer contains mixed-width content that cannot be replayed
correctly.

## SCROLLBACK_ROWS (10,000) on the live VTE

The VTE's structured scrollback is the only scrollback store. The
vt100 crate handles reflow on resize natively (it splits/joins lines
when the width changes). A raw byte buffer cannot do this — that
fundamental limitation caused approaches #2–4 to fail.

10,000 rows matches typical terminal emulator defaults. Memory cost is
modest: each row stores cells and attributes for the terminal width.

## DECRST/DECSET 47 trick for alt screen scrollback

The vt100 public API only exposes the active screen. When the child is
on the alternate screen, `scrollback()` returns 0 — the alt screen has
no scrollback. But the main screen grid (with all history) still exists
internally; it is just inaccessible through the public API.

We use `\x1b[?47l` (DECRST 47) to temporarily clear the
`MODE_ALTERNATE_SCREEN` flag, exposing the main grid. After cloning or
reading, `\x1b[?47h` (DECSET 47) restores the flag. These escape
sequences are processed entirely in the in-memory `vt100::Parser` —
they never reach the real terminal.

**Why DECSET 47, not 1049**: DECSET 1049 clears the alternate grid
when switching to it. DECSET 47 just sets the mode flag without
clearing. We need 47 so that restoring the flag does not destroy the
child's alt screen content. Verified by reading vt100's source code.

**Why not patch vt100**: Adding a public method to access the main
grid directly would work but requires forking the dependency. The
DECRST/DECSET 47 approach achieves the same result using the existing
API.

## Screen snapshot (clone) for scroll mode

Scroll mode presents a frozen view. The live VTE continues receiving
output from the reader thread while the user scrolls.
`vt100::Screen::clone()` captures the state at scroll mode entry.
`set_scrollback(offset)` on the clone shifts the viewport without
affecting the live VTE.

**Why clone instead of reading from the live VTE**: The live VTE is
behind a `Mutex` shared with the reader thread. Holding the lock while
rendering each scroll frame would block the reader thread, stalling
child output. Cloning once and releasing the lock is cleaner.

**Cost**: Cloning a Screen with deep scrollback is not free, but it
happens once on scroll mode entry, not per-frame.

## `render_screen_positioned()` with explicit cursor positioning

On transitions, we need to sync the terminal with the VTE's current
state. We cannot use `state_formatted()` or `state_diff()` because
they emit `\r\n` between rows, which scrolls content within the scroll
region that protects our status bar.

Instead, each row is rendered with `CSI row;1 H` (move cursor to row)
+ `CSI K` (erase line) + row content. Explicit cursor positioning
never causes scrolling — it jumps directly to the target row. This is
what tmux does.

## CSI 3 J interception

**Root cause of the original bug**: Claude Code sends `\x1b[3J` (Erase
Saved Lines) during context compaction to clear scrollback. tmux honors
this via `screen_write_clearhistory()`. The vt100 crate's `ed()`
handles modes 0, 1, 2 but falls through to `unhandled` for mode 3 —
silently ignoring it. Without interception, scrollback accumulates old
content that the child intended to clear.

**Implementation**: After `vte.process(data)`, we scan the byte chunk
for `\x1b[3J`. If found, we create a fresh VTE and replay
`state_formatted()` to preserve the visible screen while discarding
all scrollback.

**Why a fresh VTE**: The vt100 crate provides no public API to clear
scrollback. The grid's scrollback buffer is private. Creating a fresh
VTE and replaying `state_formatted()` is the only way to get a clean
slate while preserving visible screen content.

**Why scan for raw bytes instead of hooking the parser**: vt100 does
not expose a callback for unhandled sequences. Scanning for `\x1b[3J`
in the byte stream is simple and correct.

**Known limitation**: A CSI 3 J sequence split across two `read()`
calls would be missed. PTY reads rarely split 4-byte sequences, and
the worst case is one missed clear — the next compaction sends another.

## `stop()` resets SGR and input modes

When switching away from a session (to tree view or another session),
the child's terminal modes must not leak. The reset sequence covers:
SGR attributes, mouse tracking (modes 1000/1002/1003/1006), focus
events, bracketed paste, cursor keys, keypad, and cursor visibility.

Without this, a child that set bold red text or hid the cursor would
affect the tree view or other sessions.

## Shepherd owns scrollback

The shepherd is the long-lived process — it persists across workspace
restarts. Like tmux's server, it owns the authoritative scrollback via
its own VTE with `SCROLLBACK_ROWS`.

**Why the shepherd, not the workspace**: The workspace is a client
that can disconnect and reconnect. Without shepherd-side scrollback,
reconnecting would require replaying the full raw byte history, which
produces thousands of duplicate lines (every intermediate redraw frame
accumulates in scrollback).

## Size-first handshake on reconnection

The workspace sends `Resize` before reading `InitialState`. This lets
the shepherd resize its VTE to the current terminal dimensions before
building the restore sequence. Without this, the restore data would be
formatted for the old dimensions, producing mixed-width artifacts.

## `build_restore_sequence()` — structured restore

On reconnect, the shepherd sends scrollback rows +
`state_formatted()` instead of the full raw byte buffer. This
eliminates two problems: (a) mixed-width bytes from resize history,
and (b) intermediate redraw frames accumulating in scrollback.

**Phase order**: Scrollback rows are written first (oldest to newest)
so they scroll off the top of the receiving VTE into its scrollback
buffer. Then `state_formatted()` restores the visible screen — it
starts with ClearScreen, so the already-scrolled rows are unaffected.
If the child was on the alt screen, a third phase sends DECSET 47 +
alt screen `state_formatted()`.

## `resize_both_screens()`

The vt100 crate's `set_size()` only resizes the active screen grid. A
terminal has one physical size, so both the main and alternate grids
must match. If the child is on alt screen and only that grid is
resized, switching back to main (via DECRST 47 for scroll mode) shows
content at old dimensions. The same DECRST/DECSET 47 trick is used to
resize the inactive grid.

## Alternate screen at startup

The workspace enters `EnterAlternateScreen` once at startup. Both tree
view and passthrough view render within it. This prevents workspace
output from polluting the user's shell scrollback, and makes raw byte
forwarding safe (alt screen has no scrollback buffer for forwarded
bytes to accumulate in).

## Unconditional mouse tracking in passthrough

`render_screen_positioned()` always enables mouse tracking (modes
1000 + 1006), regardless of whether the child requested it. The
workspace intercepts scroll and click events for its own scroll mode
entry. Other mouse events are forwarded to the child only if the child
requested tracking.

**Tradeoff**: This means the workspace receives mouse events even from
apps that did not request them. Without it, there is no way to enter
scroll mode via mouse when viewing a non-mouse app.
