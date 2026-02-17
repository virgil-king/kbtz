use std::io::Write;

/// Write `buf` to `out`, omitting escape sequences that would trigger
/// a terminal response when replayed.
///
/// Stripped sequences:
/// - **CSI queries**: Device Attributes (`ESC[c`, `ESC[>c`), Device
///   Status Report (`ESC[6n`), and any other CSI ending in `c` or `n`.
/// - **OSC queries**: Any OSC whose last parameter is `?` (color
///   queries like `ESC]11;?ST`, palette queries like `ESC]4;0;?ST`).
///
/// All other output — text, SGR colors, cursor movement, mode changes,
/// non-query OSC (window title, hyperlinks) — is preserved so terminal
/// scrollback renders correctly.
pub fn replay(out: &mut impl Write, buf: &[u8]) {
    let mut i = 0;
    // Everything in buf[flush_from..i] is "safe" output that hasn't
    // been written yet.  We batch writes for efficiency.
    let mut flush_from = 0;

    while i < buf.len() {
        if buf[i] != 0x1b || i + 1 >= buf.len() {
            i += 1;
            continue;
        }

        let seq_start = i;

        match buf[i + 1] {
            b'[' => {
                // CSI sequence: ESC [
                i += 2;
                // Parameter bytes (0x30–0x3F: digits, semicolons, <=>? etc.)
                while i < buf.len() && (0x30..=0x3F).contains(&buf[i]) {
                    i += 1;
                }
                // Intermediate bytes (0x20–0x2F: space, !, ", #, $ etc.)
                while i < buf.len() && (0x20..=0x2F).contains(&buf[i]) {
                    i += 1;
                }
                // Final byte (0x40–0x7E)
                if i < buf.len() && (0x40..=0x7E).contains(&buf[i]) {
                    let final_byte = buf[i];
                    i += 1;
                    if final_byte == b'c' || final_byte == b'n' {
                        let _ = out.write_all(&buf[flush_from..seq_start]);
                        flush_from = i;
                    }
                }
            }
            b']' => {
                // OSC sequence: ESC ]
                i += 2;
                let data_start = i;
                // Scan for string terminator: BEL (0x07) or ST (ESC \).
                while i < buf.len() {
                    if buf[i] == 0x07 {
                        // BEL-terminated OSC.
                        let data = &buf[data_start..i];
                        i += 1; // consume BEL
                        if is_osc_query(data) {
                            let _ = out.write_all(&buf[flush_from..seq_start]);
                            flush_from = i;
                        }
                        break;
                    }
                    if buf[i] == 0x1b && i + 1 < buf.len() && buf[i + 1] == b'\\' {
                        // ST-terminated OSC (ESC \).
                        let data = &buf[data_start..i];
                        i += 2; // consume ESC \.
                        if is_osc_query(data) {
                            let _ = out.write_all(&buf[flush_from..seq_start]);
                            flush_from = i;
                        }
                        break;
                    }
                    i += 1;
                }
                // If we hit end-of-buffer without a terminator, the
                // incomplete OSC is included in the next flush as-is.
            }
            _ => {
                // Other ESC sequence — skip the ESC and let the next
                // iteration handle the following byte normally.
                i += 1;
            }
        }
    }
    let _ = out.write_all(&buf[flush_from..]);
}

/// Returns true if the OSC data represents a query (last param is `?`).
///
/// Examples: `11;?` (background color query), `4;0;?` (palette query).
fn is_osc_query(data: &[u8]) -> bool {
    // Query OSCs end with `;?` — the `?` is the sole content of the
    // last semicolon-delimited parameter.
    data.len() >= 2 && data[data.len() - 2] == b';' && data[data.len() - 1] == b'?'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_da_and_dsr() {
        let mut output = Vec::new();
        // DA1 query (ESC [ c), DSR query (ESC [ 6 n), with normal text and SGR around them.
        let input = b"hello\x1b[c world\x1b[6n!\x1b[1;31mred\x1b[0m";
        replay(&mut output, input);
        assert_eq!(output, b"hello world!\x1b[1;31mred\x1b[0m");
    }

    #[test]
    fn strips_da2() {
        let mut output = Vec::new();
        // DA2 query: ESC [ > c  ('>' is 0x3E, a parameter byte)
        let input = b"before\x1b[>cafter";
        replay(&mut output, input);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn preserves_non_query_csi() {
        let mut output = Vec::new();
        // CUP, ED, SGR — none end in 'c' or 'n', so all are preserved.
        let input = b"\x1b[1;1H\x1b[2Jhello\x1b[1;31mred\x1b[0m";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn handles_plain_text() {
        let mut output = Vec::new();
        let input = b"just plain text\n";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn handles_incomplete_csi_at_end() {
        let mut output = Vec::new();
        // Buffer ends with an incomplete CSI — should be preserved as-is.
        let input = b"text\x1b[";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn strips_osc_background_color_query_bel() {
        let mut output = Vec::new();
        // OSC 11 ; ? BEL — background color query, BEL-terminated.
        let input = b"before\x1b]11;?\x07after";
        replay(&mut output, input);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn strips_osc_background_color_query_st() {
        let mut output = Vec::new();
        // OSC 11 ; ? ST — background color query, ST-terminated.
        let input = b"before\x1b]11;?\x1b\\after";
        replay(&mut output, input);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn strips_osc_foreground_color_query() {
        let mut output = Vec::new();
        // OSC 10 ; ? BEL — foreground color query.
        let input = b"before\x1b]10;?\x07after";
        replay(&mut output, input);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn strips_osc_palette_query() {
        let mut output = Vec::new();
        // OSC 4 ; 0 ; ? BEL — palette color 0 query.
        let input = b"before\x1b]4;0;?\x07after";
        replay(&mut output, input);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn preserves_osc_window_title() {
        let mut output = Vec::new();
        // OSC 0 ; title BEL — set window title (not a query).
        let input = b"\x1b]0;my title\x07hello";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn preserves_osc_hyperlink() {
        let mut output = Vec::new();
        // OSC 8 ; ; url ST — hyperlink with ? in URL (not a query).
        let input = b"\x1b]8;;https://example.com?q=1\x1b\\link\x1b]8;;\x1b\\";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn handles_incomplete_osc_at_end() {
        let mut output = Vec::new();
        // Unterminated OSC — preserved as-is.
        let input = b"text\x1b]11;?";
        replay(&mut output, input);
        assert_eq!(output.as_slice(), input.as_slice());
    }

    #[test]
    fn mixed_csi_and_osc_queries() {
        let mut output = Vec::new();
        // Mix of CSI DA, OSC color query, normal text, and SGR.
        let input = b"a\x1b[cb\x1b]11;?\x07c\x1b[1;31md\x1b[0m";
        replay(&mut output, input);
        assert_eq!(output, b"abc\x1b[1;31md\x1b[0m");
    }
}
