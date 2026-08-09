//! Narrowly typed `simctl` fallback client.
//!
//! Every operation is an explicit method with validated, deterministic
//! arguments: UDIDs only (never names or `"booted"`), identifiers not display
//! names, and no shell. There is no generic "run this simctl command"
//! passthrough — [`SimctlClient::run`] is private and only reachable through
//! the typed operations below.
//!
//! All subprocesses use [`tokio::process::Command`] with argument arrays,
//! null stdin, piped stdout/stderr, `kill_on_drop` for cancellation safety,
//! and a per-call timeout. When the timeout fires the future is dropped and
//! tokio kills and reaps the child.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::backend::validate_udid;
use crate::error::{ErrorCode, SimtopError};
use crate::model::{DeviceLog, DeviceState, LaunchInfo, LogEntry, SimDevice};

/// Log window requested from `log show` in the simulator.
const LOG_LAST: &str = "10m";

/// Captured stdout of a successful simctl invocation (stderr is only
/// meaningful on failure and is consumed in `run`).
struct CommandOutput {
    stdout: String,
}

/// Client for the `simctl` binary at a resolved path.
#[derive(Debug, Clone)]
pub struct SimctlClient {
    simctl: PathBuf,
    timeout: Duration,
}

impl SimctlClient {
    /// `simctl` must be the already-validated path from the Xcode
    /// environment; `timeout` bounds every subprocess.
    pub fn new(simctl: PathBuf, timeout: Duration) -> Self {
        Self { simctl, timeout }
    }

