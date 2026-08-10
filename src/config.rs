//! Persisted project configuration.
//!
//! The on-disk format is deliberately small and stable: a schema version,
//! project roots, and the last selection for each discovered project. Writes
//! use a sibling temporary file so an interrupted save cannot leave a partial
//! configuration at the destination.

use crate::error::{ErrorCode, Result, SimtopError};
use crate::project::ProjectId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Version of the persisted configuration format.
pub const CONFIG_SCHEMA: u32 = 1;

/// User's persisted project roots and project-specific selections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Persisted configuration format version.
    pub schema: u32,
    /// Directories in which Xcode projects are discovered.
    pub project_roots: Vec<PathBuf>,
    /// Last-used selection keyed by the stable project identifier.
    pub project_selections: BTreeMap<String, SavedProjectSelection>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            project_roots: Vec::new(),
            project_selections: BTreeMap::new(),
        }
    }
}

/// Persisted Xcode selection for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProjectSelection {
    /// Selected workspace or project container path.
    pub container: PathBuf,
    /// Selected Xcode scheme.
    pub scheme: String,
    /// Selected build configuration.
    pub configuration: String,
    /// Selected simulator device UDID.
    pub simulator_udid: String,
}

/// Return the default configuration path under the user's home directory.
///
/// `HOME` is resolved explicitly rather than relying on a platform-specific
/// directories crate. A missing or empty `HOME` is an actionable environment
/// error rather than silently writing to the current directory.
pub fn default_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SimtopError::new(
                ErrorCode::IoError,
                "HOME is unavailable; cannot determine the simtop configuration path",
            )
        })?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("simtop")
        .join("config.json"))
}
/// Expand a leading `~` or `~/` using the current user's home directory.
///
/// Other paths, including `~other-user`, are returned unchanged. A missing
/// `HOME` is reported instead of silently resolving to the current directory.
pub fn expand_leading_tilde(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let starts_with_tilde = matches!(
        path.components().next(),
        Some(Component::Normal(component)) if component == OsStr::new("~")
    );
    if !starts_with_tilde {
        return Ok(path.to_path_buf());
    }

    let mut expanded = home_directory()?;
    for component in path.components().skip(1) {
        expanded.push(component.as_os_str());
    }
    Ok(expanded)
}

fn home_directory() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            SimtopError::new(
                ErrorCode::IoError,
                "HOME is unavailable; cannot expand a project path",
            )
        })
}

/// Canonicalize an existing project root after expanding a leading `~`.
pub fn canonical_project_root(path: impl AsRef<Path>) -> Result<PathBuf> {
    let expanded = expand_leading_tilde(path)?;
    fs::canonicalize(&expanded).map_err(|error| {
        SimtopError::with_source(
            ErrorCode::IoError,
            format!("failed to canonicalize project root {}", expanded.display()),
            error,
        )
    })
}

fn normalize_project_root(path: &Path) -> Result<PathBuf> {
    let expanded = expand_leading_tilde(path)?;
    Ok(fs::canonicalize(&expanded).unwrap_or(expanded))
}

impl Config {
    /// Return the default configuration path under the user's home directory.
    pub fn default_path() -> Result<PathBuf> {
        default_path()
    }

