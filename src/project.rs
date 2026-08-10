//! Xcode project discovery and metadata loading.
//!
//! This module deliberately keeps filesystem traversal and Xcode invocation
//! independent from the terminal UI. Discovery is deterministic, bounded, and
//! treats a workspace and a project in the same directory as one project.

use crate::config::SavedProjectSelection;
use crate::error::{ErrorCode, Result, SimtopError};
use crate::model::SimDevice;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Stable identity for a discovered Xcode project.
///
/// The identity is the canonical project directory path. Unlike a basename or
/// a hash of a basename, this remains collision-safe when two roots contain
/// projects with the same name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProjectId(pub String);

impl ProjectId {
    /// Return the stable identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An Xcode container that can be passed to `xcodebuild`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum XcodeContainer {
    /// An `.xcworkspace` bundle.
    Workspace(PathBuf),
    /// An `.xcodeproj` bundle.
    Project(PathBuf),
}

impl XcodeContainer {
    /// The canonical path of this container.
    pub fn path(&self) -> &Path {
        match self {
            XcodeContainer::Workspace(path) | XcodeContainer::Project(path) => path,
        }
    }

    /// Whether this container is a workspace.
    pub fn is_workspace(&self) -> bool {
        matches!(self, XcodeContainer::Workspace(_))
    }
}

/// A discovered Xcode project row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XcodeProject {
    /// Stable identity derived from [`Self::directory`].
    pub id: ProjectId,
    /// Display name derived from the preferred container's filename.
    pub name: String,
    /// Canonical directory containing the containers.
    pub directory: PathBuf,
    /// All sibling containers, with the preferred workspace first.
    pub containers: Vec<XcodeContainer>,
    /// Workspace when one exists, otherwise the project container.
    pub preferred_container: XcodeContainer,
}

/// Metadata returned by `xcodebuild -list -json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectMetadata {
    pub schemes: Vec<String>,

    pub configurations: Vec<String>,
}
/// The effective choices for a project after persisted values are validated
/// against the current metadata and simulator snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectSelection {
    pub container: XcodeContainer,
    pub scheme: String,
    pub configuration: String,
    pub simulator_udid: String,
}

impl ResolvedProjectSelection {
    /// Convert an effective selection back to the stable persisted shape.
    pub fn into_saved(self) -> SavedProjectSelection {
        SavedProjectSelection {
            container: self.container.path().to_path_buf(),
            scheme: self.scheme,
            configuration: self.configuration,
            simulator_udid: self.simulator_udid,
        }
    }
}

/// Resolve a saved selection against the currently discovered project values.
///
/// Every choice is checked independently. An invalid container falls back to
/// the project's preferred container (workspace first), an invalid scheme
/// falls back to the first discovered scheme, an invalid configuration prefers
/// `Debug` and then the first discovered configuration, and an invalid
/// simulator prefers the first available device and then the first device.
/// `None` is returned when any required choice has no current value.
pub fn resolve_project_selection(
    project: &XcodeProject,
    metadata: &ProjectMetadata,
    simulators: &[SimDevice],
    saved: Option<&SavedProjectSelection>,
) -> Option<ResolvedProjectSelection> {
    let container = project
        .containers
        .iter()
        .find(|container| {
            saved
                .map(|saved| same_container_path(container.path(), &saved.container))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| {
            project
                .containers
                .iter()
                .find(|container| container.path() == project.preferred_container.path())
                .cloned()
        })
        .or_else(|| project.containers.first().cloned())?;

    let scheme = choose_string(
        metadata.schemes.iter().map(String::as_str),
        saved.map(|selection| selection.scheme.as_str()),
    )?;
    let configuration = choose_configuration(
        &metadata.configurations,
        saved.map(|selection| selection.configuration.as_str()),
    )?;
    let simulator_udid = choose_simulator(
        simulators,
        saved.map(|selection| selection.simulator_udid.as_str()),
    )?;

    Some(ResolvedProjectSelection {
        container,
        scheme,
        configuration,
        simulator_udid,
    })
}

impl XcodeProject {
    /// Resolve this project's persisted selection against current values.
    pub fn resolve_selection(
        &self,
        metadata: &ProjectMetadata,
        simulators: &[SimDevice],
        saved: Option<&SavedProjectSelection>,
    ) -> Option<ResolvedProjectSelection> {
        resolve_project_selection(self, metadata, simulators, saved)
    }
}

fn same_container_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn choose_string<'a, I>(values: I, saved: Option<&str>) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let values = values
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>();
    if let Some(saved) = saved.filter(|value| !value.trim().is_empty()) {
        if let Some(value) = values.iter().find(|value| **value == saved) {
            return Some((*value).to_owned());
        }
    }
    values.first().map(|value| (*value).to_owned())
}

