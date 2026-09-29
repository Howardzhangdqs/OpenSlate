//! Clipboard — the copy-out chain (fix-13 OSC 52 + copy-1 fallbacks).
//!
//! In-app copy that bypasses terminal text selection entirely: the
//! source text is offered over THREE environment-independent paths —
//!
//! 1. **OSC 52** (`ESC ] 52 ; c ; <base64> BEL`, written to the real
//!    stdout): terminals (and tmux with `set-clipboard on`) forward it
//!    to the system clipboard, so it works through SSH — but many
//!    outer terminals REFUSE the sequence, so a sink success only
//!    means "sent", never "received";
//! 2. **copy file** (`<data dir>/last-copy.md`): the one path
//!    guaranteed to work on every machine (file落盘 always succeeds);
//! 3. **clipboard tool** (wl-copy/xclip/xsel/pbcopy/clip.exe when one
//!    exists on PATH, fed the full text on stdin).
//!
//! No new dependencies: base64 is hand-rolled below (standard RFC 4648
//! alphabet with `=` padding — what OSC 52 receivers expect), and the
//! OSC 52 write goes through a thin [`ClipboardSink`] seam whose
//! production implementation performs the same write+flush on the REAL
//! stdout that `crossterm::execute!` does for the mouse-capture toggle
//! (NOT the ratatui backend). The terminal consumes the sequence
//! immediately — the next diff-based draw is unaffected.

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

// ── Path 2: the copy file (copy-1) ──────────────────────────────────────

/// The copy file's name inside the data dir. `.md` because the copied
/// material is the assistant's raw markdown (or a transcript rendering
/// of it) — a file the user can paste from with any editor.
const COPY_FILE_NAME: &str = "last-copy.md";

/// Write `text` to `dir/last-copy.md`, creating parents as needed and
/// OVERWRITING any previous copy. Returns the file path (the caller
/// reports it in the notice — the one copy path that always works).
pub fn write_copy_file(dir: &std::path::Path, text: &str) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(COPY_FILE_NAME);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Production copy-file target: `<data dir>/last-copy.md`.
fn openslate_data_dir() -> std::path::PathBuf {
    // SAME resolution as main.rs's `open_log_file` (the tui.log dir
    // minus the trailing `logs` segment): dirs' XDG data dir with a
    // home fallback — `~/.local/share/openslate/` on Linux.
    dirs::data_dir()
        .map(|d| d.join("openslate"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/openslate")))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share/openslate"))
}

/// [`write_copy_file`] into the user data dir (the tui.log dir's
/// parent — `~/.local/share/openslate/last-copy.md`).
pub fn copy_file_default(text: &str) -> std::io::Result<std::path::PathBuf> {
    write_copy_file(&openslate_data_dir(), text)
}

// ── Path 3: a detected clipboard tool (copy-1) ─────────────────────────

/// Candidate tools in priority order: (binary name, fixed arguments).
/// `xclip`/`xsel` need their selection flags to target the system
/// clipboard (not the primary selection); the others read plain stdin.
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("pbcopy", &[]),
    ("clip.exe", &[]),
];

/// Scan a `PATH`-style value for the first candidate tool (tool
/// priority is OUTER: every dir is checked for wl-copy before xclip is
/// considered). A dir ENTRY named like a tool but not a regular file
/// (e.g. a directory) does not match. Pure function over the value so
/// tests can inject their own dirs without touching the process env.
fn find_tool_in_path(path_value: &std::ffi::OsStr) -> Option<&'static str> {
    for (tool, _) in CLIPBOARD_TOOLS {
        for dir in std::env::split_paths(path_value) {
            if dir.join(tool).is_file() {
                return Some(tool);
            }
        }
    }
    None
}

/// The clipboard tool usable on this machine (highest priority hit on
/// PATH), resolved once per process ([`OnceLock`] — the scan walks
/// every PATH dir for five names). `None` when no candidate exists:
/// the copy chain then runs without the tool leg.
pub fn detect_clipboard_tool() -> Option<&'static str> {
    static DETECTED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| std::env::var_os("PATH").and_then(|p| find_tool_in_path(&p)))
}

