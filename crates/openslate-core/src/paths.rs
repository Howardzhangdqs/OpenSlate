use std::path::{Path, PathBuf};

/// Resolved paths for OpenSlate configuration and data.
#[derive(Debug, Clone)]
pub struct OpenSlatePaths {
    /// Project-local config directory (CWD/.openslate/), if it exists.
    pub local_config_dir: Option<PathBuf>,
    /// User-global config directory (~/.config/openslate/).
    pub global_config_dir: PathBuf,
    /// User-global data directory (~/.local/share/openslate/).
    pub global_data_dir: PathBuf,
    /// Active config directory (local if exists, else global).
    pub active_config_dir: PathBuf,
    /// Active data directory.
    pub active_data_dir: PathBuf,
    /// Path to openslate.toml.
    pub config_file: PathBuf,
    /// Path to agents directory.
    pub agents_dir: PathBuf,
    /// Path to SQLite database.
    pub database_path: PathBuf,
    /// Path to prompts directory.
    pub prompts_dir: PathBuf,
}

/// Get the global config directory, respecting XDG_CONFIG_HOME env var.
fn get_global_config_dir() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config_home).join("openslate")
    } else {
        dirs::home_dir()
            .map(|h| h.join(".config").join("openslate"))
            .unwrap_or_else(|| PathBuf::from("~/.config/openslate"))
    }
}

/// Get the global data directory, respecting XDG_DATA_HOME env var.
fn get_global_data_dir() -> PathBuf {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data_home).join("openslate")
    } else {
        dirs::home_dir()
            .map(|h| h.join(".local").join("share").join("openslate"))
            .unwrap_or_else(|| PathBuf::from("~/.local/share/openslate"))
    }
}