fn choose_configuration(values: &[String], saved: Option<&str>) -> Option<String> {
    if let Some(value) = choose_string(values.iter().map(String::as_str), saved) {
        if saved.is_some_and(|saved| saved == value) {
            return Some(value);
        }
    }
    values
        .iter()
        .find(|value| value.trim() == "Debug")
        .cloned()
        .or_else(|| choose_string(values.iter().map(String::as_str), None))
}

fn choose_simulator(simulators: &[SimDevice], saved: Option<&str>) -> Option<String> {
    let valid = simulators
        .iter()
        .filter(|device| device.is_available && !device.udid.trim().is_empty())
        .collect::<Vec<_>>();
    if let Some(saved) = saved.filter(|value| !value.trim().is_empty()) {
        if let Some(device) = valid.iter().find(|device| device.udid == saved) {
            return Some(device.udid.clone());
        }
    }
    valid.first().map(|device| device.udid.clone()).or_else(|| {
        simulators
            .iter()
            .find(|device| !device.udid.trim().is_empty())
            .map(|device| device.udid.clone())
    })
}

const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    "DerivedData",
    ".build",
    "build",
    "Pods",
    "Carthage",
    "node_modules",
];

/// Discover Xcode projects under configured roots and the launch directory.
///
/// Roots are canonicalized and de-duplicated before traversal. A root itself
/// may be an Xcode container; otherwise its children are traversed to
/// `max_depth` directory levels. Traversal never descends into an Xcode
/// package, hidden directory, or one of the common generated/dependency
/// directories listed above.
pub fn discover_projects(
    roots: &[PathBuf],
    launch_dir: &Path,
    max_depth: usize,
) -> Result<Vec<XcodeProject>> {
    let launch_root = launch_dir.to_path_buf();
    let mut canonical_roots = BTreeSet::new();
    for root in roots.iter().chain(std::iter::once(&launch_root)) {
        if !root.exists() {
            // Configured roots can become stale. They should not prevent
            // discovery from the remaining roots (including launch_dir).
            continue;
        }
        if let Ok(canonical) = fs::canonicalize(root) {
            canonical_roots.insert(canonical);
        }
    }

    let mut groups: BTreeMap<PathBuf, BTreeSet<ContainerPath>> = BTreeMap::new();
    for root in canonical_roots {
        if is_xcode_container(&root) {
            add_container(&mut groups, root);
            continue;
        }
        if !root.is_dir() || is_hidden_path_component(root.file_name()) {
            continue;
        }
        let mut visited = HashSet::new();
        walk_directory(&root, 0, max_depth, &mut visited, &mut groups);
    }

    let mut projects = Vec::with_capacity(groups.len());
    for (directory, found) in groups {
        let mut containers: Vec<XcodeContainer> = found
            .into_iter()
            .map(|container| container.into_xcode_container())
            .collect();
        containers.sort_by(container_order);
        let Some(preferred_container) = containers.first().cloned() else {
            continue;
        };
        let name = preferred_container
            .path()
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| directory.to_string_lossy().into_owned());
        let id = ProjectId(directory.to_string_lossy().into_owned());
        projects.push(XcodeProject {
            id,
            name,
            directory,
            containers,
            preferred_container,
        });
    }

    // BTreeMap already gives path ordering; retaining an explicit sort makes
    // the contract obvious if the grouping implementation changes later.
    projects.sort_by(|left, right| {
        left.directory
            .cmp(&right.directory)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(projects)
}

fn walk_directory(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<PathBuf>,
    groups: &mut BTreeMap<PathBuf, BTreeSet<ContainerPath>>,
) {
    let Ok(canonical_directory) = fs::canonicalize(directory) else {
        return;
    };
    if !visited.insert(canonical_directory.clone()) {
        return;
    }

    let Ok(read_dir) = fs::read_dir(&canonical_directory) else {
        return;
    };
    let mut entries = read_dir
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let Some(file_name) = path.file_name() else {
            continue;
        };
        if is_hidden_path_component(Some(file_name)) {
            continue;
        }

        if is_xcode_container(&path) {
            if let Ok(canonical) = fs::canonicalize(&path) {
                add_container(groups, canonical);
            }
            // Xcode containers are packages; never inspect their contents.
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || should_skip_directory(file_name) || depth >= max_depth {
            continue;
        }
        walk_directory(&path, depth + 1, max_depth, visited, groups);
    }
}

fn should_skip_directory(name: &std::ffi::OsStr) -> bool {
    SKIPPED_DIRECTORY_NAMES
        .iter()
        .any(|skipped| name == std::ffi::OsStr::new(skipped))
}

fn is_hidden_path_component(name: Option<&std::ffi::OsStr>) -> bool {
    name.and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn is_xcode_container(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extension.eq_ignore_ascii_case("xcworkspace")
                || extension.eq_ignore_ascii_case("xcodeproj")
        })
        .unwrap_or(false)
}

