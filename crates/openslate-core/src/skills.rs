//! Agent Skills — Claude/Codex-style `SKILL.md` discovery (agentskills.io).
//!
//! A skill is a subdirectory containing a `SKILL.md` file with YAML
//! frontmatter (`name` + `description`, both required). At startup only the
//! name+description pairs are injected into every agent's system prompt
//! (progressive disclosure tier 1) via [`SkillsCatalog::catalog_prompt`];
//! skill bodies are loaded later by the `read_skill` tool.
//!
//! Discovery walks a list of source directories ordered LOW→HIGH precedence
//! (see [`discover_skills`]); later sources shadow earlier ones on name
//! collisions, and all problems are reported as [`SkillWarning`]s instead of
//! hard failures — a broken skill never blocks startup.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;

/// Directory names never considered as skill candidates.
const SKIPPED_DIRS: [&str; 2] = [".git", "node_modules"];

/// Description truncation limit (chars) applied when the catalog prompt
/// exceeds its character budget.
const MAX_DESCRIPTION_CHARS: usize = 160;

/// A single parsed skill (`{dir}/SKILL.md`).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Path to the SKILL.md file
    pub path: PathBuf,
    /// Directory containing the SKILL.md (for resolving relative resource paths)
    pub dir: PathBuf,
    /// Markdown body after frontmatter, trimmed
    pub body: String,
}

/// A warning-level problem found during skill discovery (never blocking).
#[derive(Debug, Clone)]
pub struct SkillWarning {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for SkillWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// A set of skills, sorted by name, produced by [`discover_skills`].
///
/// The `Default` value is the empty catalog.
#[derive(Debug, Clone, Default)]
pub struct SkillsCatalog {
    skills: Vec<Skill>, // sorted by name
}

/// Frontmatter fields deserialized from a `SKILL.md` YAML header.
///
/// Unknown fields are ignored (serde default) so extra metadata does not
/// break parsing.
#[derive(Debug, Clone, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

/// Parse a single `SKILL.md` (with YAML frontmatter) into a [`Skill`].
///
/// Mirrors [`crate::config::parse_agent_markdown`]: strips a UTF-8 BOM,
/// requires a leading `---`, splits at the first `\n---`, and trims the body
/// after the closing delimiter. `name` and `description` are required; a
/// missing field or malformed YAML yields [`ConfigError::ParseError`] with a
/// message prefixed by the file path.
pub fn parse_skill_markdown(content: &str, path: &Path) -> Result<Skill, ConfigError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let content = content.strip_prefix("---").ok_or_else(|| {
        ConfigError::ParseError(format!("{}: no frontmatter delimiter", path.display()))
    })?;

    let (yaml_str, body) = match content.find("\n---") {
        Some(pos) => {
            let yaml = &content[..pos];
            let body = &content[pos + "\n---".len()..];
            (yaml, body)
        }
        None => {
            return Err(ConfigError::ParseError(format!(
                "{}: unclosed frontmatter (missing closing ---)",
                path.display()
            )));
        }
    };

    let fm: SkillFrontmatter = serde_yml::from_str(yaml_str).map_err(|e| {
        ConfigError::ParseError(format!("{}: invalid frontmatter YAML: {e}", path.display()))
    })?;

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    Ok(Skill {
        name: fm.name,
        description: fm.description,
        path: path.to_path_buf(),
        dir,
        body: body.trim().to_owned(),
    })
}