    /// Load a configuration from `path`.
    ///
    /// A missing file is the first-run state and returns [`Config::default`].
    /// Existing files must be valid JSON with exactly [`CONFIG_SCHEMA`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(SimtopError::with_source(
                    ErrorCode::IoError,
                    format!("failed to read configuration {}", path.display()),
                    error,
                ));
            }
        };

        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate_schema()?;
        Ok(config)
    }

    /// Save this configuration atomically to `path`.
    ///
    /// Serialization and schema validation happen before touching the
    /// destination. The serialized bytes are written, flushed, and synced to
    /// a sibling temporary file, which is then atomically renamed over the
    /// destination.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate_schema()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic_write(path.as_ref(), &bytes)
    }

    /// Look up the last selection for a discovered project.
    pub fn selection(&self, project_id: &ProjectId) -> Option<&SavedProjectSelection> {
        self.project_selections.get(project_id.as_str())
    }

    /// Alias for [`Config::selection`] for callers that prefer an explicit
    /// lookup name.
    pub fn lookup_selection(&self, project_id: &ProjectId) -> Option<&SavedProjectSelection> {
        self.selection(project_id)
    }

    /// Replace the saved selection for a project, returning the old value.
    pub fn update_selection(
        &mut self,
        project_id: &ProjectId,
        selection: SavedProjectSelection,
    ) -> Option<SavedProjectSelection> {
        self.project_selections
            .insert(project_id.as_str().to_owned(), selection)
    }

    /// Remove and return the saved selection for a project.
    pub fn remove_selection(&mut self, project_id: &ProjectId) -> Option<SavedProjectSelection> {
        self.project_selections.remove(project_id.as_str())
    }

    /// Add a canonical project root unless that root is already present.
    pub fn add_project_root(&mut self, root: impl AsRef<Path>) -> Result<bool> {
        self.deduplicate_project_roots()?;
        let canonical = canonical_project_root(root)?;
        if self
            .project_roots
            .iter()
            .any(|existing| existing == &canonical)
        {
            return Ok(false);
        }
        self.project_roots.push(canonical);
        Ok(true)
    }

    /// Remove a project root after normalizing both the requested and saved
    /// paths. Stale roots remain removable even after their directory goes
    /// away.
    pub fn remove_project_root(&mut self, root: impl AsRef<Path>) -> Result<bool> {
        self.deduplicate_project_roots()?;
        let normalized = normalize_project_root(root.as_ref())?;
        let before = self.project_roots.len();
        self.project_roots
            .retain(|existing| existing != &normalized);
        Ok(self.project_roots.len() != before)
    }

    /// Expand and canonicalize project roots, retaining the first occurrence
    /// of each root. Missing roots are retained in expanded form so a stale
    /// configuration can still be displayed and removed.
    pub fn deduplicate_project_roots(&mut self) -> Result<()> {
        let roots = std::mem::take(&mut self.project_roots);
        let mut seen = HashSet::new();
        self.project_roots = roots
            .into_iter()
            .map(|root| normalize_project_root(&root))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|root| seen.insert(root.clone()))
            .collect();
        Ok(())
    }

    /// Add a project root and normalize all existing roots.
    pub fn add_root(&mut self, root: impl AsRef<Path>) -> Result<bool> {
        self.add_project_root(root)
    }

    /// Remove a project root and normalize all existing roots.
    pub fn remove_root(&mut self, root: impl AsRef<Path>) -> Result<bool> {
        self.remove_project_root(root)
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != CONFIG_SCHEMA {
            return Err(SimtopError::new(
                ErrorCode::ParseError,
                format!(
                    "unsupported configuration schema {}; expected {}",
                    self.schema, CONFIG_SCHEMA
                ),
            ));
        }
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        SimtopError::new(
            ErrorCode::InvalidArgument,
            format!("configuration path has no file name: {}", path.display()),
        )
    })?;

    fs::create_dir_all(parent).map_err(|error| {
        SimtopError::with_source(
            ErrorCode::IoError,
            format!(
                "failed to create configuration directory {}",
                parent.display()
            ),
            error,
        )
    })?;

    let (temp_path, mut temp_file) = create_temp_file(parent, file_name)?;
    let result = write_temp_file(&mut temp_file, &temp_path, bytes);
    drop(temp_file);
    let result = result.and_then(|()| {
        fs::rename(&temp_path, path).map_err(|error| {
            SimtopError::with_source(
                ErrorCode::IoError,
                format!("failed to replace configuration file {}", path.display()),
                error,
            )
        })
    });
    if result.is_err() {
        // Best effort: the original destination remains untouched on every
        // failure before rename, while a failed cleanup cannot hide the
        // original error.
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_temp_file(parent: &Path, file_name: &std::ffi::OsStr) -> Result<(PathBuf, File)> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100u32 {
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".tmp-{}-{timestamp}-{attempt}", std::process::id()));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(SimtopError::with_source(
                    ErrorCode::IoError,
                    format!(
                        "failed to create temporary configuration file {}",
                        temp_path.display()
                    ),
                    error,
                ));
            }
        }
    }

    Err(SimtopError::new(
        ErrorCode::IoError,
        format!(
            "could not create a unique temporary configuration file beside {}",
            parent.join(file_name).display()
        ),
    ))
}

