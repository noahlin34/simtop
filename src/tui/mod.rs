//! Interactive terminal UI for `simtop`.
//!
//! This module owns the public TUI session API and the session boundary for
//! two persistent views: simulator monitoring and project build/run. The
//! [`simulators`] implementation owns terminal lifecycle and tab selection,
//! while [`projects`] provides the project view, so callers use the same
//! [`run`] and [`run_with`] API for either tab.

use std::path::PathBuf;
use std::time::Duration;

use crate::backend::SimulatorBackend;
use crate::error::SimtopError;

mod projects;
mod simulators;
/// Default bound for the activity history.
const DEFAULT_ACTIVITY_CAP: usize = 200;
/// Default bound for the followed device-log buffer.
const DEFAULT_LOG_CAP: usize = 500;

/// Tunables for the TUI session.
#[derive(Clone, Debug)]
pub struct TuiConfig {
    /// Interval between snapshot polls; adjustable at runtime with `+`/`-`.
    pub refresh_interval: Duration,
    /// Bounded activity (operation results/errors) history size.
    pub activity_capacity: usize,
    /// Bounded device-log buffer size shown in the log pane.
    pub log_capacity: usize,
    /// Resolved Xcode developer directory used by project discovery/builds.
    pub developer_dir: Option<PathBuf>,
    /// Persisted project configuration path.
    pub config_path: Option<PathBuf>,
    /// Root for project build caches.
    pub cache_root: Option<PathBuf>,
    /// Working directory used to launch built applications.
    pub launch_dir: Option<PathBuf>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            refresh_interval: Duration::from_secs(2),
            activity_capacity: DEFAULT_ACTIVITY_CAP,
            log_capacity: DEFAULT_LOG_CAP,
            developer_dir: None,
            config_path: None,
            cache_root: None,
            launch_dir: None,
        }
    }
}

/// Run the TUI session with default configuration until the user quits.
///
/// Consumes the backend, manages its own terminal setup (raw mode, alternate
/// screen), and restores the terminal on every exit path including panics.
/// Must be awaited inside a Tokio runtime.
pub async fn run(backend: Box<dyn SimulatorBackend>) -> Result<(), SimtopError> {
    run_with(backend, TuiConfig::default()).await
}

/// Run the TUI session with a custom [`TuiConfig`]; see [`run`].
pub async fn run_with(
    backend: Box<dyn SimulatorBackend>,
    config: TuiConfig,
) -> Result<(), SimtopError> {
    simulators::start(backend, config).await
}
