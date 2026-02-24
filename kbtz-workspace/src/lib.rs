pub mod protocol;

/// Max scrollback rows retained per session for the scroll-back viewer.
/// Shared between the workspace (session.rs) and the shepherd.
pub const SCROLLBACK_ROWS: usize = 10_000;

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
/// 2. `state_formatted()` — restores the visible screen (starts with
///    ClearScreen, so scrollback is not affected).
/// 3. If the child was on the alt screen: DECSET 47 + alt screen
///    `state_formatted()`.
pub fn build_restore_sequence(vte: &mut vt100::Parser) -> Vec<u8> {
    let was_alt = vte.screen().alternate_screen();

    // Access main grid (DECRST 47 just clears the mode flag, no data loss)
    if was_alt {
        vte.process(b"\x1b[?47l");
    }

    let screen = vte.screen_mut();
    let cols = screen.size().1;

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

    // Phase 2: Restore main screen visible content
    restore.extend_from_slice(&screen.state_formatted());

    // Phase 3: If child was on alt screen, switch and restore alt content
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

        // Verify scrollback was transferred.
        let dst_scrollback = scrollback_depth(dst.screen_mut());
        assert!(
            dst_scrollback > 0,
            "destination should have scrollback, got 0"
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

        // Switch to main screen to check scrollback.
        dst.process(b"\x1b[?47l");
        let dst_scrollback = scrollback_depth(dst.screen_mut());
        assert!(
            dst_scrollback > 0,
            "main screen should have scrollback after alt screen restore"
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
}
