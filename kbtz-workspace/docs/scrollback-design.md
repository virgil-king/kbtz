# Scrollback Design (Clean Slate)

## Goal

tmux-like scrollback for all CLI apps (main screen and alt screen),
surviving terminal resizes and shepherd reconnections, with no duplication.

## Architecture: Shepherd Owns the Scrollback

The shepherd is the long-lived process. Like tmux's server, it should own
the authoritative scrollback. The workspace is a client that can disconnect
and reconnect.

```
                    SHEPHERD (long-lived)
                    ┌──────────────────────┐
Child PTY ────────► │ VTE (SCROLLBACK_ROWS)│ ◄── source of truth
                    │ No raw byte buffer   │
                    └──────────┬───────────┘
                               │
                    Unix socket (protocol.rs)
                               │
                    ┌──────────┴───────────┐
                    │ WORKSPACE (transient) │
                    │ VTE (SCROLLBACK_ROWS) │ ◄── rebuilt on reconnect
                    └──────────────────────┘
```

### What changes from today

| Component | Today | New |
|-----------|-------|-----|
| Shepherd VTE | `scrollback=0` | `scrollback=SCROLLBACK_ROWS` |
| Shepherd raw buffer | 16 MB `output_buffer` | **Removed** |
| InitialState content | Raw bytes (every byte child ever wrote) | Synthetic state: scrollback rows + visible screen |
| Reconnect handshake | Shepherd sends InitialState immediately | Workspace sends Resize first, then shepherd sends InitialState at correct size |
| Workspace VTE | `scrollback=SCROLLBACK_ROWS` | Same |

### Why the shepherd must own scrollback

The workspace is transient — it restarts, and when it reconnects it needs
history. Raw bytes cannot provide this correctly because:

1. **Raw bytes encode terminal width.** After resize, the buffer contains
   output at mixed widths. Replaying produces duplication (approach #2).
2. **Raw bytes contain intermediate redraws.** Claude Code redraws at
   ~60fps. Replaying into a VTE with scrollback fills it with thousands
   of duplicate frames.
3. **Truncating raw bytes loses context.** You can't trim the buffer on
   resize without losing history (approach #3).

Structured VTE scrollback avoids all three: the vt100 crate stores lines
as structured data and handles resize reflow natively.

## Reconnection Protocol

Today the handshake is:
1. Workspace connects
2. Shepherd sends `InitialState(raw_bytes)` immediately
3. Workspace sends `Resize(rows, cols)`

This is wrong because InitialState is at the shepherd's current size, then
the workspace resizes — the scrollback was rendered at the wrong width.

### New handshake

1. Workspace connects and sends `Resize(rows, cols)` immediately
2. Shepherd receives Resize, updates its VTE size and PTY size
3. Shepherd constructs InitialState from its VTE (now at correct size)
4. Shepherd sends `InitialState(synthetic_bytes)`

The workspace processes InitialState into its fresh VTE. Done.

### Constructing InitialState (shepherd side)

The shepherd builds a synthetic byte stream that, when processed by a
fresh VTE with `SCROLLBACK_ROWS`, reproduces the correct state.

#### Case 1: Child on main screen

```
[scrollback_line_1]\r\n
[scrollback_line_2]\r\n
...
[scrollback_line_N]\r\n
[state_formatted()]          ← clears screen, positions cursor, fills cells
```

The scrollback lines cause content to scroll off the top of the workspace's
VTE into its scrollback buffer. Then `state_formatted()` starts with
`ClearScreen` + cursor positioning, restoring the visible screen without
affecting scrollback.

#### Case 2: Child on alt screen

```
[main_scrollback_line_1]\r\n
[main_scrollback_line_2]\r\n
...
[main_scrollback_line_N]\r\n
[main_screen state_formatted()]     ← restore main screen visible content
\x1b[?47h                           ← enter alt screen (DECSET 47, no clear)
[alt_screen state_formatted()]      ← restore alt screen content
```

The shepherd uses the DECRST 47 trick to access the main grid:
1. `\x1b[?47l]` — expose main grid
2. Extract scrollback rows + main screen state
3. `\x1b[?47h` — restore alt grid
4. Extract alt screen state

DECSET 47 (not 1049) is used for the synthetic stream because it does NOT
clear the alt grid on the workspace side. The workspace's VTE ends up with
both grids populated and the alt screen flag set.

#### Extracting scrollback rows

The shepherd iterates through its VTE's scrollback:

```rust
fn extract_scrollback_rows(screen: &mut vt100::Screen, cols: u16) -> Vec<Vec<u8>> {
    // Probe total depth
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();

    let mut rows = Vec::with_capacity(total);
    for offset in (1..=total).rev() {
        screen.set_scrollback(offset);
        // First row of viewport at this offset is the scrollback line
        if let Some(row) = screen.rows_formatted(0, cols).next() {
            rows.push(row);
        }
    }
    screen.set_scrollback(0);
    rows
}
```

Each row from `rows_formatted()` includes inline escape codes for color
and attributes, so formatting is preserved.

## Scroll Mode (workspace side)

Unchanged from current implementation. The workspace VTE has
`SCROLLBACK_ROWS`. Scroll mode uses the DECRST 47 trick to access the
main grid, clones the Screen, and renders from the clone.

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
    // snapshot now has main grid with scrollback...
}
```

After reconnection, the workspace's VTE has scrollback populated from the
shepherd's InitialState, so scroll mode works immediately.

## Direct PTY Sessions (non-shepherd)

No change needed. The workspace owns the VTE directly with
`SCROLLBACK_ROWS`. There is no reconnection — the VTE lives as long as
the child process. Scroll mode works the same way.

## Resize Handling

### During normal operation

1. Workspace receives terminal resize event
2. Workspace resizes its local VTE: `screen_mut().set_size(rows, cols)`
   (vt100 reflows scrollback natively)
3. Workspace sends `Resize(rows, cols)` to shepherd
4. Shepherd resizes its VTE and the child PTY

Both VTEs reflow their scrollback to the new width. No raw bytes involved.

### During reconnection

The new handshake sends Resize before InitialState, so the shepherd
constructs InitialState at the workspace's current size. The workspace
VTE processes it at the same size. No mixed-width data.

## Invariants

These must always be true. Violating any of them causes duplication or
data loss.

1. **No raw byte buffers for scrollback.** Raw bytes encode terminal
   width and contain intermediate redraws. They cannot be replayed
   correctly after resize.

2. **Scrollback is structured line data in VTE grids.** The vt100 crate
   handles reflow. We never try to reflow ourselves.

3. **InitialState contains synthetic screen state, not raw replay.**
   The shepherd constructs InitialState from its VTE's structured data,
   not from accumulated raw bytes.

4. **Resize before InitialState.** The workspace sends its size before
   the shepherd constructs InitialState, so the data is at the correct
   width.

5. **DECSET 47 (not 1049) for alt screen toggles.** DECSET 1049 clears
   the alt grid. DECSET 47 does not. Both in the DECRST 47 trick and
   in the synthetic InitialState stream.

6. **Escape sequences in the DECRST 47 trick never reach the real
   terminal.** They are processed by the in-memory `vt100::Parser`.

## Gotchas from Previous Attempts

| Gotcha | How this design avoids it |
|--------|--------------------------|
| Raw bytes at mixed widths after resize → duplication | No raw byte buffer. VTE reflows natively. |
| Terminal scrollback is append-only → stale content | Workspace renders from VTE state via `state_diff`, never writes raw bytes to stdout. |
| vt100 API only exposes active screen → no scrollback on alt screen | DECRST 47 trick toggles the mode flag to access main grid. |
| DECSET 1049 clears alt screen → data loss | Use DECSET 47 which doesn't clear. |
| InitialState raw replay fills scrollback with intermediate redraws | Shepherd sends structured state, not raw replay. |
| Resize after InitialState → scrollback at wrong width | Handshake sends Resize first. |
| Clearing buffer on resize loses history | No buffer to clear. VTE scrollback persists across resizes. |

## Memory

Each VTE with 10,000 scrollback rows at 200 columns uses roughly
10,000 * 200 * ~16 bytes/cell = ~32 MB. With both shepherd and workspace
maintaining scrollback, that's ~64 MB per session.

This is comparable to tmux's per-pane memory. If it becomes a concern,
`SCROLLBACK_ROWS` can be reduced or made configurable. The raw
`output_buffer` it replaces was already capped at 16 MB but growing
unpredictably.

## Implementation Sequence

### 1. Change shepherd VTE to use SCROLLBACK_ROWS

Replace `vt100::Parser::new(rows, cols, 0)` with
`vt100::Parser::new(rows, cols, SCROLLBACK_ROWS)` in kbtz-shepherd.
Remove `output_buffer` and `append_output_buffer()` entirely.

### 2. Add scrollback extraction to shepherd

Implement `construct_initial_state(vte, was_alt)` that builds the
synthetic byte stream described above.

### 3. Change reconnection handshake

- Shepherd: after accepting a connection, read the first message (must
  be Resize), apply it, then send InitialState.
- Workspace: on connect, send Resize first, then read InitialState.

### 4. Remove workspace-side temp VTE workaround

The workspace no longer needs to process InitialState through a temp VTE
with `scrollback=0`. It just processes the synthetic bytes directly into
its Passthrough VTE.

### 5. Update protocol documentation

InitialState semantics change from "raw output buffer" to "synthetic
screen state including scrollback."