/// Validate a skill name against the spec (warning-level, never blocking).
///
/// Rules: length 1..=64, only `[a-z0-9-]`, must not start or end with a
/// hyphen, no consecutive hyphens. Returns one concise message per violated
/// rule (empty vec when valid).
pub fn skill_name_warnings(name: &str) -> Vec<String> {
    let mut warnings = Vec::new();

    let char_count = name.chars().count();
    if char_count == 0 || char_count > 64 {
        warnings.push(format!(
            "skill name must be 1-64 characters long, got {char_count}"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        warnings
            .push("skill name may only contain lowercase letters, digits, and hyphens".to_owned());
    }
    if name.starts_with('-') || name.ends_with('-') {
        warnings.push("skill name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        warnings.push("skill name must not contain consecutive hyphens".to_owned());
    }

    warnings
}

/// Discover skills under `sources` (ordered LOW→HIGH precedence).
///
/// For each existing source directory, entries are visited sorted by file
/// name for determinism. Only directories count (symlinks followed, matching
/// Codex); `.git` and `node_modules` are skipped. A candidate skill is
/// `{entry}/SKILL.md` — entries without one are silently skipped, as are
/// missing source directories.
///
/// A parse failure records a warning and skips that skill; name-spec
/// violations and name/directory mismatches warn but still load. On a name
/// collision the later skill wins and a shadow warning is recorded (applies
/// across sources and within one source). The returned catalog is sorted by
/// name.
pub fn discover_skills(sources: &[PathBuf]) -> (SkillsCatalog, Vec<SkillWarning>) {
    let mut warnings = Vec::new();
    let mut skills: Vec<Skill> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for source in sources {
        let entries = match std::fs::read_dir(source) {
            Ok(entries) => entries,
            Err(_) => continue, // missing source dir: silently skipped
        };

        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            // Only directories (follows symlinks), skip VCS / deps dirs.
            let is_dir = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if !is_dir {
                continue;
            }
            let dir_name = entry.file_name();
            let dir_name = dir_name.to_string_lossy();
            if SKIPPED_DIRS.contains(&dir_name.as_ref()) {
                continue;
            }

            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue; // no SKILL.md: silently skip the entry
            }

            let content = match std::fs::read_to_string(&skill_md) {
                Ok(content) => content,
                Err(e) => {
                    warnings.push(SkillWarning {
                        path: skill_md,
                        message: format!("failed to read: {e}"),
                    });
                    continue;
                }
            };

            let skill = match parse_skill_markdown(&content, &skill_md) {
                Ok(skill) => skill,
                Err(e) => {
                    warnings.push(SkillWarning {
                        path: skill_md,
                        message: e.to_string(),
                    });
                    continue;
                }
            };

            // Warning-level name checks: the skill still loads.
            for message in skill_name_warnings(&skill.name) {
                warnings.push(SkillWarning {
                    path: skill_md.clone(),
                    message,
                });
            }
            if skill.name != dir_name {
                warnings.push(SkillWarning {
                    path: skill_md.clone(),
                    message: format!(
                        "skill name '{}' does not match directory name '{dir_name}'",
                        skill.name
                    ),
                });
            }

            // Name collision: later wins, warn about the shadowed one.
            match index.get(&skill.name).copied() {
                Some(pos) => {
                    let older_path = skills[pos].path.clone();
                    warnings.push(SkillWarning {
                        path: skill_md.clone(),
                        message: format!(
                            "skill '{}' from {} shadows {}",
                            skill.name,
                            skill_md.display(),
                            older_path.display()
                        ),
                    });
                    skills[pos] = skill;
                }
                None => {
                    index.insert(skill.name.clone(), skills.len());
                    skills.push(skill);
                }
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    (SkillsCatalog { skills }, warnings)
}

// ── Catalog prompt ───────────────────────────────────────────────────────────

/// XML-escape a description so skill metadata cannot inject markup into the
/// system prompt.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Truncate a description to [`MAX_DESCRIPTION_CHARS`] chars plus an ellipsis
/// (no-op when already within the limit). Operates on char boundaries.
fn truncate_description(desc: &str) -> String {
    let mut out: String = desc.chars().take(MAX_DESCRIPTION_CHARS).collect();
    if desc.chars().count() > MAX_DESCRIPTION_CHARS {
        out.push('…');
    }
    out
}

/// Render the skills catalog section: header paragraph, one
/// `- name:` / `  description:` pair per kept skill, and an optional
/// omission trailer.
fn render_catalog(kept: &[&Skill], descriptions: &[String], omission: Option<String>) -> String {
    let mut section = String::from(
        "# Skills\n\nThe following skills provide specialized instructions for specific tasks. \
When a task matches a skill's description, or the user explicitly names a skill, call the \
`read_skill` tool with the skill's name to load its full instructions before proceeding. Read \
the full skill instructions before acting; load a skill at most once per task.\n\n",
    );
    let mut lines: Vec<String> = kept
        .iter()
        .zip(descriptions)
        .map(|(skill, desc)| format!("- name: {}\n  description: {desc}", skill.name))
        .collect();
    if let Some(omission) = omission {
        lines.push(omission);
    }
    section.push_str(&lines.join("\n"));
    section
}

impl SkillsCatalog {
    /// All skills, sorted by name.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Whether no skills were discovered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Render the tier-1 (name+description) catalog section for system
    /// prompts. Returns `None` when the catalog is empty.
    ///
    /// When `max_list_chars > 0` and the section exceeds the budget:
    /// 1. every description is truncated to 160 chars + "…" and retried;
    /// 2. if still over, skills are dropped from the END (name-sorted) until
    ///    it fits, appending `- ({n} more skills omitted: ...)` (which itself
    ///    counts toward the budget and is dropped if even it cannot fit).
    ///
    /// `max_list_chars == 0` means unlimited.
    pub fn catalog_prompt(&self, max_list_chars: usize) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let fits = |s: &str| max_list_chars == 0 || s.chars().count() <= max_list_chars;

        // Full descriptions first.
        let full: Vec<String> = self
            .skills
            .iter()
            .map(|s| xml_escape(&s.description))
            .collect();
        let section = render_catalog(&self.skills.iter().collect::<Vec<_>>(), &full, None);
        if fits(&section) {
            return Some(section);
        }

        // Truncate every description and retry (with and without dropping).
        let truncated: Vec<String> = self
            .skills
            .iter()
            .map(|s| xml_escape(&truncate_description(&s.description)))
            .collect();
        let section = render_catalog(&self.skills.iter().collect::<Vec<_>>(), &truncated, None);
        if fits(&section) {
            return Some(section);
        }

        // Drop skills from the END (name-sorted) until it fits, with an
        // omission line that counts toward the budget.
        for keep in (0..self.skills.len()).rev() {
            let omitted: Vec<&str> = self.skills[keep..]
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            let omission = format!(
                "- ({} more skills omitted: {})",
                omitted.len(),
                omitted.join(", ")
            );
            let kept: Vec<&Skill> = self.skills[..keep].iter().collect();
            let section = render_catalog(&kept, &truncated[..keep], Some(omission));
            if fits(&section) {
                return Some(section);
            }
        }

        // Even the omission line never fits: drop it too and keep only as
        // many (name-sorted) skills as the budget allows.
        for keep in (0..self.skills.len()).rev() {
            let kept: Vec<&Skill> = self.skills[..keep].iter().collect();
            let section = render_catalog(&kept, &truncated[..keep], None);
            if fits(&section) {
                return Some(section);
            }
        }

        // Pathological budget (< header length): return the bare header.
        Some(render_catalog(&[], &[], None))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_skill(root: &Path, dir_name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, format!("---\n{frontmatter}---\n{body}")).unwrap();
        path
    }

    // ── parse_skill_markdown ─────────────────────────────────────────────

    #[test]
    fn parse_valid_skill_markdown() {
        let path = Path::new("/skills/pdf-processing/SKILL.md");
        let skill = parse_skill_markdown(
            "---\nname: pdf-processing\ndescription: Handle PDF files\n---\n\n## Usage\n\nDo the thing.\n",
            path,
        )
        .expect("should parse");
        assert_eq!(skill.name, "pdf-processing");
        assert_eq!(skill.description, "Handle PDF files");
        assert_eq!(skill.path, path);
        assert_eq!(skill.dir, Path::new("/skills/pdf-processing"));
        assert_eq!(skill.body, "## Usage\n\nDo the thing.");
    }

    #[test]
    fn parse_missing_description_is_error() {
        let result = parse_skill_markdown(
            "---\nname: pdf-processing\n---\nbody\n",
            Path::new("/skills/x/SKILL.md"),
        );
        let err = result.expect_err("missing description must fail");
        assert!(matches!(err, ConfigError::ParseError(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("/skills/x/SKILL.md"),
            "error must be path-prefixed: {msg}"
        );
        assert!(
            msg.contains("description"),
            "error must name the field: {msg}"
        );
    }

    #[test]
    fn parse_missing_name_is_error() {
        let result = parse_skill_markdown(
            "---\ndescription: no name\n---\nbody\n",
            Path::new("/skills/x/SKILL.md"),
        );
        let err = result.expect_err("missing name must fail");
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn parse_malformed_yaml_is_error() {
        let result = parse_skill_markdown(
            "---\nname: [broken\n---\nbody\n",
            Path::new("/skills/x/SKILL.md"),
        );
        let err = result.expect_err("malformed YAML must fail");
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("/skills/x/SKILL.md"));
    }

    #[test]
    fn parse_no_frontmatter_delimiter_is_error() {
        let result = parse_skill_markdown("just text\n", Path::new("/skills/x/SKILL.md"));
        let err = result.expect_err("missing delimiter must fail");
        assert!(err.to_string().contains("no frontmatter delimiter"));
    }

    #[test]
    fn parse_unclosed_frontmatter_is_error() {
        let result = parse_skill_markdown(
            "---\nname: x\ndescription: y\n",
            Path::new("/skills/x/SKILL.md"),
        );
        let err = result.expect_err("unclosed frontmatter must fail");
        assert!(err.to_string().contains("unclosed frontmatter"));
    }

    #[test]
    fn parse_strips_utf8_bom() {
        let content = "\u{feff}---\nname: bom\ndescription: bom skill\n---\nbody\n";
        let skill = parse_skill_markdown(content, Path::new("/skills/bom/SKILL.md"))
            .expect("BOM-prefixed content should parse");
        assert_eq!(skill.name, "bom");
        assert_eq!(skill.body, "body");
    }

    #[test]
    fn parse_unknown_fields_ignored() {
        let skill = parse_skill_markdown(
            "---\nname: x\ndescription: d\nmetadata: extra\nallowed-tools: [read_file]\n---\nb\n",
            Path::new("/skills/x/SKILL.md"),
        )
        .expect("unknown frontmatter fields are ignored");
        assert_eq!(skill.name, "x");
        assert_eq!(skill.description, "d");
    }

    #[test]
    fn parse_dir_falls_back_to_current() {
        let skill = parse_skill_markdown(
            "---\nname: x\ndescription: d\n---\nb\n",
            Path::new("SKILL.md"),
        )
        .expect("should parse");
        assert_eq!(skill.dir, Path::new("."));
    }

    // ── skill_name_warnings ──────────────────────────────────────────────

    #[test]
    fn skill_name_warnings_valid() {
        assert!(skill_name_warnings("pdf-processing").is_empty());
        assert!(skill_name_warnings("a").is_empty());
        assert!(skill_name_warnings("a1-b2").is_empty());
        assert!(skill_name_warnings(&"a".repeat(64)).is_empty());
    }

    #[test]
    fn skill_name_warnings_table() {
        // Uppercase → charset violation only.
        assert_eq!(skill_name_warnings("PDF").len(), 1);
        // Leading hyphen → start/end violation only.
        assert_eq!(skill_name_warnings("-pdf").len(), 1);
        // Consecutive hyphens.
        assert_eq!(skill_name_warnings("pdf--processing").len(), 1);
        // 65 chars → length violation only.
        assert_eq!(skill_name_warnings(&"a".repeat(65)).len(), 1);
        // Underscore → charset violation only.
        assert_eq!(skill_name_warnings("pdf_processing").len(), 1);
        // Empty → length violation only.
        assert_eq!(skill_name_warnings("").len(), 1);
        // Trailing hyphen.
        assert_eq!(skill_name_warnings("pdf-").len(), 1);
        // Multiple rules at once: too long AND uppercase.
        let warnings = skill_name_warnings(&"A".repeat(70));
        assert_eq!(warnings.len(), 2);
    }

    // ── discover_skills ──────────────────────────────────────────────────

    #[test]
    fn discover_from_single_source_sorted_by_name() {
        let root = TempDir::new().unwrap();
        write_skill(
            root.path(),
            "zeta",
            "name: zeta\ndescription: z\n",
            "z body",
        );
        write_skill(
            root.path(),
            "alpha",
            "name: alpha\ndescription: a\n",
            "a body",
        );

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty());
        let names: Vec<&str> = catalog.skills().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(catalog.get("alpha").unwrap().body, "a body");
        assert!(!catalog.is_empty());
    }

    #[test]
    fn discover_higher_precedence_source_shadows_lower() {
        let low = TempDir::new().unwrap();
        let high = TempDir::new().unwrap();
        let low_path = write_skill(
            low.path(),
            "pdf",
            "name: pdf\ndescription: low\n",
            "low body",
        );
        write_skill(
            high.path(),
            "pdf",
            "name: pdf\ndescription: high\n",
            "high body",
        );
        // A second, non-colliding skill in the low source.
        write_skill(low.path(), "ocr", "name: ocr\ndescription: o\n", "ocr body");

        let (catalog, warnings) =
            discover_skills(&[low.path().to_path_buf(), high.path().to_path_buf()]);
        assert_eq!(catalog.skills().len(), 2);
        assert_eq!(catalog.get("pdf").unwrap().description, "high");
        assert_eq!(catalog.get("ocr").unwrap().description, "o");

        assert_eq!(warnings.len(), 1, "exactly one shadow warning");
        let msg = warnings[0].to_string();
        assert!(
            msg.contains("skill 'pdf'"),
            "shadow warning names the skill: {msg}"
        );
        assert!(msg.contains("shadows"), "shadow warning wording: {msg}");
        assert!(msg.contains(low_path.display().to_string().as_str()));
    }

    #[test]
    fn discover_collision_within_one_source_shadows() {
        let root = TempDir::new().unwrap();
        // Two directories declaring the same skill name: "b" sorts after
        // "a", so b's skill wins regardless of directory naming.
        write_skill(
            root.path(),
            "a-first",
            "name: dup\ndescription: first\n",
            "1",
        );
        write_skill(
            root.path(),
            "b-second",
            "name: dup\ndescription: second\n",
            "2",
        );

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert_eq!(catalog.get("dup").unwrap().description, "second");
        assert!(warnings.iter().any(|w| w.message.contains("shadows")));
    }

    #[test]
    fn discover_skips_non_dirs_and_missing_skill_md() {
        let root = TempDir::new().unwrap();
        // A plain file entry (not a dir) — skipped.
        fs::write(root.path().join("plain.txt"), "hi").unwrap();
        // A dir without SKILL.md — silently skipped.
        fs::create_dir(root.path().join("no-skill-md")).unwrap();
        // Skipped dirs even if they contain a SKILL.md.
        write_skill(root.path(), ".git", "name: git\ndescription: g\n", "g");
        write_skill(
            root.path(),
            "node_modules",
            "name: nm\ndescription: n\n",
            "n",
        );
        // One real skill.
        write_skill(root.path(), "real", "name: real\ndescription: r\n", "r");

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty(), "no warnings, got: {warnings:?}");
        let names: Vec<&str> = catalog.skills().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn discover_parse_failure_warns_but_others_load() {
        let root = TempDir::new().unwrap();
        let bad_path = {
            let dir = root.path().join("bad");
            fs::create_dir_all(&dir).unwrap();
            let p = dir.join("SKILL.md");
            fs::write(&p, "name: bad\n(no closing delimiter\n").unwrap();
            p
        };
        write_skill(
            root.path(),
            "good",
            "name: good\ndescription: g\n",
            "g body",
        );

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(catalog.get("good").is_some(), "valid skill still loads");
        assert!(catalog.get("bad").is_none());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, bad_path);
        assert!(
            warnings[0].message.contains("bad"),
            "{}",
            warnings[0].message
        );
    }

    #[test]
    fn discover_name_spec_violation_warns_but_loads() {
        let root = TempDir::new().unwrap();
        write_skill(root.path(), "dir-name", "name: PDF!\ndescription: p\n", "p");

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(
            catalog.get("PDF!").is_some(),
            "spec-violating name still loads"
        );
        // One charset warning + one name/dir mismatch warning.
        assert!(warnings.iter().any(|w| w.message.contains("lowercase")));
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("does not match directory")),
            "expected mismatch warning, got: {warnings:?}"
        );
    }

    #[test]
    fn discover_name_dir_mismatch_warns_but_loads() {
        let root = TempDir::new().unwrap();
        write_skill(
            root.path(),
            "renamed-dir",
            "name: other-name\ndescription: d\n",
            "d",
        );

        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(catalog.get("other-name").is_some());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("does not match directory"));
        assert!(warnings[0]
            .to_string()
            .starts_with(warnings[0].path.display().to_string().as_str()));
    }

    #[test]
    fn discover_missing_source_dir_silently_skipped() {
        let (catalog, warnings) = discover_skills(&[PathBuf::from("/nonexistent/skills-dir")]);
        assert!(catalog.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn discover_empty_sources() {
        let (catalog, warnings) = discover_skills(&[]);
        assert!(catalog.is_empty());
        assert!(warnings.is_empty());
    }

    // ── catalog_prompt ───────────────────────────────────────────────────

    fn one_skill_catalog(name: &str, description: &str) -> SkillsCatalog {
        let root = TempDir::new().unwrap();
        write_skill(
            root.path(),
            name,
            &format!("name: {name}\ndescription: {description}\n"),
            "body",
        );
        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty());
        catalog
    }

    #[test]
    fn catalog_prompt_empty_is_none() {
        let catalog = SkillsCatalog::default();
        assert!(catalog.catalog_prompt(8000).is_none());
        assert!(catalog.catalog_prompt(0).is_none());
    }

    #[test]
    fn catalog_prompt_contains_names_and_descriptions() {
        let catalog = one_skill_catalog("pdf-processing", "Handle PDF files");
        let prompt = catalog
            .catalog_prompt(0)
            .expect("non-empty catalog renders");
        assert!(prompt.starts_with("# Skills\n"));
        assert!(prompt.contains("read_skill"));
        assert!(prompt.contains("- name: pdf-processing"));
        assert!(prompt.contains("  description: Handle PDF files"));
        assert!(prompt.ends_with("  description: Handle PDF files"));
    }

    #[test]
    fn catalog_prompt_escapes_xml_characters() {
        let catalog = one_skill_catalog("esc", "Use <tags> & entities");
        let prompt = catalog.catalog_prompt(0).expect("renders");
        assert!(prompt.contains("Use &lt;tags&gt; &amp; entities"));
        assert!(!prompt.contains("<tags>"));
    }

    #[test]
    fn catalog_prompt_budget_truncates_descriptions() {
        let long_desc = "x".repeat(500);
        let catalog = one_skill_catalog("long", &long_desc);
        // Budget: header + 200 chars — room for the truncated description
        // (161 chars) but not the full 500-char one.
        let unlimited = catalog.catalog_prompt(0).unwrap();
        let header_len = unlimited.chars().count() - (12 + 16 + 500);
        let prompt = catalog.catalog_prompt(header_len + 200).expect("renders");
        let desc = "x".repeat(160);
        assert!(
            prompt.contains(&format!("  description: {desc}…")),
            "description should be truncated to 160 chars + ellipsis"
        );
        assert!(!prompt.contains(&"x".repeat(200)));
        assert!(prompt.chars().count() <= header_len + 200);
    }

    #[test]
    fn catalog_prompt_budget_drops_skills_from_end_with_omission() {
        let root = TempDir::new().unwrap();
        let desc = "d".repeat(200);
        for name in ["a", "b", "c"] {
            write_skill(
                root.path(),
                name,
                &format!("name: {name}\ndescription: {desc}\n"),
                "x",
            );
        }
        let (catalog, warnings) = discover_skills(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty());

        // Full entry = 9 ("− name: a") + 16 + 200 = 225 chars; the unlimited
        // section is header + 3*225 + 2 joins. A budget of header + 300 fits
        // one truncated entry (186) + omission line but not two entries.
        let unlimited = catalog.catalog_prompt(0).unwrap();
        let header_len = unlimited.chars().count() - (3 * 225 + 2);
        let prompt = catalog.catalog_prompt(header_len + 300).expect("renders");
        assert!(
            prompt.contains("- name: a"),
            "first (name-sorted) skill kept"
        );
        assert!(
            prompt.contains("2 more skills omitted: b, c"),
            "expected omission line naming the dropped skills, got: {prompt}"
        );
        assert!(!prompt.contains("- name: b"), "dropped skill has no entry");
        assert!(!prompt.contains("- name: c"), "dropped skill has no entry");
        assert!(prompt.chars().count() <= header_len + 300);
    }

    #[test]
    fn catalog_prompt_budget_drops_omission_line_when_it_does_not_fit() {
        let root = TempDir::new().unwrap();
        let desc = "d".repeat(200);
        for name in ["a", "b"] {
            write_skill(
                root.path(),
                name,
                &format!("name: {name}\ndescription: {desc}\n"),
                "x",
            );
        }
        let (catalog, _) = discover_skills(&[root.path().to_path_buf()]);

        // Budget: header + 20. Neither entry (186 truncated) nor any
        // omission line fits, so the omission line itself is dropped and the
        // bare header is the only thing left that fits.
        let unlimited = catalog.catalog_prompt(0).unwrap();
        let header_len = unlimited.chars().count() - (2 * 225 + 1);
        let prompt = catalog.catalog_prompt(header_len + 20).expect("renders");
        assert!(prompt.starts_with("# Skills"));
        assert!(
            !prompt.contains("- name:"),
            "no entries fit in this budget, got: {prompt}"
        );
        assert!(
            !prompt.contains("omitted"),
            "omission line does not fit and must be dropped, got: {prompt}"
        );
    }

    #[test]
    fn catalog_prompt_tiny_budget_returns_bare_header() {
        let catalog = one_skill_catalog("tiny", "desc");
        // Smaller than the fixed header: still returns the bare-header
        // fallback rather than panicking or looping.
        let prompt = catalog.catalog_prompt(10).expect("renders something");
        assert!(prompt.starts_with("# Skills"));
        assert!(!prompt.contains("- name:"));
    }

    #[test]
    fn catalog_prompt_zero_means_unlimited() {
        let root = TempDir::new().unwrap();
        let long_desc = "y".repeat(10_000);
        write_skill(
            root.path(),
            "big",
            &format!("name: big\ndescription: {long_desc}\n"),
            "x",
        );
        let (catalog, _) = discover_skills(&[root.path().to_path_buf()]);
        let prompt = catalog.catalog_prompt(0).expect("renders");
        assert!(prompt.contains(&format!("description: {long_desc}")));
    }

    #[test]
    fn catalog_prompt_budget_boundary_fits_exactly() {
        let catalog = one_skill_catalog("edge", "d");
        let unlimited = catalog.catalog_prompt(0).unwrap();
        let exact = unlimited.chars().count();
        // Exactly at the budget → no truncation.
        let prompt = catalog
            .catalog_prompt(exact)
            .expect("exact budget must fit");
        assert_eq!(prompt, unlimited);
        // One below the budget → truncation kicks in (description "d" is
        // already short, so the omission path drops the skill instead).
        let tight = catalog.catalog_prompt(exact - 1).expect("renders");
        assert!(tight.chars().count() < exact);
    }

    // ── SkillWarning Display ─────────────────────────────────────────────

    #[test]
    fn skill_warning_display() {
        let w = SkillWarning {
            path: PathBuf::from("/a/b/SKILL.md"),
            message: "boom".to_owned(),
        };
        assert_eq!(w.to_string(), "/a/b/SKILL.md: boom");
    }
}
