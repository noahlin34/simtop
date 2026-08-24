//! Command-line interface: argument parsing and command dispatch.
//!
//! `simtop` runs the interactive TUI by default; every other command is a
//! one-shot automation operation against the shared [`SimulatorBackend`]:
//!
//! - `list` / `watch` — snapshot the device set (once / continuously);
//! - `boot` / `shutdown` / `open` / `create` / `delete` — device lifecycle;
//! - `app install|launch|terminate|uninstall|logs|open-url` — per-device
//!   app operations;
//! - `screenshot` — capture the device screen.
//!
//! # Selectors
//!
//! Commands that address a device take a selector, resolved deterministically:
//! an exact UDID match wins; otherwise the selector must match exactly one
//! device name case-insensitively. Zero matches is
//! [`ErrorCode::DeviceNotFound`]; more than one is
//! [`ErrorCode::InvalidArgument`] listing the matching UDIDs — a command
//! never silently picks an ambiguous simulator.
//!
//! # Streams and exit codes
//!
//! `watch` and `app logs --follow` stream until Ctrl-C (clean exit 0) or a
//! `--count`/non-follow bound. Every failure maps to a deterministic exit
//! code via [`SimtopError::exit_code`] (see `src/error.rs`); declining the
//! exit with clap's standard code 2. `--json` never prompts.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use crate::backend::{connect, SimulatorBackend};
use crate::config::Config;
use crate::error::{ErrorCode, SimtopError};
use crate::model::SimDevice;
use crate::output::Output;
use crate::xcode::XcodeEnvironment;

/// Command-line entry point. `main` delegates here inside the tokio runtime.
pub async fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    let out = Output::new(cli.json);

    // Platform check after parsing so --help/--version work everywhere.
    if let Err(err) = crate::require_macos() {
        return report(&out, &cli, err);
    }

    match dispatch(&cli, &out).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Aborted) => {
            eprintln!("simtop: aborted.");
            ExitCode::from(1)
        }
        Err(RunError::Failed(err)) => report(&out, &cli, err),
    }
}

/// Emit an error and return its deterministic exit code.
fn report(out: &Output, cli: &Cli, err: SimtopError) -> ExitCode {
    let code = to_exit_code(err.exit_code());
    if let Err(io_err) = out.error(cli.command_name(), &err) {
        eprintln!("simtop: failed to report error: {io_err}");
    }
    code
}

fn to_exit_code(code: i32) -> ExitCode {
    ExitCode::from(code.clamp(1, 255) as u8)
}

/// Internal dispatch outcome. [`RunError::Aborted`] marks an interactive
/// operation the user declined (exit 1, no error envelope — `--json` and
/// `--no-input` never reach that path).
enum RunError {
    Failed(SimtopError),
    Aborted,
}

impl From<SimtopError> for RunError {
    fn from(err: SimtopError) -> Self {
        RunError::Failed(err)
    }
}

impl From<io::Error> for RunError {
    fn from(err: io::Error) -> Self {
        RunError::Failed(SimtopError::from(err))
    }
}

/// simtop — high-performance iOS Simulator management TUI and automation CLI.
#[derive(Debug, Parser)]
#[command(name = "simtop", version, about, disable_help_subcommand = false)]
struct Cli {
    /// Emit machine-readable JSON (schema v1) on stdout instead of human
    /// text. Never prompts; one envelope per line for streaming commands.
    #[arg(long, global = true)]
    json: bool,

    /// Xcode developer directory override (resolution order: this flag,
    /// then $DEVELOPER_DIR, then `xcode-select -p`).
    #[arg(long, global = true, value_name = "DIR")]
    developer_dir: Option<PathBuf>,

    /// Timeout for backend operations, in seconds.
    #[arg(
        long,
        global = true,
        value_name = "SECONDS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    timeout: u64,