    /// Run simctl with an argument array. Non-zero exits become
    /// [`ErrorCode::CommandFailed`] (or [`ErrorCode::DeviceNotFound`] when
    /// stderr identifies an unknown device), carrying the captured stderr.
    async fn run(&self, args: &[&str]) -> Result<CommandOutput, SimtopError> {
        let mut cmd = Command::new(&self.simctl);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            SimtopError::with_source(
                ErrorCode::IoError,
                format!("failed to spawn {} {args:?}", self.simctl.display()),
                e,
            )
        })?;
        let output = match timeout(self.timeout, child.wait_with_output()).await {
            Err(_elapsed) => {
                return Err(SimtopError::new(
                    ErrorCode::Timeout,
                    format!("simctl {args:?} exceeded the {:?} timeout", self.timeout),
                ))
            }
            Ok(res) => res.map_err(|e| {
                SimtopError::with_source(
                    ErrorCode::IoError,
                    "failed to read simctl output".to_string(),
                    e,
                )
            })?,
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let code = classify_failure(&stderr);
            return Err(SimtopError::new(
                code,
                format!(
                    "simctl {args:?} failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            ));
        }
        Ok(CommandOutput { stdout })
    }

    /// Full device list, parsed from `simctl list -j devices`.
    pub async fn list_devices(&self) -> Result<Vec<SimDevice>, SimtopError> {
        let out = self.run(&["list", "-j", "devices"]).await?;
        parse_device_list(&out.stdout)
    }

    /// Boot a device. Booting an already-booted device is idempotent success.
    pub async fn boot(&self, udid: &str) -> Result<(), SimtopError> {
        match self.run(&["boot", udid]).await {
            Ok(_) => Ok(()),
            Err(e) if is_state_conflict(&e, "Booted") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Shut down a device. Shutting down an already-shutdown device is
    /// idempotent success.
    pub async fn shutdown(&self, udid: &str) -> Result<(), SimtopError> {
        match self.run(&["shutdown", udid]).await {
            Ok(_) => Ok(()),
            Err(e) if is_state_conflict(&e, "Shutdown") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Create a device from identifiers; returns the new device.
    pub async fn create(
        &self,
        name: &str,
        device_type: &str,
        runtime: &str,
    ) -> Result<SimDevice, SimtopError> {
        let out = self.run(&["create", name, device_type, runtime]).await?;
        let udid = out.stdout.trim();
        if validate_udid(udid).is_err() {
            return Err(SimtopError::new(
                ErrorCode::ParseError,
                format!("simctl create did not report a valid UDID (stdout: {udid:?})"),
            ));
        }
        Ok(SimDevice {
            udid: udid.to_string(),
            name: name.to_string(),
            state: DeviceState::Creating,
            device_type: device_type.to_string(),
            runtime: runtime.to_string(),
            os_version: os_version_from_runtime(runtime),
            is_available: true,
        })
    }

    /// Delete a device.
    pub async fn delete(&self, udid: &str) -> Result<(), SimtopError> {
        self.run(&["delete", udid]).await.map(|_| ())
    }

    /// Install an `.app` bundle.
    pub async fn install(&self, udid: &str, app_path: &Path) -> Result<(), SimtopError> {
        if !app_path.is_dir() {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                format!("app bundle is not a directory: {}", app_path.display()),
            ));
        }
        if app_path.extension().map(|e| e != "app").unwrap_or(true) {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "app bundle must be a .app directory, got: {}",
                    app_path.display()
                ),
            ));
        }
        let path = app_path
            .to_str()
            .ok_or_else(|| {
                SimtopError::new(
                    ErrorCode::InvalidArgument,
                    format!("app path is not valid UTF-8: {}", app_path.display()),
                )
            })?
            .to_string();
        self.run(&["install", udid, &path]).await.map(|_| ())
    }

    /// Launch an app; the pid reported by simctl is parsed from stdout.
    pub async fn launch(&self, udid: &str, bundle_id: &str) -> Result<LaunchInfo, SimtopError> {
        let out = self.run(&["launch", udid, bundle_id]).await?;
        let line = out.stdout.trim();
        let pid = parse_launch_pid(line, bundle_id)?;
        Ok(LaunchInfo {
            udid: udid.to_string(),
            bundle_id: bundle_id.to_string(),
            pid: Some(pid),
        })
    }

    /// Terminate a running app.
    pub async fn terminate(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError> {
        self.run(&["terminate", udid, bundle_id]).await.map(|_| ())
    }

    /// Uninstall an app.
    pub async fn uninstall(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError> {
        self.run(&["uninstall", udid, bundle_id]).await.map(|_| ())
    }

    /// Open a URL inside the device.
    pub async fn open_url(&self, udid: &str, url: &str) -> Result<(), SimtopError> {
        self.run(&["openurl", udid, url]).await.map(|_| ())
    }

    /// Capture a PNG screenshot. Relative output paths are resolved against
    /// the current directory so the write target is deterministic.
    pub async fn screenshot(&self, udid: &str, out_path: &Path) -> Result<(), SimtopError> {
        let abs = if out_path.is_absolute() {
            out_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| {
                    SimtopError::with_source(
                        ErrorCode::IoError,
                        "cannot resolve current directory for screenshot".to_string(),
                        e,
                    )
                })?
                .join(out_path)
        };
        if let Some(parent) = abs.parent() {
            if !parent.is_dir() {
                return Err(SimtopError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "screenshot output directory does not exist: {}",
                        parent.display()
                    ),
                ));
            }
        }
        let path = abs.to_str().ok_or_else(|| {
            SimtopError::new(
                ErrorCode::InvalidArgument,
                format!("screenshot path is not valid UTF-8: {}", abs.display()),
            )
        })?;
        self.run(&["io", udid, "screenshot", path])
            .await
            .map(|_| ())
    }

    /// Snapshot of recent device log entries via `log show` inside the
    /// device.
    pub async fn logs(&self, udid: &str) -> Result<DeviceLog, SimtopError> {
        let out = self
            .run(&[
                "spawn", udid, "log", "show", "--last", LOG_LAST, "--style", "json",
            ])
            .await?;
        Ok(DeviceLog {
            udid: udid.to_string(),
            entries: parse_log_entries(&out.stdout)?,
        })
    }
}

/// Map a simctl failure to a stable error code based on stderr content.
fn classify_failure(stderr: &str) -> ErrorCode {
    if stderr.contains("Unable to find a device") || stderr.contains("Invalid UDID") {
        ErrorCode::DeviceNotFound
    } else {
        ErrorCode::CommandFailed
    }
}

/// simctl refuses boot/shutdown of a device already in the target state.
fn is_state_conflict(err: &SimtopError, state: &str) -> bool {
    err.code() == ErrorCode::CommandFailed
        && err.message().contains(&format!("current state: {state}"))
}