/// Feed `text` to `tool` on stdin (spawn + write-all + wait). Returns
/// whether the tool exited successfully — a failed spawn (missing
/// binary, headless X, …) is a plain `false`: this is one leg of the
/// fallback chain and must never panic. `tool` may be an absolute
/// path (tests); unknown names get no extra arguments.
pub fn run_clipboard_tool(tool: &str, text: &str) -> bool {
    let args: &[&str] = match tool {
        "xclip" => &["-selection", "clipboard"],
        "xsel" => &["--clipboard", "--input"],
        _ => &[],
    };
    let mut child = match std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    // Write the full text, then let the pipe CLOSE (drop the handle)
    // so readers see EOF before wait().
    let wrote = child
        .stdin
        .take()
        .map(|mut stdin| {
            use std::io::Write;
            stdin.write_all(text.as_bytes()).is_ok()
        })
        .unwrap_or(false);
    match child.wait() {
        Ok(status) => wrote && status.success(),
        Err(_) => false,
    }
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

    // ── copy-1: the copy file leg ───────────────────────────────────

    #[test]
    fn write_copy_file_creates_parents_and_overwrites() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("nested/copy");
        let path = write_copy_file(&dir, "first").expect("write creates parents");
        assert_eq!(path, dir.join("last-copy.md"), "file name is fixed");
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "first");
        // Second write REPLACES (no append, no stale bytes).
        write_copy_file(&dir, "second你好").expect("write overwrites");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "second你好"
        );
    }

    #[test]
    fn openslate_data_dir_is_absolute_and_openslate_named() {
        // Same root the tui.log lives under (`~/.local/share/openslate`):
        // absolute in any resolvable environment, last segment `openslate`.
        let dir = openslate_data_dir();
        assert!(dir.is_absolute(), "data dir is absolute: {}", dir.display());
        assert_eq!(
            dir.file_name().and_then(std::ffi::OsStr::to_str),
            Some("openslate")
        );
    }

    // ── copy-1: the clipboard-tool leg ──────────────────────────────

    #[test]
    fn find_tool_in_path_none_in_empty_dir_and_skips_non_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(find_tool_in_path(tmp.path().as_os_str()), None);
        // A DIRECTORY named like a tool is not an executable file.
        std::fs::create_dir(tmp.path().join("wl-copy")).expect("dir named wl-copy");
        assert_eq!(
            find_tool_in_path(tmp.path().as_os_str()),
            None,
            "directories never match"
        );
    }

    #[test]
    fn find_tool_in_path_tool_priority_beats_dir_order() {
        // xclip sits in the FIRST dir, wl-copy in the SECOND — the
        // outer tool loop wins: wl-copy is returned regardless.
        let xclip_dir = tempfile::tempdir().expect("xclip dir");
        std::fs::write(xclip_dir.path().join("xclip"), "#!/bin/sh\n").expect("xclip");
        let wl_dir = tempfile::tempdir().expect("wl dir");
        std::fs::write(wl_dir.path().join("wl-copy"), "#!/bin/sh\n").expect("wl-copy");
        let paths = std::env::join_paths([xclip_dir.path(), wl_dir.path()]).expect("join");
        assert_eq!(find_tool_in_path(paths.as_os_str()), Some("wl-copy"));
        // Without wl-copy the first-dir xclip is found.
        let only_xclip = std::env::join_paths([xclip_dir.path()]).expect("join");
        assert_eq!(find_tool_in_path(only_xclip.as_os_str()), Some("xclip"));
    }

    #[test]
    fn detect_clipboard_tool_is_cached_and_total() {
        // The OnceLock cache: repeated calls return the same answer
        // (whatever the real PATH resolves to — `None` on this
        // tool-less dev machine).
        assert_eq!(detect_clipboard_tool(), detect_clipboard_tool());
    }

    /// A real round-trip through a stub "tool": the runner feeds the
    /// FULL text on stdin and reports the exit status.
    #[cfg(unix)]
    #[test]
    fn run_clipboard_tool_pipes_full_stdin_and_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("out.txt");
        let script = tmp.path().join("stub-copy");
        std::fs::write(&script, format!("#!/bin/sh\ncat > {}\n", out.display()))
            .expect("write script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let text = "hello 你好\nsecond line";
        assert!(
            run_clipboard_tool(script.to_str().expect("utf8 path"), text),
            "stub tool exits 0"
        );
        assert_eq!(
            std::fs::read_to_string(&out).expect("read stub output"),
            text,
            "full text piped to stdin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_clipboard_tool_reports_failing_tools_as_false() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("stub-fail");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").expect("write script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(!run_clipboard_tool(
            script.to_str().expect("utf8 path"),
            "text"
        ));
    }

    #[test]
    fn run_clipboard_tool_missing_binary_is_false() {
        assert!(
            !run_clipboard_tool("/nonexistent/openslate-no-such-tool", "text"),
            "spawn failure is a silent false"
        );
    }
}
