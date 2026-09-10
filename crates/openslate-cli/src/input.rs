//! Input processing utilities for openslate-cli.
//!
//! Provides:
//! - `@file` reference expansion (security-checked)
//! - Stdin pipe detection and reading
//! - Multi-line input accumulation (REPL)

use anyhow::{Context, Result};
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

/// Workspace root for security checks.
///
/// Derived from the config directory (parent of `.openslate/`).
#[derive(Debug, Clone)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Create a WorkspaceRoot from a config file path (e.g., `/project/.openslate/openslate.toml`).
    pub fn from_config_path(config_path: &Path) -> Self {
        // Workspace root is the parent of `.openslate/` directory
        let root = config_path
            .parent() // .openslate/
            .and_then(|p| p.parent()) // project/
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| config_path.parent().unwrap().to_path_buf());
        Self(root)
    }

    /// Get the workspace root path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Check if a path is within the workspace root.
    /// Returns the canonicalized path if valid, or an error if outside workspace.
    ///
    /// Both the target and the root are canonicalized (resolving `..` and
    /// symlinks) before comparison, so lexical escapes like
    /// `<root>/../../etc/passwd` cannot slip past the prefix check. The
    /// target must exist: `@file` references are only ever read, and a path
    /// that cannot be canonicalized (missing file, broken symlink) cannot be
    /// read either — reject it instead of falling back to a lexical check.
    pub fn validate_path(&self, target_path: &Path) -> Result<PathBuf> {
        // Relative paths resolve from the workspace root; absolute paths
        // are taken as-is. Either way the result must canonicalize.
        let joined = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            self.0.join(target_path)
        };

        let canonical = joined.canonicalize().with_context(|| {
            format!(
                "Path '{}' cannot be resolved within workspace root '{}' (does it exist?)",
                target_path.display(),
                self.0.display()
            )
        })?;

        // Canonicalize the root too, so the containment check compares
        // fully resolved paths on both sides.
        let root_canonical = self
            .0
            .canonicalize()
            .with_context(|| format!("Workspace root '{}' cannot be resolved", self.0.display()))?;

        if canonical.starts_with(&root_canonical) {
            Ok(canonical)
        } else {
            anyhow::bail!(
                "Path '{}' is outside workspace root '{}'",
                target_path.display(),
                self.0.display()
            );
        }
    }
}

/// Read all stdin content if it's a pipe (not a terminal).
/// Returns None if stdin is a terminal.
pub fn read_stdin_if_pipe() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    match io::stdin().read_to_string(&mut buffer) {
        Ok(_) => Some(buffer),
        Err(_) => None,
    }
}

/// Expand `@path` tokens in input text with file contents.
///
/// Security: only allows files within workspace root.
///
/// - Valid files: replaced with `<file path="...">\n{content}\n</file>`
/// - Nonexistent files: prints warning, leaves `@path` as-is
/// - Paths outside workspace: security error, left as-is with warning
pub fn expand_at_files(input: &str, workspace_root: &WorkspaceRoot) -> String {
    let mut result = String::with_capacity(input.len());
    let mut remaining = input;

    while let Some(at_pos) = remaining.find('@') {
        // Append everything before the @
        result.push_str(&remaining[..at_pos]);
        remaining = &remaining[at_pos + 1..]; // Skip the @

        // Find the end of the path (whitespace or end of string)
        let end_pos = remaining
            .find(|c: char| c.is_whitespace())
            .unwrap_or(remaining.len());

        let path_str = &remaining[..end_pos];
        let after_path = &remaining[end_pos..];

        if path_str.is_empty() {
            // Double @@ - literal @
            result.push('@');
            remaining = after_path;
            continue;
        }

        let target_path = Path::new(path_str);

        // Try to read the file
        match workspace_root.validate_path(target_path) {
            Ok(canonical_path) => {
                match fs::read_to_string(&canonical_path) {
                    Ok(content) => {
                        // Success: wrap content in markers
                        let escaped_path = canonical_path.display();
                        result.push_str(&format!(
                            "<file path=\"{}\">\n{}\n</file>",
                            escaped_path, content
                        ));
                    }
                    Err(_) => {
                        // File exists but can't be read - leave as-is with warning
                        eprintln!(
                            "Warning: could not read file '{}': leaving @ reference as-is",
                            path_str
                        );
                        result.push('@');
                        result.push_str(path_str);
                    }
                }
            }
            Err(_) => {
                // Outside workspace or unresolvable (e.g. nonexistent) —
                // leave as-is with a warning
                eprintln!(
                    "Warning: path '{}' is not accessible within workspace root '{}': leaving @ reference as-is",
                    path_str,
                    workspace_root.path().display()
                );
                result.push('@');
                result.push_str(path_str);
            }
        }

        remaining = after_path;
    }

    // Append any remaining content
    result.push_str(remaining);

    result
}