    /// Never prompt for confirmation. With `--no-input` (or `--json`)
    /// destructive commands proceed without asking.
    #[arg(long, global = true)]
    no_input: bool,

    /// Theme override for the interactive TUI.
    #[arg(long, global = true, value_name = "THEME")]
    theme: Option<crate::tui::theme::ThemeName>,

    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// Stable command name used in JSON envelopes and error reports.
    fn command_name(&self) -> &'static str {
        match &self.command {
            None => "tui",
            Some(command) => command.name(),
        }
    }
}

/// Commands (default: interactive TUI).
#[derive(Debug, Subcommand)]
enum Command {
    /// List all simulators (one snapshot).
    List,
    /// Continuously refresh and print simulator snapshots until interrupted.
    Watch {
        /// Refresh interval in seconds.
        #[arg(
            long,
            default_value_t = 2,
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        interval: u64,
        /// Stop after this many snapshots (default: run until Ctrl-C).
        #[arg(long)]
        count: Option<u64>,
    },
    /// Boot a simulator (idempotent when already booted).
    Boot {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
    },
    /// Shut down a simulator (idempotent when already shutdown).
    Shutdown {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
    },
    /// Open the Simulator UI focused on the device.
    Open {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
    },
    /// Create a simulator from CoreSimulator identifiers.
    Create {
        /// Name for the new device.
        #[arg(long)]
        name: String,
        /// Device-type identifier, e.g.
        /// com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro.
        #[arg(long, value_name = "IDENTIFIER")]
        device_type: String,
        /// Runtime identifier, e.g.
        /// com.apple.CoreSimulator.SimRuntime.iOS-18-0.
        #[arg(long, value_name = "IDENTIFIER")]
        runtime: String,
    },
    /// Delete a simulator (prompts for confirmation in interactive human
    /// mode; never prompts with --json or --no-input).
    Delete {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
    },
    /// Capture a PNG screenshot of the device screen.
    Screenshot {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
        /// Output file (PNG). Defaults to
        /// simtop-<UDID>-<unix-seconds>.png in the current directory.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Manage apps installed on a simulator.
    App {
        /// Simulator: exact UDID or unique case-insensitive name.
        selector: String,
        #[command(subcommand)]
        action: AppAction,
    },
}

impl Command {
    /// Stable command name for JSON envelopes and error reports.
    fn name(&self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Watch { .. } => "watch",
            Command::Boot { .. } => "boot",
            Command::Shutdown { .. } => "shutdown",
            Command::Open { .. } => "open",
            Command::Create { .. } => "create",
            Command::Delete { .. } => "delete",
            Command::Screenshot { .. } => "screenshot",
            Command::App { action, .. } => action.name(),
        }
    }
}

/// App operations, nested under `simtop app <SELECTOR> ...`.
#[derive(Debug, Subcommand)]
enum AppAction {
    /// Install an .app bundle onto the device.
    Install {
        /// Path to the .app bundle.
        app_path: PathBuf,
    },
    /// Launch an installed app by bundle identifier.
    Launch {
        /// Bundle identifier, e.g. com.example.MyApp.
        bundle_id: String,
    },
    /// Terminate a running app by bundle identifier.
    Terminate {
        /// Bundle identifier, e.g. com.example.MyApp.
        bundle_id: String,
    },
    /// Uninstall an app by bundle identifier.
    Uninstall {
        /// Bundle identifier, e.g. com.example.MyApp.
        bundle_id: String,
    },
    /// Print recent device log entries; --follow polls for new entries.
    Logs {
        /// Poll interval in seconds.
        #[arg(
            long,
            default_value_t = 2,
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        interval: u64,
        /// Keep polling for new log entries until interrupted.
        #[arg(long)]
        follow: bool,
    },
    /// Open a URL (http/https or custom scheme) inside the device.
    OpenUrl {
        /// URL to open, e.g. https://example.com or myapp://path.
        url: String,
    },
}

