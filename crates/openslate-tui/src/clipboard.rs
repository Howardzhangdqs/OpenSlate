//! Clipboard — OSC 52 copy-out (fix-13).
//!
//! In-app copy that bypasses terminal text selection entirely: the last
//! assistant message's RAW markdown is wrapped into an OSC 52 escape
//! (`ESC ] 52 ; c ; <base64> BEL`) and written to the real stdout.
//! Terminals (and tmux with `set-clipboard on`) forward it to the
//! system clipboard, so it works through SSH and inside tmux where
//! native selection is blocked by mouse capture / copy-mode semantics.
//!
//! No new dependencies: base64 is hand-rolled below (standard RFC 4648
//! alphabet with `=` padding — what OSC 52 receivers expect), and the
//! write goes through a thin [`ClipboardSink`] seam whose production
//! implementation performs the same write+flush on the REAL stdout that
//! `crossterm::execute!` does for the mouse-capture toggle (NOT the
//! ratatui backend). The terminal consumes the sequence immediately —
//! the next diff-based draw is unaffected.

/// Copy size cap: text above 32 KiB is truncated (OSC 52 length is
/// unbounded by spec but terminal/tmux implementations cap it in
/// practice; 32 KiB is the conservative interop point).
pub const COPY_MAX_BYTES: usize = 32 * 1024;

/// Standard base64 alphabet (RFC 4648, `+/` variant).
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encode `data` (standard alphabet, `=` padding). Hand-rolled
/// (~30 lines) to keep the dependency tree frozen.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = *chunk.get(1).unwrap_or(&0) as u32;
        let b3 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b1 << 16) | (b2 << 8) | b3;
        let index = |shift: u32| B64_ALPHABET[(triple >> shift) as usize & 0x3f] as char;
        out.push(index(18));
        out.push(index(12));
        // The trailing octets may be absent (final chunk) — those
        // sextets become `=` padding instead of alphabet digits.
        out.push(if chunk.len() > 1 { index(6) } else { '=' });
        out.push(if chunk.len() > 2 { index(0) } else { '=' });
    }
    out
}

/// Build the OSC 52 escape carrying `text` (UTF-8 → base64) for the
/// SYSTEM clipboard selector `c` (tmux/terminal also mirror `c` to the
/// primary selection where relevant). BEL terminator: the most widely
/// consumed form (tmux emits it itself).
pub fn osc52_payload(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Cap `text` at [`COPY_MAX_BYTES`] of UTF-8, cutting back to the
/// nearest char boundary (never splits a multi-byte char). Returns a
/// slice of the original when it already fits.
pub fn truncate_for_copy(text: &str) -> &str {
    if text.len() <= COPY_MAX_BYTES {
        return text;
    }
    let mut end = COPY_MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Where the OSC 52 escape goes (test seam). Returns whether the write
/// succeeded — a failed write must NOT report success in the notice.
pub type ClipboardSink = Box<dyn Fn(&str) -> bool + Send>;

/// Production sink: write the payload bytes to the REAL stdout and
/// flush (the same channel + write+flush the run loop's
/// mouse-capture toggle uses via `crossterm::execute!`; there is no
/// raw-string crossterm `Command`, and `execute!` on a string command
/// does exactly this under the hood).
pub fn stdout_clipboard_sink() -> ClipboardSink {
    Box::new(|payload| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        out.write_all(payload.as_bytes()).is_ok() && out.flush().is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known vectors (RFC 4648 test suite + UTF-8): every padding case
    /// (0/1/2 `=`) and a multi-chunk CJK string.
    #[test]
    fn base64_matches_known_vectors() {
        let vectors: &[(&str, &str)] = &[
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("line1\nline2", "bGluZTEKbGluZTI="),
            // 你好 = E4 BD A0 E5 A5 BD (UTF-8) → two clean 3-byte chunks.
            ("你好", "5L2g5aW9"),
        ];
        for (input, expected) in vectors {
            assert_eq!(
                &base64_encode(input.as_bytes()),
                expected,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn payload_is_osc52_wrapped_base64() {
        let payload = osc52_payload("foobar");
        assert!(
            payload.starts_with("\x1b]52;c;"),
            "OSC 52 header with the system-clipboard selector: {payload:?}"
        );
        assert!(payload.ends_with('\x07'), "BEL terminator: {payload:?}");
        let body = &payload["\x1b]52;c;".len()..payload.len() - 1];
        assert_eq!(body, "Zm9vYmFy");
        // Exactly one escape introducer, nothing stray.
        assert_eq!(payload.matches('\x1b').count(), 1);
        assert_eq!(payload.matches('\x07').count(), 1);
        // UTF-8 input rides through as base64 of its bytes.
        assert_eq!(osc52_payload("你好"), "\x1b]52;c;5L2g5aW9\x07");
    }

    #[test]
    fn short_text_is_never_truncated() {
        assert_eq!(truncate_for_copy("hello"), "hello");
        // Exactly at the cap stays whole (boundary is inclusive).
        let exact = "x".repeat(COPY_MAX_BYTES);
        assert_eq!(truncate_for_copy(&exact), exact);
    }

    #[test]
    fn long_text_caps_at_32_kib() {
        let long = "x".repeat(COPY_MAX_BYTES + 1000);
        let cut = truncate_for_copy(&long);
        assert_eq!(cut.len(), COPY_MAX_BYTES);
        // And the payload therefore stays bounded: ESC header (7) +
        // base64 of 32 KiB (ceil(32768/3)*4 = 43692) + BEL (1).
        assert_eq!(osc52_payload(cut).len(), 7 + 43692 + 1);
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        // 32767 ASCII bytes, then a 3-byte CJK char straddling the cap:
        // the cut walks back below the boundary instead of splitting it.
        let mut text = "a".repeat(COPY_MAX_BYTES - 1);
        text.push_str("中中");
        assert!(text.len() > COPY_MAX_BYTES);
        let cut = truncate_for_copy(&text);
        assert!(cut.len() < COPY_MAX_BYTES, "walked back to a boundary");
        assert_eq!(cut, "a".repeat(COPY_MAX_BYTES - 1));
        // A text whose cap boundary falls INSIDE the second CJK char.
        let mut text = "a".repeat(COPY_MAX_BYTES - 2);
        text.push_str("中中");
        let cut = truncate_for_copy(&text);
        assert!(cut.ends_with(&"a".repeat(COPY_MAX_BYTES - 2)));
        assert!(!cut.contains('中'));
    }
}
