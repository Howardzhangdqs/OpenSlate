//! `openslate skills` command — list discovered agent skills.
//!
//! Lightweight: takes the already-loaded config from main (default discovery
//! merges the user-global library underneath the active config; explicit
//! `--config` stays single-file — see `wiring::load_effective_config`),
//! discovers `SKILL.md` catalogs from the same sources the run wiring uses
//! (no sqlite, no providers, no MCP connections), and prints a table.
//! Discovery warnings go to stderr prefixed `WARN`; the listing goes to
//! stdout.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use openslate_core::config::OpenSlateConfig;
use openslate_core::skills::{discover_skills, SkillsCatalog};

/// Width cap for the description column (long first lines get ellipsized).
const MAX_DESCRIPTION_COLUMN: usize = 48;

/// The first line of a description, truncated for the table column.
fn first_description_line(description: &str) -> String {
    let first = description.lines().next().unwrap_or("").trim();
    if first.chars().count() > MAX_DESCRIPTION_COLUMN {
        let cut: String = first.chars().take(MAX_DESCRIPTION_COLUMN - 1).collect();
        format!("{cut}…")
    } else {
        first.to_owned()
    }
}

/// Render the skills listing for stdout: a `NAME  DESCRIPTION  PATH` table
/// when the catalog is non-empty, otherwise "No skills found." plus the
/// scanned directories (one per line) to aid debugging.
pub(crate) fn render_skills_listing(catalog: &SkillsCatalog, sources: &[PathBuf]) -> String {
    if catalog.is_empty() {
        let mut out = String::from("No skills found.\n\nScanned directories:\n");
        for source in sources {
            out.push_str(&format!("  {}\n", source.display()));
        }
        return out;
    }

    // Column widths count chars (not bytes) so non-ASCII names don't skew
    // the table; `{:<w$}` padding also counts chars for str.
    let name_w = catalog
        .skills()
        .iter()
        .map(|s| s.name.chars().count())
        .chain(std::iter::once("NAME".len()))
        .max()
        .unwrap_or(0);
    let desc_w = catalog
        .skills()
        .iter()
        .map(|s| first_description_line(&s.description).chars().count())
        .chain(std::iter::once("DESCRIPTION".len()))
        .max()
        .unwrap_or(0);

    let mut out = format!(
        "{:<name_w$}  {:<desc_w$}  PATH\n",
        "NAME",
        "DESCRIPTION",
        name_w = name_w,
        desc_w = desc_w,
    );
    for skill in catalog.skills() {
        out.push_str(&format!(
            "{:<name_w$}  {:<desc_w$}  {}\n",
            skill.name,
            first_description_line(&skill.description),
            skill.path.display(),
            name_w = name_w,
            desc_w = desc_w,
        ));
    }
    out
}