fn add_container(groups: &mut BTreeMap<PathBuf, BTreeSet<ContainerPath>>, path: PathBuf) {
    let Some(directory) = path.parent() else {
        return;
    };
    groups
        .entry(directory.to_path_buf())
        .or_default()
        .insert(ContainerPath::new(path));
}

fn container_order(left: &XcodeContainer, right: &XcodeContainer) -> std::cmp::Ordering {
    container_rank(left)
        .cmp(&container_rank(right))
        .then_with(|| left.path().cmp(right.path()))
}

fn container_rank(container: &XcodeContainer) -> u8 {
    match container {
        XcodeContainer::Workspace(_) => 0,
        XcodeContainer::Project(_) => 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ContainerPath {
    workspace: bool,
    path: PathBuf,
}

impl ContainerPath {
    fn new(path: PathBuf) -> Self {
        let workspace = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("xcworkspace"))
            .unwrap_or(false);
        Self { workspace, path }
    }

    fn into_xcode_container(self) -> XcodeContainer {
        if self.workspace {
            XcodeContainer::Workspace(self.path)
        } else {
            XcodeContainer::Project(self.path)
        }
    }
}

/// Load schemes and configurations for an Xcode container.
///
/// The selected developer directory's `usr/bin/xcodebuild` is preferred. When
/// it is unavailable, `xcrun xcodebuild` is executed directly with
/// `DEVELOPER_DIR` set to the selected directory; no shell is involved.
pub async fn load_metadata(
    developer_dir: &Path,
    container: &XcodeContainer,
) -> Result<ProjectMetadata> {
    let selected_xcodebuild = developer_dir.join("usr/bin/xcodebuild");
    let use_selected_xcodebuild = selected_xcodebuild.is_file();
    let executable = if use_selected_xcodebuild {
        selected_xcodebuild
    } else {
        PathBuf::from("xcrun")
    };

    let mut args = Vec::<OsString>::new();
    if !use_selected_xcodebuild {
        args.push(OsString::from("xcodebuild"));
    }
    args.extend([OsString::from("-list"), OsString::from("-json")]);
    match container {
        XcodeContainer::Workspace(path) => {
            args.push(OsString::from("-workspace"));
            args.push(path.as_os_str().to_os_string());
        }
        XcodeContainer::Project(path) => {
            args.push(OsString::from("-project"));
            args.push(path.as_os_str().to_os_string());
        }
    }

    let mut command = Command::new(&executable);
    command
        .args(&args)
        .env("DEVELOPER_DIR", developer_dir)
        .stdin(Stdio::null());
    if let Some(parent) = container.path().parent() {
        command.current_dir(parent);
    }
    let output = command.output().await.map_err(|error| {
        SimtopError::with_source(
            ErrorCode::XcodeNotFound,
            format!("failed to execute {}", executable.display()),
            error,
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("xcodebuild exited with status {:?}", output.status.code())
        } else {
            format!(
                "xcodebuild exited with status {:?}: {}",
                output.status.code(),
                stderr
            )
        };
        return Err(SimtopError::new(ErrorCode::CommandFailed, detail));
    }

    parse_metadata_output(&output.stdout)
}

fn parse_metadata_output(output: &[u8]) -> Result<ProjectMetadata> {
    let text = String::from_utf8_lossy(output);
    let value = parse_json_output(&text).map_err(|error| {
        SimtopError::new(
            ErrorCode::ParseError,
            format!("failed to parse xcodebuild metadata: {error}"),
        )
    })?;

    let mut schemes = BTreeSet::new();
    let mut configurations = BTreeSet::new();
    collect_named_values(&value, &mut schemes, &mut configurations);
    Ok(ProjectMetadata {
        schemes: schemes.into_iter().collect(),
        configurations: configurations.into_iter().collect(),
    })
}

fn parse_json_output(text: &str) -> std::result::Result<Value, serde_json::Error> {
    let trimmed = text.trim();
    match serde_json::from_str(trimmed) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            // Some Xcode versions can prefix JSON with informational output.
            // Restrict fallback parsing to one complete object/array so an
            // arbitrary malformed response is not silently accepted.
            let object_start = trimmed.find('{');
            let array_start = trimmed.find('[');
            let start = match (object_start, array_start) {
                (Some(object), Some(array)) => Some(object.min(array)),
                (Some(object), None) => Some(object),
                (None, Some(array)) => Some(array),
                (None, None) => None,
            };
            if let Some(start) = start {
                let object_end = trimmed.rfind('}');
                let array_end = trimmed.rfind(']');
                let end = match (object_end, array_end) {
                    (Some(object), Some(array)) => Some(object.max(array)),
                    (Some(object), None) => Some(object),
                    (None, Some(array)) => Some(array),
                    (None, None) => None,
                };
                if let Some(end) = end {
                    if start <= end {
                        if let Ok(value) = serde_json::from_str(&trimmed[start..=end]) {
                            return Ok(value);
                        }
                    }
                }
            }
            Err(first_error)
        }
    }
}