impl AppAction {
    fn name(&self) -> &'static str {
        match self {
            AppAction::Install { .. } => "app install",
            AppAction::Launch { .. } => "app launch",
            AppAction::Terminate { .. } => "app terminate",
            AppAction::Uninstall { .. } => "app uninstall",
            AppAction::Logs { .. } => "app logs",
            AppAction::OpenUrl { .. } => "app open-url",
        }
    }
}

/// Resolve Xcode, connect the backend, and run the selected command.
async fn dispatch(cli: &Cli, out: &Output) -> Result<(), RunError> {
    let xcode = XcodeEnvironment::resolve(cli.developer_dir.as_deref())?;
    let backend = connect(xcode.developer_dir(), Duration::from_secs(cli.timeout))?;
    match &cli.command {
        None => {
            if cli.json || cli.no_input {
                return Err(RunError::Failed(SimtopError::new(
                    ErrorCode::InvalidArgument,
                    "the interactive TUI cannot be combined with --json or --no-input",
                )));
            }
            let config = tui_config(xcode.developer_dir(), cli.theme)?;
            crate::tui::run_with(backend, config)
                .await
                .map_err(RunError::Failed)
        }
        Some(command) => {
            let backend: &dyn SimulatorBackend = &*backend;
            run_command(backend, out, command, cli.no_input).await
        }
    }
}

fn tui_config(
    developer_dir: &Path,
    theme: Option<crate::tui::theme::ThemeName>,
) -> Result<crate::tui::TuiConfig, RunError> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RunError::Failed(SimtopError::new(
                ErrorCode::IoError,
                "HOME is unavailable; cannot determine TUI runtime paths",
            ))
        })?;
    let launch_dir = env::current_dir()?;
    Ok(crate::tui::TuiConfig {
        developer_dir: Some(developer_dir.to_path_buf()),
        config_path: Some(Config::default_path()?),
        cache_root: Some(
            PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("simtop"),
        ),
        launch_dir: Some(launch_dir),
        theme,
        ..crate::tui::TuiConfig::default()
    })
}

async fn run_command(
    backend: &dyn SimulatorBackend,
    out: &Output,
    command: &Command,
    no_input: bool,
) -> Result<(), RunError> {
    match command {
        Command::List => cmd_list(backend, out).await,
        Command::Watch { interval, count } => cmd_watch(backend, out, *interval, *count).await,
        Command::Boot { selector } => cmd_boot(backend, out, selector).await,
        Command::Shutdown { selector } => cmd_shutdown(backend, out, selector).await,
        Command::Open { selector } => cmd_open(backend, out, selector).await,
        Command::Create {
            name,
            device_type,
            runtime,
        } => cmd_create(backend, out, name, device_type, runtime).await,
        Command::Delete { selector } => cmd_delete(backend, out, selector, no_input).await,
        Command::Screenshot { selector, output } => {
            cmd_screenshot(backend, out, selector, output.as_deref()).await
        }
        Command::App { selector, action } => cmd_app(backend, out, selector, action).await,
    }
}

/// Resolve a device selector: exact UDID first, then a unique
/// case-insensitive name. Ambiguity is an error, never a silent pick.
async fn resolve_device(
    backend: &dyn SimulatorBackend,
    selector: &str,
) -> Result<SimDevice, SimtopError> {
    let devices = backend.list_devices().await?;
    if let Some(device) = devices.iter().find(|d| d.udid == selector) {
        return Ok(device.clone());
    }
    let names: Vec<&SimDevice> = devices
        .iter()
        .filter(|d| d.name.eq_ignore_ascii_case(selector))
        .collect();
    match names.len() {
        0 => Err(SimtopError::new(
            ErrorCode::DeviceNotFound,
            format!(
                "no simulator matches selector '{selector}' (expected an exact UDID or a unique case-insensitive name)"
            ),
        )),
        1 => Ok(names[0].clone()),
        n => {
            let udids: Vec<&str> = names.iter().map(|d| d.udid.as_str()).collect();
            Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "selector '{selector}' is ambiguous: matches {n} simulators ({})",
                    udids.join(", ")
                ),
            ))
        }
    }
}