fn write_temp_file(file: &mut File, temp_path: &Path, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes).map_err(|error| {
        SimtopError::with_source(
            ErrorCode::IoError,
            format!(
                "failed to write temporary configuration file {}",
                temp_path.display()
            ),
            error,
        )
    })?;
    file.flush().map_err(|error| {
        SimtopError::with_source(
            ErrorCode::IoError,
            format!(
                "failed to flush temporary configuration file {}",
                temp_path.display()
            ),
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        SimtopError::with_source(
            ErrorCode::IoError,
            format!(
                "failed to sync temporary configuration file {}",
                temp_path.display()
            ),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "simtop-config-{label}-{}-{timestamp}",
            std::process::id()
        ))
    }

    fn sample_config() -> Config {
        let mut project_selections = BTreeMap::new();
        project_selections.insert(
            "demo".to_owned(),
            SavedProjectSelection {
                container: PathBuf::from("/tmp/Demo.xcworkspace"),
                scheme: "Demo".to_owned(),
                configuration: "Debug".to_owned(),
                simulator_udid: "AAAA-BBBB".to_owned(),
            },
        );
        Config {
            schema: CONFIG_SCHEMA,
            project_roots: vec![PathBuf::from("/tmp/Projects")],
            project_selections,
        }
    }

    #[test]
    fn missing_file_returns_default_config() {
        let path = unique_path("missing");
        let loaded = Config::load(&path).expect("missing config should be a first-run default");
        assert_eq!(loaded, Config::default());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn config_round_trips_through_human_readable_json() {
        let path = unique_path("round-trip");
        let expected = sample_config();
        expected.save(&path).expect("config should save");
        let loaded = Config::load(&path).expect("saved config should load");
        assert_eq!(loaded, expected);
        let text = fs::read_to_string(&path).expect("saved config should be readable");
        assert!(text.contains('\n'));
        assert!(text.contains("\"project_roots\""));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn schema_rejection_does_not_mutate_existing_file() {
        let path = unique_path("schema");
        let original = br#"{"schema":99,"project_roots":[],"project_selections":{}}"#;
        fs::write(&path, original).expect("test config should be written");
        let error = Config::load(&path).expect_err("unsupported schema should fail");
        assert_eq!(error.code(), ErrorCode::ParseError);
        assert_eq!(
            fs::read(&path).expect("config should remain readable"),
            original
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn saving_again_atomically_overwrites_previous_config() {
        let path = unique_path("overwrite");
        let first = sample_config();
        first.save(&path).expect("initial config should save");
        let mut second = Config::default();
        second
            .project_roots
            .push(PathBuf::from("/tmp/OtherProjects"));
        second.save(&path).expect("updated config should save");
        assert_eq!(
            Config::load(&path).expect("updated config should load"),
            second
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn project_roots_are_canonical_and_duplicate_free() {
        let root = unique_path("root-dedup");
        fs::create_dir_all(&root).expect("root should be created");
        let mut config = Config {
            project_roots: vec![root.clone(), root.join("."), root.clone()],
            ..Config::default()
        };
        config
            .deduplicate_project_roots()
            .expect("roots should normalize");
        assert_eq!(config.project_roots, vec![fs::canonicalize(&root).unwrap()]);
        assert!(!config
            .add_project_root(&root)
            .expect("duplicate root should be accepted"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn leading_tilde_expands_only_for_home_component() {
        let home = env::var_os("HOME").expect("test environment should provide HOME");
        assert_eq!(
            expand_leading_tilde("~/Projects").expect("tilde should expand"),
            PathBuf::from(home).join("Projects")
        );
        assert_eq!(
            expand_leading_tilde("~other/Projects").expect("user tilde should stay literal"),
            PathBuf::from("~other/Projects")
        );
    }

    #[test]
    fn selection_update_lookup_and_remove_use_project_id() {
        let project_id = ProjectId("demo".to_owned());
        let mut config = Config::default();
        let first = SavedProjectSelection {
            container: PathBuf::from("Demo.xcodeproj"),
            scheme: "Demo".to_owned(),
            configuration: "Debug".to_owned(),
            simulator_udid: "sim-a".to_owned(),
        };
        assert!(config
            .update_selection(&project_id, first.clone())
            .is_none());
        assert_eq!(config.selection(&project_id), Some(&first));

        let mut replacement = first.clone();
        replacement.scheme = "Tests".to_owned();
        assert_eq!(
            config.update_selection(&project_id, replacement.clone()),
            Some(first)
        );
        assert_eq!(config.lookup_selection(&project_id), Some(&replacement));
        assert_eq!(config.remove_selection(&project_id), Some(replacement));
        assert!(config.selection(&project_id).is_none());
    }
}