fn collect_named_values(
    value: &Value,
    schemes: &mut BTreeSet<String>,
    configurations: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key.eq_ignore_ascii_case("schemes") || key.eq_ignore_ascii_case("scheme") {
                    collect_string_values(child, schemes);
                } else if key.eq_ignore_ascii_case("configurations")
                    || key.eq_ignore_ascii_case("configuration")
                {
                    collect_string_values(child, configurations);
                }
                collect_named_values(child, schemes, configurations);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_named_values(child, schemes, configurations);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn collect_string_values(value: &Value, values: &mut BTreeSet<String>) {
    match value {
        Value::String(string) => {
            let string = string.trim();
            if !string.is_empty() {
                values.insert(string.to_owned());
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_string_values(value, values);
            }
        }
        Value::Object(object) => {
            let mut found_named_value = false;
            for (key, value) in object {
                if key.eq_ignore_ascii_case("name")
                    || key.eq_ignore_ascii_case("scheme")
                    || key.eq_ignore_ascii_case("configuration")
                {
                    found_named_value = true;
                    collect_string_values(value, values);
                }
            }
            if !found_named_value {
                for value in object.values() {
                    collect_string_values(value, values);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("simtop-project-{nonce}"));
            fs::create_dir_all(&path).expect("create temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_dir(path: &Path) {
        fs::create_dir_all(path).expect("create directory");
    }

    #[test]
    fn discovery_groups_siblings_skips_packages_and_honors_depth() {
        let temp = TempDir::new();
        let app = temp.path().join("App");
        make_dir(&app);
        make_dir(&app.join("App.xcodeproj"));
        make_dir(&app.join("App.xcworkspace"));
        make_dir(&app.join("Deep").join("Deep.xcodeproj"));
        make_dir(&app.join(".hidden").join("Hidden.xcodeproj"));
        make_dir(&app.join("Pods").join("Pod.xcodeproj"));
        make_dir(
            &app.join("App.xcodeproj")
                .join("Nested")
                .join("Nested.xcodeproj"),
        );

        let projects = discover_projects(&[temp.path().to_path_buf()], temp.path(), 1)
            .expect("discovery succeeds");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "App");
        assert_eq!(projects[0].containers.len(), 2);
        assert!(matches!(
            projects[0].preferred_container,
            XcodeContainer::Workspace(_)
        ));
        assert!(matches!(
            projects[0].containers[1],
            XcodeContainer::Project(_)
        ));
    }

    #[test]
    fn metadata_parser_handles_workspace_and_project_shapes() {
        let workspace = br#"{
            "workspace": {"schemes": ["App", {"name": "Tests"}], "projects": ["App.xcodeproj"]}
        }"#;
        let parsed = parse_metadata_output(workspace).expect("workspace metadata parses");
        assert_eq!(parsed.schemes, vec!["App", "Tests"]);
        assert!(parsed.configurations.is_empty());

        let project = br#"{
            "project": {"configurations": ["Debug", "Release"], "schemes": ["App"]}
        }"#;
        let parsed = parse_metadata_output(project).expect("project metadata parses");
        assert_eq!(parsed.schemes, vec!["App"]);
        assert_eq!(parsed.configurations, vec!["Debug", "Release"]);
    }

    #[test]
    fn invalid_saved_values_fall_back_without_panicking() {
        let workspace = XcodeContainer::Workspace(PathBuf::from("/tmp/App.xcworkspace"));
        let project_container = XcodeContainer::Project(PathBuf::from("/tmp/App.xcodeproj"));
        let project = XcodeProject {
            id: ProjectId("/tmp".to_owned()),
            name: "App".to_owned(),
            directory: PathBuf::from("/tmp"),
            containers: vec![workspace.clone(), project_container],
            preferred_container: workspace.clone(),
        };
        let metadata = ProjectMetadata {
            schemes: vec!["App".to_owned(), "Tests".to_owned()],
            configurations: vec!["Release".to_owned(), "Debug".to_owned()],
        };
        let simulators = vec![
            SimDevice {
                udid: "deleted".to_owned(),
                name: "Deleted".to_owned(),
                state: crate::model::DeviceState::Shutdown,
                device_type: "type".to_owned(),
                runtime: "runtime".to_owned(),
                os_version: "18.0".to_owned(),
                is_available: false,
            },
            SimDevice {
                udid: "available".to_owned(),
                name: "Available".to_owned(),
                state: crate::model::DeviceState::Shutdown,
                device_type: "type".to_owned(),
                runtime: "runtime".to_owned(),
                os_version: "18.0".to_owned(),
                is_available: true,
            },
        ];
        let saved = SavedProjectSelection {
            container: PathBuf::from("/tmp/Missing.xcodeproj"),
            scheme: "Missing".to_owned(),
            configuration: "Missing".to_owned(),
            simulator_udid: "missing".to_owned(),
        };

        let resolved = resolve_project_selection(&project, &metadata, &simulators, Some(&saved))
            .expect("current choices should resolve");
        assert_eq!(resolved.container, workspace);
        assert_eq!(resolved.scheme, "App");
        assert_eq!(resolved.configuration, "Debug");
        assert_eq!(resolved.simulator_udid, "available");
    }
}