async fn cmd_list(backend: &dyn SimulatorBackend, out: &Output) -> Result<(), RunError> {
    let snapshot = backend.snapshot().await?;
    out.snapshot("list", &snapshot, false)?;
    Ok(())
}

async fn cmd_watch(
    backend: &dyn SimulatorBackend,
    out: &Output,
    interval: u64,
    count: Option<u64>,
) -> Result<(), RunError> {
    let mut emitted: u64 = 0;
    loop {
        let snapshot = backend.snapshot().await?;
        out.snapshot("watch", &snapshot, true)?;
        emitted += 1;
        if count.is_some_and(|c| emitted >= c) {
            return Ok(());
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
    }
}

async fn cmd_boot(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    backend.boot(&device.udid).await?;
    out.udid_result(
        "boot",
        &device.udid,
        format!("Booted {} ({})", device.name, device.udid),
    )?;
    Ok(())
}

async fn cmd_shutdown(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    backend.shutdown(&device.udid).await?;
    out.udid_result(
        "shutdown",
        &device.udid,
        format!("Shut down {} ({})", device.name, device.udid),
    )?;
    Ok(())
}

async fn cmd_open(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    backend.open(&device.udid).await?;
    out.udid_result(
        "open",
        &device.udid,
        format!("Opened {} ({}) in Simulator", device.name, device.udid),
    )?;
    Ok(())
}

async fn cmd_create(
    backend: &dyn SimulatorBackend,
    out: &Output,
    name: &str,
    device_type: &str,
    runtime: &str,
) -> Result<(), RunError> {
    let device = backend.create(name, device_type, runtime).await?;
    out.created(&device)?;
    Ok(())
}

async fn cmd_delete(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
    no_input: bool,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    if !out.is_json() && !no_input && io::stdin().is_terminal() && !confirm_delete(&device) {
        return Err(RunError::Aborted);
    }
    backend.delete(&device.udid).await?;
    out.udid_result(
        "delete",
        &device.udid,
        format!("Deleted {} ({})", device.name, device.udid),
    )?;
    Ok(())
}

/// Interactive delete confirmation on stderr. Returns false when the user
/// declines (or the answer cannot be read — never delete on doubt).
fn confirm_delete(device: &SimDevice) -> bool {
    let mut err = io::stderr().lock();
    let _ = write!(
        err,
        "Delete simulator '{}' ({})? [y/N] ",
        device.name, device.udid
    );
    let _ = err.flush();
    drop(err);
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

async fn cmd_screenshot(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
    output: Option<&Path>,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    let path = match output {
        Some(path) => path.to_path_buf(),
        None => default_screenshot_path(&device.udid),
    };
    backend.screenshot(&device.udid, &path).await?;
    out.screenshot_result(
        "screenshot",
        &device.udid,
        &path,
        format!("Saved screenshot to {}", path.display()),
    )?;
    Ok(())
}

fn default_screenshot_path(udid: &str) -> PathBuf {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("simtop-{udid}-{seconds}.png"))
}

async fn cmd_app(
    backend: &dyn SimulatorBackend,
    out: &Output,
    selector: &str,
    action: &AppAction,
) -> Result<(), RunError> {
    let device = resolve_device(backend, selector).await?;
    match action {
        AppAction::Install { app_path } => {
            backend.install(&device.udid, app_path).await?;
            out.install_result(
                "app install",
                &device.udid,
                app_path,
                format!("Installed {} on {}", app_path.display(), device.name),
            )?;
        }
        AppAction::Launch { bundle_id } => {
            let info = backend.launch(&device.udid, bundle_id).await?;
            out.launched(&info)?;
        }
        AppAction::Terminate { bundle_id } => {
            backend.terminate(&device.udid, bundle_id).await?;
            out.bundle_result(
                "app terminate",
                &device.udid,
                bundle_id,
                format!("Terminated {bundle_id} on {}", device.name),
            )?;
        }
        AppAction::Uninstall { bundle_id } => {
            backend.uninstall(&device.udid, bundle_id).await?;
            out.bundle_result(
                "app uninstall",
                &device.udid,
                bundle_id,
                format!("Uninstalled {bundle_id} from {}", device.name),
            )?;
        }
        AppAction::OpenUrl { url } => {
            backend.open_url(&device.udid, url).await?;
            out.url_result(
                "app open-url",
                &device.udid,
                url,
                format!("Opened {url} on {}", device.name),
            )?;
        }
        AppAction::Logs { interval, follow } => {
            cmd_logs(backend, out, &device, *interval, *follow).await?;
        }
    }
    Ok(())
}

/// One-shot or --follow device log streaming.
///
/// The backend returns a rolling window of recent entries; follow mode
/// emits only entries newer than the previously seen count. When the
/// window shrinks (log rotation or a fresh boot), emission restarts from
/// the top of the window.
async fn cmd_logs(
    backend: &dyn SimulatorBackend,
    out: &Output,
    device: &SimDevice,
    interval: u64,
    follow: bool,
) -> Result<(), RunError> {
    let mut seen: usize = 0;
    loop {
        let log = backend.logs(&device.udid).await?;
        if !follow {
            out.log("app logs", &log)?;
            return Ok(());
        }
        let entries = &log.entries;
        // Follow mode emits only entries newer than the previously seen
        // count; a shrinking window (rotation, fresh boot) restarts from
        // the top.
        let fresh: &[crate::model::LogEntry] = if entries.len() >= seen {
            &entries[seen..]
        } else {
            &entries[..]
        };
        if !fresh.is_empty() {
            out.log_entries("app logs", &device.udid, fresh)?;
            seen = entries.len();
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    use crate::model::{DeviceLog, DeviceSnapshot, LaunchInfo};

    /// Deterministic backend used by dispatch tests.
    struct FakeBackend {
        devices: Vec<SimDevice>,
    }

    impl FakeBackend {
        fn new(devices: Vec<SimDevice>) -> Self {
            FakeBackend { devices }
        }
    }

    #[async_trait]
    impl SimulatorBackend for FakeBackend {
        async fn snapshot(&self) -> Result<DeviceSnapshot, SimtopError> {
            Ok(DeviceSnapshot::new(
                1,
                "2026-08-08T00:00:00Z",
                self.devices.clone(),
            ))
        }
        async fn list_devices(&self) -> Result<Vec<SimDevice>, SimtopError> {
            Ok(self.devices.clone())
        }
        async fn boot(&self, _udid: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn shutdown(&self, _udid: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn open(&self, _udid: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn create(
            &self,
            _name: &str,
            _device_type: &str,
            _runtime: &str,
        ) -> Result<SimDevice, SimtopError> {
            Err(SimtopError::new(
                ErrorCode::UnsupportedOperation,
                "operation unavailable in test backend",
            ))
        }
        async fn delete(&self, _udid: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn install(&self, _udid: &str, _app_path: &Path) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn launch(&self, _udid: &str, _bundle_id: &str) -> Result<LaunchInfo, SimtopError> {
            Err(SimtopError::new(
                ErrorCode::UnsupportedOperation,
                "operation unavailable in test backend",
            ))
        }
        async fn terminate(&self, _udid: &str, _bundle_id: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn uninstall(&self, _udid: &str, _bundle_id: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn open_url(&self, _udid: &str, _url: &str) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn screenshot(&self, _udid: &str, _out_path: &Path) -> Result<(), SimtopError> {
            Ok(())
        }
        async fn logs(&self, _udid: &str) -> Result<DeviceLog, SimtopError> {
            Ok(DeviceLog {
                udid: _udid.to_string(),
                entries: Vec::new(),
            })
        }
    }

    fn device(udid: &str, name: &str) -> SimDevice {
        SimDevice {
            udid: udid.to_string(),
            name: name.to_string(),
            state: crate::model::DeviceState::Shutdown,
            device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro".to_string(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-18-0".to_string(),
            os_version: "18.0".to_string(),
            is_available: true,
        }
    }

    #[tokio::test]
    async fn exact_udid_wins_over_name() {
        let backend =
            FakeBackend::new(vec![device("AAAA-1", "iPhone"), device("BBBB-2", "aaaa-1")]);
        let resolved = resolve_device(&backend, "AAAA-1").await.unwrap();
        assert_eq!(resolved.udid, "AAAA-1");
    }

    #[tokio::test]
    async fn unique_case_insensitive_name_resolves() {
        let backend = FakeBackend::new(vec![
            device("AAAA-1", "iPhone 16 Pro"),
            device("BBBB-2", "iPhone 15"),
        ]);
        let resolved = resolve_device(&backend, "iphone 16 pro").await.unwrap();
        assert_eq!(resolved.udid, "AAAA-1");
    }

    #[tokio::test]
    async fn ambiguous_name_is_an_error() {
        let backend = FakeBackend::new(vec![device("AAAA-1", "Demo"), device("BBBB-2", "DEMO")]);
        let err = resolve_device(&backend, "demo").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
        assert!(err.message().contains("ambiguous"));
        assert!(err.message().contains("AAAA-1"));
        assert!(err.message().contains("BBBB-2"));
    }

    #[tokio::test]
    async fn no_match_is_device_not_found() {
        let backend = FakeBackend::new(vec![device("AAAA-1", "iPhone 16 Pro")]);
        let err = resolve_device(&backend, "nope").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::DeviceNotFound);
    }

    #[test]
    fn theme_defaults_to_none() {
        let cli = Cli::try_parse_from(["simtop"]).unwrap();
        assert_eq!(cli.theme, None);
    }

    #[test]
    fn theme_accepts_all_canonical_values() {
        for theme in crate::tui::theme::ThemeName::ALL.iter().copied() {
            let value = theme.to_string();
            let cli = Cli::try_parse_from(["simtop", "--theme", value.as_str()]).unwrap();
            assert_eq!(cli.theme, Some(theme));
        }
    }

    #[test]
    fn theme_rejects_invalid_values() {
        let err = Cli::try_parse_from(["simtop", "--theme", "not-a-theme"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn theme_is_global_before_and_after_subcommand() {
        let before = Cli::try_parse_from(["simtop", "--theme", "nord", "list"]).unwrap();
        assert_eq!(before.theme, Some(crate::tui::theme::ThemeName::Nord));
        assert!(matches!(before.command, Some(Command::List)));

        let after = Cli::try_parse_from(["simtop", "list", "--theme", "nord"]).unwrap();
        assert_eq!(after.theme, Some(crate::tui::theme::ThemeName::Nord));
        assert!(matches!(after.command, Some(Command::List)));
    }

    #[test]
    fn command_names_are_stable() {
        assert_eq!(Command::List.name(), "list");
        assert_eq!(
            Command::Boot {
                selector: String::new()
            }
            .name(),
            "boot"
        );
        assert_eq!(
            Command::App {
                selector: String::new(),
                action: AppAction::OpenUrl { url: String::new() },
            }
            .name(),
            "app open-url"
        );
        assert_eq!(
            Command::App {
                selector: String::new(),
                action: AppAction::Logs {
                    interval: 2,
                    follow: false,
                },
            }
            .name(),
            "app logs"
        );
    }

    #[test]
    fn default_screenshot_path_is_png() {
        let path = default_screenshot_path("AAAA-1");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("simtop-AAAA-1-"));
        assert!(name.ends_with(".png"));
    }
}