/// Run the skills command: load config, discover skills from the standard
/// sources, print warnings (stderr) and the listing (stdout).
/// Run the skills command: discover skills from the standard sources for the
/// (already loaded, possibly globally merged) `config`, print warnings
/// (stderr) and the listing (stdout). `config_path` locates the active
/// config dir for the skill discovery sources.
pub fn run_skills_command(config_path: &Path, config: OpenSlateConfig) -> Result<()> {
    if !config.skills.enabled {
        println!("Skills are disabled ([skills] enabled = false)");
        return Ok(());
    }

    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let sources = crate::wiring::skills_sources(config_path, &cwd);
    let (catalog, warnings) = discover_skills(&sources);

    for warning in &warnings {
        eprintln!("WARN {warning}");
    }
    print!("{}", render_skills_listing(&catalog, &sources));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Temp project with a valid config (skills enabled by default) and,
    /// optionally, skills under `.openslate/skills/`.
    fn temp_project_with_skills() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"
"#;
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        (tmp, openslate_dir)
    }

    fn write_skill(root: &Path, dir_name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).expect("create skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .expect("write SKILL.md");
    }

    #[test]
    fn test_listing_contains_name_and_path() {
        let (_tmp, openslate_dir) = temp_project_with_skills();
        let skills_dir = openslate_dir.join("skills");
        write_skill(
            &skills_dir,
            "demo-skill",
            "name: demo-skill\ndescription: Demo skill for tests\n",
            "demo body",
        );

        let (catalog, warnings) = discover_skills(std::slice::from_ref(&skills_dir));
        assert!(warnings.is_empty());
        let out = render_skills_listing(&catalog, std::slice::from_ref(&skills_dir));
        assert!(
            out.contains("demo-skill"),
            "listing should contain the skill name: {out}"
        );
        assert!(
            out.contains(
                skills_dir
                    .join("demo-skill")
                    .join("SKILL.md")
                    .display()
                    .to_string()
                    .as_str()
            ),
            "listing should contain the SKILL.md path: {out}"
        );
        assert!(out.contains("Demo skill for tests"), "{out}");
    }

    #[test]
    fn test_empty_listing_lists_scanned_dirs() {
        let sources = vec![
            PathBuf::from("/home/u/.agents/skills"),
            PathBuf::from("/project/.openslate/skills"),
        ];
        let out = render_skills_listing(&SkillsCatalog::default(), &sources);
        assert!(out.contains("No skills found."), "{out}");
        assert!(out.contains("/home/u/.agents/skills"), "{out}");
        assert!(out.contains("/project/.openslate/skills"), "{out}");
    }

    #[test]
    fn test_disabled_config_exits_ok() {
        let (_tmp, openslate_dir) = temp_project_with_skills();
        let skills_dir = openslate_dir.join("skills");
        write_skill(
            &skills_dir,
            "demo-skill",
            "name: demo-skill\ndescription: d\n",
            "b",
        );
        let toml_path = openslate_dir.join("openslate.toml");
        let base = fs::read_to_string(&toml_path).expect("read toml");
        fs::write(&toml_path, format!("{base}\n[skills]\nenabled = false\n")).expect("write");

        // Prints the disabled notice and exits 0 — even with skills present.
        let config = crate::wiring::load_config(&toml_path).expect("load config");
        let result = run_skills_command(&toml_path, config);
        assert!(
            result.is_ok(),
            "disabled skills is not an error: {result:?}"
        );
    }

    #[test]
    fn test_bad_skill_warns_but_command_succeeds() {
        let (_tmp, openslate_dir) = temp_project_with_skills();
        let skills_dir = openslate_dir.join("skills");
        write_skill(
            &skills_dir,
            "good",
            "name: good\ndescription: g\n",
            "good body",
        );
        let bad_dir = skills_dir.join("bad");
        fs::create_dir_all(&bad_dir).expect("create bad dir");
        fs::write(bad_dir.join("SKILL.md"), "name: bad\n(no closing ---\n").expect("write bad");

        let toml_path = openslate_dir.join("openslate.toml");
        let config = crate::wiring::load_config(&toml_path).expect("load config");
        let result = run_skills_command(&toml_path, config);
        assert!(
            result.is_ok(),
            "a broken skill must not fail the command: {result:?}"
        );
    }

    #[test]
    fn test_first_description_line_takes_first_line_and_truncates() {
        assert_eq!(first_description_line("first\nsecond"), "first");
        assert_eq!(first_description_line("  padded  "), "padded");
        let long = "x".repeat(80);
        let truncated = first_description_line(&long);
        assert_eq!(truncated.chars().count(), MAX_DESCRIPTION_COLUMN);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn test_listing_width_counts_chars_not_bytes() {
        // A non-ASCII (spec-violating but loaded) name: the name column is
        // sized by char count, so the row is name + the standard 2-space
        // separator, not byte-padded with extra spaces.
        let root = TempDir::new().expect("temp dir");
        write_skill(
            root.path(),
            "nonascii",
            "name: 日本語語語語\ndescription: d\n",
            "b",
        );
        let (catalog, _) = discover_skills(&[root.path().to_path_buf()]);
        let out = render_skills_listing(&catalog, &[]);
        assert!(
            out.contains("日本語語語語  d"),
            "char-based width, no byte over-padding: {out}"
        );
        assert!(!out.contains("日本語語語語   d"), "{out}");
    }
}
