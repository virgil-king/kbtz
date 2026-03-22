pub mod protocol;

use std::io::{BufWriter, StdoutLock, Write};

/// Max scrollback rows retained per session for the scroll-back viewer.
/// Shared between the workspace (session.rs) and the shepherd.
pub const SCROLLBACK_ROWS: usize = 10_000;

/// Run `f` inside a buffered, synchronized stdout update.
///
/// Wraps stdout in a `BufWriter` so all writes coalesce into one flush,
/// and brackets the output with DEC private mode 2026 (synchronized
/// update) so terminals that support it hold painting until the frame
/// is complete.  Terminals that don't recognize the sequence ignore it.
pub fn with_sync_stdout<T>(f: impl FnOnce(&mut BufWriter<StdoutLock<'_>>) -> T) -> T {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let _ = out.write_all(b"\x1b[?2026h");
    let result = f(&mut out);
    let _ = out.write_all(b"\x1b[?2026l");
    let _ = out.flush();
    result
}

/// Resize both the main and alternate screen grids.  The vt100 crate's
/// `set_size()` only resizes the active screen; a terminal has one
/// physical size so both grids must match.
pub fn resize_both_screens(vte: &mut vt100::Parser, rows: u16, cols: u16) {
    let was_alt = vte.screen().alternate_screen();
    if was_alt {
        vte.process(b"\x1b[?47l"); // expose main grid
        vte.screen_mut().set_size(rows, cols);
        vte.process(b"\x1b[?47h"); // restore alt grid
    }
    vte.screen_mut().set_size(rows, cols);
}

/// Build a synthetic byte stream from a VTE that, when processed by a
/// fresh `vt100::Parser` with `SCROLLBACK_ROWS`, reproduces the screen
/// state including scrollback.
///
/// The sequence is:
/// 1. Scrollback rows (oldest first), each followed by `\r\n` — these
///    scroll off the top of the receiving VTE into its scrollback buffer.
/// 2. Visible rows 0..H-1 (each followed by `\r\n`) — these provide
///    the scroll pressure needed to push all scrollback rows off-screen
///    in the receiving VTE.  Writing N lines with `\r\n` to an H-row
///    screen produces N - H + 1 scrollback rows, so we need
///    `total_scrollback + H - 1` total lines to get `total_scrollback`
///    rows of scrollback.  The visible rows themselves end up on-screen
///    and are overwritten by the next step.
/// 3. `state_formatted()` — restores the visible screen using cursor
///    positioning (no scrolling), so scrollback is not affected.
/// 4. If the child was on the alt screen: DECSET 47 + alt screen
///    `state_formatted()`.
pub fn build_restore_sequence(vte: &mut vt100::Parser) -> Vec<u8> {
    let was_alt = vte.screen().alternate_screen();

    // Access main grid (DECRST 47 just clears the mode flag, no data loss)
    if was_alt {
        vte.process(b"\x1b[?47l");
    }

    let screen = vte.screen_mut();
    let (rows, cols) = screen.size();

    // Probe total scrollback depth
    screen.set_scrollback(usize::MAX);
    let total_scrollback = screen.scrollback();

    let mut restore = Vec::new();

    // Phase 1: Write scrollback rows (oldest first = highest offset)
    // At offset N, the top row of the viewport is the Nth-oldest scrollback line.
    for offset in (1..=total_scrollback).rev() {
        screen.set_scrollback(offset);
        if let Some(row_bytes) = screen.rows_formatted(0, cols).next() {
            restore.extend_from_slice(&row_bytes);
            restore.extend_from_slice(b"\r\n");
        }
    }
    screen.set_scrollback(0);

    // Phase 2: Write visible rows 0..H-1 to create enough scroll
    // pressure for the receiving VTE.
    //
    // INVARIANT: N lines with \r\n to an H-row screen → N-H+1 scrollback
    // rows (the first H-1 lines fill the screen without scrolling).
    // Phase 1 writes total_scrollback lines.  We need total_scrollback
    // scrollback rows, so we must write total_scrollback + H - 1 lines
    // total.  These H-1 visible rows supply the difference.
    for row_bytes in screen
        .rows_formatted(0, cols)
        .take((rows as usize).saturating_sub(1))
    {
        restore.extend_from_slice(&row_bytes);
        restore.extend_from_slice(b"\r\n");
    }

    // Phase 3: Restore main screen visible content (uses cursor
    // positioning, no scrolling — scrollback is not affected).
    restore.extend_from_slice(&screen.state_formatted());

    // Phase 4: If child was on alt screen, switch and restore alt content
    if was_alt {
        vte.process(b"\x1b[?47h"); // restore shepherd's VTE to alt screen

        // Use DECSET 47 (not 1049) in the restore stream — 47 does NOT
        // clear the alt grid on the receiving side.
        restore.extend_from_slice(b"\x1b[?47h");
        restore.extend_from_slice(&vte.screen().state_formatted());
    }

    restore
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: probe total scrollback depth of a screen.
    fn scrollback_depth(screen: &mut vt100::Screen) -> usize {
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        screen.set_scrollback(0);
        total
    }

    #[test]
    fn restore_sequence_preserves_scrollback() {
        // Source VTE: 5 rows, 40 cols, write enough lines to create scrollback.
        let mut src = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        for i in 0..20 {
            src.process(format!("line {i}\r\n").as_bytes());
        }
        // Write something on the visible screen.
        src.process(b"visible content");

        let src_scrollback = scrollback_depth(src.screen_mut());
        assert!(src_scrollback > 0, "source should have scrollback");

        // Build restore sequence and replay into a fresh VTE.
        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        dst.process(&restore);

        // Verify visible screen matches.
        let src_contents = src.screen().contents();
        let dst_contents = dst.screen().contents();
        assert_eq!(src_contents, dst_contents, "visible screen should match");

        // Verify scrollback depth matches exactly.
        let dst_scrollback = scrollback_depth(dst.screen_mut());
        assert_eq!(
            dst_scrollback, src_scrollback,
            "scrollback depth must match exactly"
        );
    }

    #[test]
    fn restore_sequence_with_alt_screen() {
        // Source VTE: write main screen content, then switch to alt screen.
        let mut src = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        for i in 0..20 {
            src.process(format!("main line {i}\r\n").as_bytes());
        }
        // Enter alt screen and write something there.
        src.process(b"\x1b[?1049h");
        src.process(b"alt content");
        assert!(src.screen().alternate_screen());

        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        dst.process(&restore);

        // Should be on alt screen.
        assert!(
            dst.screen().alternate_screen(),
            "destination should be on alt screen"
        );

        // Alt screen content should match.
        assert_eq!(
            src.screen().contents(),
            dst.screen().contents(),
            "alt screen content should match"
        );

        // Switch to main screen to check scrollback depth matches.
        src.process(b"\x1b[?47l");
        let src_scrollback = scrollback_depth(src.screen_mut());
        dst.process(b"\x1b[?47l");
        let dst_scrollback = scrollback_depth(dst.screen_mut());
        assert_eq!(
            dst_scrollback, src_scrollback,
            "main screen scrollback depth must match after alt screen restore"
        );
    }

    #[test]
    fn restore_sequence_no_scrollback() {
        // Source VTE with no scrollback (fewer lines than screen height).
        let mut src = vt100::Parser::new(10, 40, SCROLLBACK_ROWS);
        src.process(b"hello world");

        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(10, 40, SCROLLBACK_ROWS);
        dst.process(&restore);

        assert_eq!(src.screen().contents(), dst.screen().contents());
        assert_eq!(scrollback_depth(dst.screen_mut()), 0);
    }

    #[test]
    fn restore_at_different_size_reflows() {
        // Source at 80 cols, write long lines.
        let mut src = vt100::Parser::new(5, 80, SCROLLBACK_ROWS);
        for i in 0..20 {
            src.process(
                format!("this is a long line number {i} with plenty of content\r\n").as_bytes(),
            );
        }

        // Resize source to 40 cols (simulating workspace reconnecting at
        // a different size — the shepherd resizes before building restore).
        src.screen_mut().set_size(5, 40);

        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        dst.process(&restore);

        // Visible screen should match.
        assert_eq!(src.screen().contents(), dst.screen().contents());

        // Should have scrollback (lines reflowed to 40 cols).
        let dst_scrollback = scrollback_depth(dst.screen_mut());
        assert!(dst_scrollback > 0, "should have scrollback after reflow");
    }

    /// Helper: capture the visible viewport at each scrollback offset.
    /// Returns one `contents()` snapshot per offset, oldest first.
    fn scrollback_viewports(vte: &mut vt100::Parser) -> Vec<String> {
        let screen = vte.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        let mut viewports = Vec::new();
        for offset in (1..=total).rev() {
            screen.set_scrollback(offset);
            viewports.push(screen.contents());
        }
        screen.set_scrollback(0);
        viewports
    }

    #[test]
    fn restore_sequence_scrollback_content_matches() {
        // Verify that viewport content at every scrollback offset
        // survives the restore sequence, not just the depth.
        let mut src = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        for i in 0..30 {
            src.process(format!("line {i}\r\n").as_bytes());
        }
        src.process(b"visible");

        let src_viewports = scrollback_viewports(&mut src);
        assert!(
            src_viewports.len() > 5,
            "need meaningful scrollback for test"
        );

        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(5, 40, SCROLLBACK_ROWS);
        dst.process(&restore);

        let dst_viewports = scrollback_viewports(&mut dst);
        assert_eq!(
            dst_viewports.len(),
            src_viewports.len(),
            "scrollback depth must match"
        );
        for (i, (src_vp, dst_vp)) in src_viewports.iter().zip(dst_viewports.iter()).enumerate() {
            assert_eq!(
                src_vp.trim(),
                dst_vp.trim(),
                "scrollback viewport at offset {i} mismatch"
            );
        }
    }

    /// Regression test at realistic terminal size.  The scrollback depth
    /// bug (missing H-1 rows) was most visible at large screen heights
    /// where H-1 is a significant number of lost rows.
    #[test]
    fn restore_sequence_realistic_size() {
        let rows: u16 = 50;
        let cols: u16 = 200;
        let mut src = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        // Simulate a long session with varied content.
        for i in 0..500 {
            src.process(format!("output line {i}: some content here\r\n").as_bytes());
        }
        src.process(b"cursor here");

        let src_scrollback = scrollback_depth(src.screen_mut());
        assert!(
            src_scrollback > (rows as usize),
            "need more scrollback than screen height"
        );

        let restore = build_restore_sequence(&mut src);
        let mut dst = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        dst.process(&restore);

        assert_eq!(src.screen().contents(), dst.screen().contents());
        assert_eq!(
            scrollback_depth(dst.screen_mut()),
            src_scrollback,
            "scrollback depth must match at realistic terminal size"
        );
    }
}