/// Resolve all OpenSlate paths based on the current working directory.
///
/// The resolution logic is:
/// 1. Detect `.openslate/` in CWD → local_config_dir
/// 2. Use `dirs` crate for global paths (respecting XDG env vars)
/// 3. Active config = local if exists, else global
/// 4. Database path prefers local over global
/// 5. Config file: `{active_config_dir}/openslate.toml`
/// 6. Agents dir: `{active_config_dir}/agents/`
/// 7. Prompts dir: `{active_config_dir}/prompts/`
pub fn resolve_paths(cwd: &Path) -> OpenSlatePaths {
    let local_config_dir = cwd.join(".openslate");

    let global_config_dir = get_global_config_dir();
    let global_data_dir = get_global_data_dir();

    // Active config is local if .openslate/ exists in CWD
    let active_config_dir = if local_config_dir.exists() {
        local_config_dir.clone()
    } else {
        global_config_dir.clone()
    };

    // Database path: always use global data dir (never pollute project directory).
    // Per-project data is distinguished by the `cwd` column in the runs table.
    let database_path = global_data_dir.join("openslate.sqlite");

    let config_file = active_config_dir.join("openslate.toml");
    let agents_dir = active_config_dir.join("agents");
    let prompts_dir = active_config_dir.join("prompts");

    let active_data_dir = global_data_dir.clone();

    OpenSlatePaths {
        local_config_dir: if local_config_dir.exists() {
            Some(local_config_dir)
        } else {
            None
        },
        global_config_dir,
        global_data_dir,
        active_config_dir,
        active_data_dir,
        config_file,
        agents_dir,
        database_path,
        prompts_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_default_xdg_paths_without_local_config() {
        // Create a temp dir without .openslate/
        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();

        let paths = resolve_paths(cwd);

        // local_config_dir should be None since .openslate/ doesn't exist
        assert!(paths.local_config_dir.is_none());

        // global paths should be set
        let home = dirs::home_dir().expect("home dir should exist");
        let expected_global_config = if std::env::var("XDG_CONFIG_HOME").is_ok() {
            PathBuf::from(std::env::var("XDG_CONFIG_HOME").unwrap()).join("openslate")
        } else {
            home.join(".config").join("openslate")
        };
        let expected_global_data = if std::env::var("XDG_DATA_HOME").is_ok() {
            PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap()).join("openslate")
        } else {
            home.join(".local").join("share").join("openslate")
        };

        assert_eq!(paths.global_config_dir, expected_global_config);
        assert_eq!(paths.global_data_dir, expected_global_data);

        // active_config_dir should be global since no local
        assert_eq!(paths.active_config_dir, expected_global_config);

        // config and agents files should be in active config dir
        assert_eq!(paths.config_file, expected_global_config.join("openslate.toml"));
        assert_eq!(paths.agents_dir, expected_global_config.join("agents"));
        assert_eq!(paths.prompts_dir, expected_global_config.join("prompts"));

        // database should be in global data dir
        assert_eq!(paths.database_path, expected_global_data.join("openslate.sqlite"));
    }

    #[test]
    fn test_local_config_exists_uses_local() {
        // Create a temp dir with .openslate/
        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();
        let local_dir = cwd.join(".openslate");
        fs::create_dir(&local_dir).unwrap();

        let paths = resolve_paths(cwd);

        // local_config_dir should be Some
        assert_eq!(paths.local_config_dir, Some(local_dir.clone()));

        // active_config_dir should be local
        assert_eq!(paths.active_config_dir, local_dir);

        // config and agents files should be in local dir
        assert_eq!(paths.config_file, local_dir.join("openslate.toml"));
        assert_eq!(paths.agents_dir, local_dir.join("agents"));
        assert_eq!(paths.prompts_dir, local_dir.join("prompts"));
    }

    #[test]
    fn test_database_always_global_even_with_local_config() {
        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();
        let local_dir = cwd.join(".openslate");
        fs::create_dir(&local_dir).unwrap();

        let paths = resolve_paths(cwd);

        let home = dirs::home_dir().expect("home dir should exist");
        let expected_global_data = if std::env::var("XDG_DATA_HOME").is_ok() {
            PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap()).join("openslate")
        } else {
            home.join(".local").join("share").join("openslate")
        };

        assert_eq!(paths.database_path, expected_global_data.join("openslate.sqlite"));
    }

    #[test]
    fn test_database_always_global_without_local_config() {
        let temp_dir = TempDir::new().unwrap();
        let cwd = temp_dir.path();

        let paths = resolve_paths(cwd);

        let home = dirs::home_dir().expect("home dir should exist");
        let expected_global_data = if std::env::var("XDG_DATA_HOME").is_ok() {
            PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap()).join("openslate")
        } else {
            home.join(".local").join("share").join("openslate")
        };

        assert_eq!(paths.database_path, expected_global_data.join("openslate.sqlite"));
    }

    #[test]
    fn test_paths_struct_has_correct_fields() {
        let temp_dir = TempDir::new().unwrap();
        let paths = resolve_paths(temp_dir.path());

        // Verify all fields are populated
        assert!(paths.local_config_dir.is_none()); // no .openslate in temp
        assert!(!paths.global_config_dir.as_os_str().is_empty());
        assert!(!paths.global_data_dir.as_os_str().is_empty());
        assert!(!paths.active_config_dir.as_os_str().is_empty());
        assert!(!paths.active_data_dir.as_os_str().is_empty());
        assert!(!paths.config_file.as_os_str().is_empty());
        assert!(!paths.agents_dir.as_os_str().is_empty());
        assert!(!paths.database_path.as_os_str().is_empty());
        assert!(!paths.prompts_dir.as_os_str().is_empty());

        // Verify file paths have correct extensions
        assert_eq!(paths.config_file.extension().unwrap(), "toml");
        assert_eq!(paths.agents_dir.components().last().unwrap(), std::path::Component::Normal("agents".as_ref()));
        assert_eq!(paths.database_path.extension().unwrap(), "sqlite");
    }
}