/// Accumulate multi-line input until a line doesn't end with backslash.
///
/// - Lines ending with `\` are joined with `\n` and continue
/// - Trailing `\` is stripped from joined lines
/// - Returns the accumulated input (without trailing backslashes per segment)
#[allow(dead_code)]
#[allow(clippy::manual_strip)]
pub fn accumulate_multiline(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let mut result_lines: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if line.ends_with('\\') {
            // Continuation line - strip trailing \ (and any trailing space before it)
            let content = if line.len() >= 2 && line.ends_with('\\') {
                // Check if there's a space before the backslash
                let stripped = &line[..line.len() - 1];
                stripped.trim_end()
            } else {
                &line[..line.len() - 1]
            };
            result_lines.push(content.to_string());
        } else {
            result_lines.push(line.to_string());
        }
        i += 1;
    }

    result_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn temp_project() -> (TempDir, WorkspaceRoot) {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "test-model-v1"

[limits]
max_steps = 10
"#;
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md =
            "---\nid: root\nname: Root Agent\nmodel: main\n---\nYou are the root agent.\n";
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");

        let workspace_root = WorkspaceRoot::from_config_path(&openslate_dir.join("openslate.toml"));
        (tmp, workspace_root)
    }

    // ── WorkspaceRoot tests ──

    #[test]
    fn test_workspace_root_from_config_path() {
        let (tmp, root) = temp_project();
        let path = root.path();
        // Should be the temp dir, not .openslate
        assert!(path.exists());
        // Verify .openslate is a child of the workspace root
        assert!(path.join(".openslate").exists());
        // Ensure the root equals the temp dir path
        assert_eq!(path, tmp.path());
    }

    #[test]
    fn test_validate_path_inside_workspace() {
        // Keep `tmp` alive: the target must exist for canonicalize to
        // succeed (dropping the TempDir would delete the file).
        let (tmp, root) = temp_project();
        let config_file = tmp.path().join(".openslate").join("openslate.toml");
        let result = root.validate_path(&config_file);
        assert!(result.is_ok(), "config file should be inside workspace");
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let (_, root) = temp_project();
        let outside_path = Path::new("/etc/passwd");
        let result = root.validate_path(outside_path);
        assert!(
            result.is_err(),
            "paths outside workspace should be rejected"
        );
    }

    #[test]
    fn test_validate_path_relative_inside() {
        // Keep `tmp` alive: the target must exist for canonicalize to succeed.
        let (_tmp, root) = temp_project();
        let relative = Path::new(".openslate/openslate.toml");
        let result = root.validate_path(relative);
        assert!(
            result.is_ok(),
            "relative path inside workspace should be valid"
        );
    }

    #[test]
    fn test_validate_path_traversal_escape_rejected() {
        let (tmp, root) = temp_project();
        // A real file OUTSIDE the workspace root, referenced through `..`:
        // the lexical form starts with the root, but resolves outside it.
        let outside = tempfile::TempDir::new().expect("create outside temp dir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("write outside file");

        let outside_dir_name = outside.path().file_name().unwrap().to_str().unwrap();
        let traversal = tmp
            .path()
            .join("..")
            .join(outside_dir_name)
            .join("secret.txt");
        assert!(
            root.validate_path(&traversal).is_err(),
            "`..` traversal pointing outside workspace must be rejected"
        );
    }

    #[test]
    fn test_validate_path_traversal_staying_inside_accepted() {
        let (tmp, root) = temp_project();
        // `..` segments that still resolve INSIDE the root are fine.
        let zigzag = tmp
            .path()
            .join(".openslate")
            .join("..")
            .join(".openslate")
            .join("openslate.toml");
        let result = root.validate_path(&zigzag);
        assert!(
            result.is_ok(),
            "`..` segments resolving inside workspace should be accepted"
        );
        assert_eq!(
            result.unwrap(),
            tmp.path()
                .join(".openslate")
                .join("openslate.toml")
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn test_validate_path_nonexistent_rejected() {
        let (tmp, root) = temp_project();
        assert!(
            root.validate_path(Path::new("no_such_file.txt")).is_err(),
            "nonexistent relative path must be rejected"
        );
        assert!(
            root.validate_path(&tmp.path().join("no_such_file.txt"))
                .is_err(),
            "nonexistent absolute path must be rejected"
        );
    }

    // ── expand_at_files tests ──

    #[test]
    fn test_expand_at_files_valid() {
        let (tmp, root) = temp_project();

        // Create a test file
        let test_file = tmp.path().join("test.txt");
        fs::write(&test_file, "file content here").expect("write test file");

        let input = format!(
            "Hello @{} and more",
            test_file.file_name().unwrap().to_str().unwrap()
        );
        let result = expand_at_files(&input, &root);

        assert!(result.contains("<file path="));
        assert!(result.contains("file content here"));
    }

    #[test]
    fn test_expand_at_files_nonexistent() {
        let (_, root) = temp_project();

        let input = "Hello @nonexistent.txt world";
        let result = expand_at_files(input, &root);

        // Should leave @nonexistent.txt as-is
        assert!(result.contains("@nonexistent.txt"));
    }

    #[test]
    fn test_expand_at_files_outside_workspace() {
        let (_, root) = temp_project();

        let input = "Hello @/etc/passwd world";
        let result = expand_at_files(input, &root);

        // Should leave @/etc/passwd as-is with warning
        assert!(result.contains("@/etc/passwd"));
    }

    #[test]
    fn test_expand_at_files_double_at() {
        let (_, root) = temp_project();

        // @@ should be treated as literal @
        let input = "Hello @@world";
        let result = expand_at_files(input, &root);
        assert!(result.contains("@world"));
    }

    #[test]
    fn test_expand_at_files_no_at() {
        let (_, root) = temp_project();

        let input = "Hello world";
        let result = expand_at_files(input, &root);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_expand_at_files_multiple() {
        let (tmp, root) = temp_project();

        let file1 = tmp.path().join("file1.txt");
        let file2 = tmp.path().join("file2.txt");
        fs::write(&file1, "content1").expect("write file1");
        fs::write(&file2, "content2").expect("write file2");

        let input = format!(
            "@{} and @{} together",
            file1.file_name().unwrap().to_str().unwrap(),
            file2.file_name().unwrap().to_str().unwrap()
        );
        let result = expand_at_files(&input, &root);

        assert!(result.contains("content1"));
        assert!(result.contains("content2"));
    }

    // ── accumulate_multiline tests ──

    #[test]
    fn test_multiline_with_continuation() {
        let input = "line1 \\\nline2 \\\nline3";
        let result = accumulate_multiline(input);
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn test_multiline_without_continuation() {
        let input = "line1\nline2\nline3";
        let result = accumulate_multiline(input);
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn test_multiline_single_line_no_backslash() {
        let input = "single line";
        let result = accumulate_multiline(input);
        assert_eq!(result, "single line");
    }

    #[test]
    fn test_multiline_strips_trailing_backslash() {
        let input = "line1 \\";
        let result = accumulate_multiline(input);
        assert_eq!(result, "line1");
    }

    #[test]
    fn test_multiline_mixed() {
        let input = "line1 \\\nline2\nline3 \\\nline4";
        let result = accumulate_multiline(input);
        assert_eq!(result, "line1\nline2\nline3\nline4");
    }

    #[test]
    fn test_multiline_empty_lines() {
        let input = "line1 \\\n\nline2";
        let result = accumulate_multiline(input);
        assert_eq!(result, "line1\n\nline2");
    }
}