/// Parse `simctl list -j devices` JSON into domain models.
///
/// Shape: `{"devices": {"<runtime id>": [{"state", "isAvailable", "name",
/// "udid"}, …], …}}`. The device-type identifier is not part of this JSON, so
/// `device_type` stays empty on the simctl path.
fn parse_device_list(stdout: &str) -> Result<Vec<SimDevice>, SimtopError> {
    let value: serde_json::Value = serde_json::from_str(stdout).map_err(|e| {
        SimtopError::with_source(
            ErrorCode::ParseError,
            "simctl list -j devices returned invalid JSON".to_string(),
            e,
        )
    })?;
    let devices = value
        .get("devices")
        .and_then(|d| d.as_object())
        .ok_or_else(|| {
            SimtopError::new(
                ErrorCode::ParseError,
                "simctl list -j devices JSON is missing the \"devices\" object".to_string(),
            )
        })?;
    let mut result = Vec::new();
    for (runtime_id, entries) in devices {
        let entries = entries.as_array().ok_or_else(|| {
            SimtopError::new(
                ErrorCode::ParseError,
                format!("simctl list -j devices entry for {runtime_id:?} is not an array"),
            )
        })?;
        for entry in entries {
            let entry = entry.as_object().ok_or_else(|| {
                SimtopError::new(
                    ErrorCode::ParseError,
                    "simctl list -j devices entry is not an object".to_string(),
                )
            })?;
            let udid = entry
                .get("udid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    SimtopError::new(
                        ErrorCode::ParseError,
                        "simctl list -j devices entry is missing \"udid\"".to_string(),
                    )
                })?
                .to_string();
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let state = entry
                .get("state")
                .and_then(|v| v.as_str())
                .map(DeviceState::from)
                .unwrap_or_else(|| DeviceState::Unknown(String::new()));
            let is_available = entry
                .get("isAvailable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            result.push(SimDevice {
                udid,
                name,
                state,
                device_type: String::new(),
                runtime: runtime_id.clone(),
                os_version: os_version_from_runtime(runtime_id),
                is_available,
            });
        }
    }
    // Deterministic ordering for callers and snapshots.
    result.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.udid.cmp(&b.udid)));
    Ok(result)
}

/// Derive a display OS version from a runtime identifier:
/// `com.apple.CoreSimulator.SimRuntime.iOS-18-0` → `18.0`.
fn os_version_from_runtime(runtime: &str) -> String {
    const PREFIX: &str = "com.apple.CoreSimulator.SimRuntime.";
    let body = runtime.strip_prefix(PREFIX).unwrap_or(runtime);
    match body.split_once('-') {
        Some((_, version)) if !version.is_empty() => version.replace('-', "."),
        // A dangling separator with no version suffix must not leak through:
        // `…SimRuntime.iOS-` renders as `iOS`, not `iOS-`.
        Some((platform, _)) => platform.to_string(),
        _ => body.to_string(),
    }
}

/// Parse `simctl launch` stdout: `"<bundle id>: <pid>"`.
fn parse_launch_pid(line: &str, bundle_id: &str) -> Result<u32, SimtopError> {
    let expected = format!("{bundle_id}:");
    let rest = line.strip_prefix(&expected).map(str::trim).ok_or_else(|| {
        SimtopError::new(
            ErrorCode::ParseError,
            format!("simctl launch output did not match \"{expected} <pid>\": {line:?}"),
        )
    })?;
    rest.parse::<u32>().map_err(|_| {
        SimtopError::new(
            ErrorCode::ParseError,
            format!("simctl launch reported a non-numeric pid: {rest:?}"),
        )
    })
}

/// Parse `log show --style json`: either a single JSON array of entries or a
/// stream of one JSON object per line (defensive; both shapes occur across
/// Xcode versions). Entries missing a message are skipped. Unparseable
/// output is a `ParseError`, never a silent empty result.
fn parse_log_entries(stdout: &str) -> Result<Vec<LogEntry>, SimtopError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Array(arr)) => {
            for v in &arr {
                if let Some(e) = log_entry_from_value(v) {
                    entries.push(e);
                }
            }
            return Ok(entries);
        }
        // Non-array JSON (or a parse error): fall through to the
        // line-delimited shape.
        _ => {}
    }
    let mut saw_object = false;
    for line in trimmed.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(e) = log_entry_from_value(&v) {
                entries.push(e);
                saw_object = true;
            }
        }
    }
    if !saw_object {
        return Err(SimtopError::new(
            ErrorCode::ParseError,
            "log show output contained no parseable JSON entries".to_string(),
        ));
    }
    Ok(entries)
}

fn log_entry_from_value(v: &serde_json::Value) -> Option<LogEntry> {
    let obj = v.as_object()?;
    let message = obj.get("eventMessage")?.as_str()?.to_string();
    let timestamp = obj
        .get("timestamp")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let process = obj
        .get("process")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    let pid = obj
        .get("pid")
        .and_then(|p| p.as_u64())
        .and_then(|p| u32::try_from(p).ok());
    Some(LogEntry {
        timestamp,
        process,
        pid,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `simctl list -j devices` JSON with a single runtime key.
    fn device_list_json(runtime: &str, entries: &str) -> String {
        format!(r#"{{"devices": {{"{runtime}": {entries}}}}}"#)
    }

    /// A `log show --style json` entry object with the given message.
    fn log_json(message: &str) -> String {
        format!(
            r#"{{"eventMessage": {message:?}, "timestamp": "2026-08-08T00:00:00Z", "process": "SpringBoard", "pid": 42}}"#
        )
    }

    #[test]
    fn device_list_maps_states_runtimes_and_availability() {
        let json = r#"{
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-18-0": [
                    {"state": "Booted", "isAvailable": true, "name": "iPhone 18", "udid": "11111111-1111-1111-1111-111111111111"},
                    {"state": "Shutting Down", "isAvailable": true, "name": "iPad", "udid": "22222222-2222-2222-2222-222222222222"}
                ],
                "com.apple.CoreSimulator.SimRuntime.iOS-17-5": [
                    {"state": "Shutdown", "isAvailable": false, "name": "iPhone SE", "udid": "33333333-3333-3333-3333-333333333333"}
                ]
            }
        }"#;
        let devices = parse_device_list(json).unwrap();
        assert_eq!(devices.len(), 3);

        let booted = devices
            .iter()
            .find(|d| d.udid == "11111111-1111-1111-1111-111111111111")
            .unwrap();
        assert_eq!(booted.state, DeviceState::Booted);
        assert!(booted.is_available);
        assert_eq!(
            booted.runtime,
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0"
        );
        assert_eq!(booted.os_version, "18.0");
        // The device-type identifier is not part of simctl list JSON.
        assert_eq!(booted.device_type, "");

        let shutting_down = devices
            .iter()
            .find(|d| d.udid == "22222222-2222-2222-2222-222222222222")
            .unwrap();
        assert_eq!(shutting_down.state, DeviceState::ShuttingDown);

        let unavailable = devices
            .iter()
            .find(|d| d.udid == "33333333-3333-3333-3333-333333333333")
            .unwrap();
        assert_eq!(unavailable.state, DeviceState::Shutdown);
        assert!(!unavailable.is_available);
        assert_eq!(unavailable.os_version, "17.5");
    }

    #[test]
    fn device_list_sorts_deterministically() {
        let json = device_list_json(
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            r#"[
                {"state": "Shutdown", "isAvailable": true, "name": "iPhone 18", "udid": "22222222-2222-2222-2222-222222222222"},
                {"state": "Shutdown", "isAvailable": true, "name": "iPhone 16", "udid": "33333333-3333-3333-3333-333333333333"},
                {"state": "Shutdown", "isAvailable": true, "name": "iPhone 16", "udid": "11111111-1111-1111-1111-111111111111"}
            ]"#,
        );
        let devices = parse_device_list(&json).unwrap();
        let udids: Vec<&str> = devices.iter().map(|d| d.udid.as_str()).collect();
        assert_eq!(
            udids,
            [
                "11111111-1111-1111-1111-111111111111",
                "33333333-3333-3333-3333-333333333333",
                "22222222-2222-2222-2222-222222222222"
            ]
        );
    }

    #[test]
    fn device_list_defaults_missing_availability_and_state() {
        let json = device_list_json(
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            r#"[{"name": "iPhone", "udid": "11111111-1111-1111-1111-111111111111"}]"#,
        );
        let devices = parse_device_list(&json).unwrap();
        assert!(!devices[0].is_available);
        assert_eq!(devices[0].state, DeviceState::Unknown(String::new()));
    }

    #[test]
    fn device_list_keeps_unknown_states_verbatim() {
        let json = device_list_json(
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            r#"[{"state": "Quarantined", "isAvailable": true, "name": "iPhone", "udid": "11111111-1111-1111-1111-111111111111"}]"#,
        );
        let devices = parse_device_list(&json).unwrap();
        assert_eq!(
            devices[0].state,
            DeviceState::Unknown("Quarantined".to_string())
        );
    }

    #[test]
    fn device_list_accepts_empty_inventory_and_runtimes() {
        assert!(parse_device_list(r#"{"devices": {}}"#).unwrap().is_empty());
        let json = device_list_json("com.apple.CoreSimulator.SimRuntime.iOS-18-0", "[]");
        assert!(parse_device_list(&json).unwrap().is_empty());
    }

    #[test]
    fn device_list_rejects_invalid_json() {
        let err = parse_device_list("not json").unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn device_list_rejects_missing_devices_object() {
        let err = parse_device_list(r#"{"runtime": []}"#).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn device_list_rejects_non_array_runtime() {
        let json = device_list_json(
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            r#"{"not": "an array"}"#,
        );
        let err = parse_device_list(&json).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn device_list_rejects_entry_missing_udid() {
        let json = device_list_json(
            "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            r#"[{"state": "Booted", "isAvailable": true, "name": "iPhone"}]"#,
        );
        let err = parse_device_list(&json).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn os_version_strips_runtime_prefix() {
        assert_eq!(
            os_version_from_runtime("com.apple.CoreSimulator.SimRuntime.iOS-18-0"),
            "18.0"
        );
        assert_eq!(
            os_version_from_runtime("com.apple.CoreSimulator.SimRuntime.iOS-17-5"),
            "17.5"
        );
        assert_eq!(
            os_version_from_runtime("com.apple.CoreSimulator.SimRuntime.watchOS-11-0"),
            "11.0"
        );
    }

    #[test]
    fn os_version_falls_back_to_runtime_body() {
        assert_eq!(os_version_from_runtime("iOS-18-0"), "18.0");
        assert_eq!(
            os_version_from_runtime("com.apple.CoreSimulator.SimRuntime.iOS"),
            "iOS"
        );
        assert_eq!(
            os_version_from_runtime("com.apple.CoreSimulator.SimRuntime.iOS-"),
            "iOS"
        );
    }

    #[test]
    fn launch_pid_parses_bundle_prefix() {
        assert_eq!(
            parse_launch_pid("com.example.app: 1234", "com.example.app").unwrap(),
            1234
        );
    }

    #[test]
    fn launch_pid_rejects_mismatched_bundle() {
        let err = parse_launch_pid("com.other.app: 1234", "com.example.app").unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn launch_pid_rejects_non_numeric_pid() {
        assert_eq!(
            parse_launch_pid("com.example.app: abc", "com.example.app")
                .unwrap_err()
                .code(),
            ErrorCode::ParseError
        );
        assert_eq!(
            parse_launch_pid("com.example.app:", "com.example.app")
                .unwrap_err()
                .code(),
            ErrorCode::ParseError
        );
    }

    #[test]
    fn log_entries_parse_array_shape() {
        let stdout = format!("[{}, {}]", log_json("first"), log_json("second"));
        let entries = parse_log_entries(&stdout).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "first");
        assert_eq!(entries[0].process, "SpringBoard");
        assert_eq!(entries[0].pid, Some(42));
        assert_eq!(entries[0].timestamp, "2026-08-08T00:00:00Z");
        assert_eq!(entries[1].message, "second");
    }

    #[test]
    fn log_entries_parse_line_delimited_shape() {
        let stdout = format!("{}\n{}\n", log_json("one"), log_json("two"));
        let entries = parse_log_entries(&stdout).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "one");
        assert_eq!(entries[1].message, "two");
    }

    #[test]
    fn log_entries_skip_entries_without_message() {
        let stdout = format!(
            "[{}, {{\"timestamp\": \"2026-08-08T00:00:00Z\"}}, {}]",
            log_json("kept"),
            log_json("also kept")
        );
        let entries = parse_log_entries(&stdout).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "kept");
        assert_eq!(entries[1].message, "also kept");
    }

    #[test]
    fn log_entries_empty_output_is_empty() {
        assert!(parse_log_entries("").unwrap().is_empty());
        assert!(parse_log_entries("  \n\t ").unwrap().is_empty());
    }

    #[test]
    fn log_entries_reject_unparseable_output() {
        let err = parse_log_entries("this is not json").unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn log_entries_reject_json_without_entries() {
        let err = parse_log_entries(r#"{"not": "an entry"}"#).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ParseError);
    }

    #[test]
    fn failure_classification_maps_device_errors() {
        assert_eq!(
            classify_failure("Unable to find a device with UDID"),
            ErrorCode::DeviceNotFound
        );
        assert_eq!(classify_failure("Invalid UDID"), ErrorCode::DeviceNotFound);
        assert_eq!(
            classify_failure("some other failure"),
            ErrorCode::CommandFailed
        );
    }

    #[test]
    fn state_conflict_detects_target_state() {
        let booted = SimtopError::new(
            ErrorCode::CommandFailed,
            "simctl boot failed (exit 149): ... current state: Booted",
        );
        assert!(is_state_conflict(&booted, "Booted"));
        assert!(!is_state_conflict(&booted, "Shutdown"));

        let shutdown = SimtopError::new(ErrorCode::CommandFailed, "current state: Shutdown");
        assert!(is_state_conflict(&shutdown, "Shutdown"));

        // The same message under a different error code is not a conflict.
        let other = SimtopError::new(ErrorCode::DeviceNotFound, "current state: Booted");
        assert!(!is_state_conflict(&other, "Booted"));
    }
}
