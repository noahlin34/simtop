//! Human and `--json` output formatting (schema v1).
//!
//! Every command result flows through [`Output`], which enforces the stream
//! contract:
//!
//! - stdout carries exactly the command result: readable text by default, or
//!   one JSON envelope per result when `--json` is active;
//! - stderr carries diagnostics only (human-mode error messages; JSON-mode
//!   errors are themselves the command result and go to stdout);
//! - every JSON line is flushed immediately, so streaming commands
//!   (`watch`, `app logs --follow`) are safe to pipe;
//! - a closed stdout pipe (`simtop watch | head`) is treated as success and
//!   stops further output instead of erroring.
//!
//! # JSON envelopes
//!
//! Success: `{"schema":1,"command":"<name>","ok":true,"data":{...}}`
//! Error:   `{"schema":1,"command":"<name>","ok":false,"error":{...}}`
//!
//! The error object is the stable [`ErrorReport`]: `code` (machine code),
//! `message`, and the deterministic process `exit_code`.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use crate::error::{ErrorReport, SimtopError};
use crate::model::{DeviceLog, DeviceSnapshot, LaunchInfo, LogEntry, SimDevice, SCHEMA_VERSION};

/// Schema-v1 JSON envelope. `data` is present exactly when `ok` is true.
#[derive(Serialize)]
struct Envelope<'a, T> {
    schema: u32,
    command: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ErrorReport>,
}

/// `{"udid": ...}` payload for device lifecycle results.
#[derive(Serialize)]
struct UdidData {
    udid: String,
}

/// `{"udid": ..., "bundle_id": ...}` payload for terminate/uninstall results.
#[derive(Serialize)]
struct BundleData {
    udid: String,
    bundle_id: String,
}

/// `{"udid": ..., "app_path": ...}` payload for install results.
#[derive(Serialize)]
struct InstallData {
    udid: String,
    app_path: String,
}

/// `{"udid": ..., "url": ...}` payload for open-url results.
#[derive(Serialize)]
struct UrlData {
    udid: String,
    url: String,
}

/// `{"udid": ..., "path": ...}` payload for screenshot results.
#[derive(Serialize)]
struct ScreenshotData {
    udid: String,
    path: String,
}

/// Formatter for command results. Construct one per process with
/// [`Output::new`], mirroring the global `--json` flag.
pub struct Output {
    json: bool,
}

impl Output {
    /// Create an output formatter. `json` mirrors the `--json` flag.
    pub fn new(json: bool) -> Self {
        Output { json }
    }

    /// Whether `--json` is active.
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Emit a device snapshot (`list`/`watch` results).
    ///
    /// JSON: one envelope whose `data` is the full [`DeviceSnapshot`].
    /// Human: a table; `header` adds a generation/timestamp banner line
    /// (used by `watch` between refreshes).
    pub fn snapshot(&self, command: &str, snap: &DeviceSnapshot, header: bool) -> io::Result<()> {
        if self.json {
            return self.emit(command, snap);
        }
        if header {
            write_stdout_line(&format!(
                "== generation {} @ {} ==",
                snap.generation, snap.timestamp
            ))?;
        }
        write_stdout_line(&render_table(&snap.devices))
    }

    /// Emit a device lifecycle result (`boot`/`shutdown`/`open`/`delete`).
    pub fn udid_result(&self, command: &str, udid: &str, human: String) -> io::Result<()> {
        if self.json {
            self.emit(
                command,
                &UdidData {
                    udid: udid.to_string(),
                },
            )
        } else {
            write_stdout_line(&human)
        }
    }

    /// Emit a terminate/uninstall result.
    pub fn bundle_result(
        &self,
        command: &str,
        udid: &str,
        bundle_id: &str,
        human: String,
    ) -> io::Result<()> {
        if self.json {
            self.emit(
                command,
                &BundleData {
                    udid: udid.to_string(),
                    bundle_id: bundle_id.to_string(),
                },
            )
        } else {
            write_stdout_line(&human)
        }
    }

    /// Emit an install result.
    pub fn install_result(
        &self,
        command: &str,
        udid: &str,
        app_path: &Path,
        human: String,
    ) -> io::Result<()> {
        if self.json {
            self.emit(
                command,
                &InstallData {
                    udid: udid.to_string(),
                    app_path: app_path.display().to_string(),
                },
            )
        } else {
            write_stdout_line(&human)
        }
    }

    /// Emit an open-url result.
    pub fn url_result(
        &self,
        command: &str,
        udid: &str,
        url: &str,
        human: String,
    ) -> io::Result<()> {
        if self.json {
            self.emit(
                command,
                &UrlData {
                    udid: udid.to_string(),
                    url: url.to_string(),
                },
            )
        } else {
            write_stdout_line(&human)
        }
    }

    /// Emit a screenshot result.
    pub fn screenshot_result(
        &self,
        command: &str,
        udid: &str,
        path: &Path,
        human: String,
    ) -> io::Result<()> {
        if self.json {
            self.emit(
                command,
                &ScreenshotData {
                    udid: udid.to_string(),
                    path: path.display().to_string(),
                },
            )
        } else {
            write_stdout_line(&human)
        }
    }

    /// Emit a create result: the created device.
    pub fn created(&self, device: &SimDevice) -> io::Result<()> {
        if self.json {
            self.emit("create", device)
        } else {
            write_stdout_line(&format!("Created {} ({})", device.name, device.udid))
        }
    }

    /// Emit a launch result.
    pub fn launched(&self, info: &LaunchInfo) -> io::Result<()> {
        if self.json {
            self.emit("app launch", info)
        } else {
            let human = match info.pid {
                Some(pid) => format!("Launched {} on {} (pid {pid})", info.bundle_id, info.udid),
                None => format!("Launched {} on {}", info.bundle_id, info.udid),
            };
            write_stdout_line(&human)
        }
    }

    /// Emit a one-shot log snapshot (`app logs` without `--follow`).
    pub fn log(&self, command: &str, log: &DeviceLog) -> io::Result<()> {
        if self.json {
            self.emit(command, log)
        } else {
            write_log_entries(&log.entries)
        }
    }

    /// Emit new log entries (`app logs --follow`): one JSON envelope per
    /// poll whose `data` is a [`DeviceLog`] containing only the fresh
    /// entries; human mode prints each entry on its own line.
    pub fn log_entries(&self, command: &str, udid: &str, entries: &[LogEntry]) -> io::Result<()> {
        if self.json {
            let log = DeviceLog {
                udid: udid.to_string(),
                entries: entries.to_vec(),
            };
            self.emit(command, &log)
        } else {
            write_log_entries(entries)
        }
    }

    /// Report a command failure.
    ///
    /// JSON: the error envelope is the command result and goes to stdout.
    /// Human: the message is a diagnostic and goes to stderr, keeping
    /// stdout free for results.
    pub fn error(&self, command: &str, err: &SimtopError) -> io::Result<()> {
        if self.json {
            let report = err.report();
            let env = Envelope::<'_, ()> {
                schema: SCHEMA_VERSION,
                command,
                ok: false,
                data: None,
                error: Some(&report),
            };
            write_stdout_line(&to_json(&env)?)
        } else {
            write_stderr_line(&format!("simtop: {err}"))
        }
    }

    /// Emit a success envelope (JSON) with `data`.
    fn emit<T: Serialize>(&self, command: &str, data: &T) -> io::Result<()> {
        let env = Envelope {
            schema: SCHEMA_VERSION,
            command,
            ok: true,
            data: Some(data),
            error: None,
        };
        write_stdout_line(&to_json(&env)?)
    }
}

fn write_log_entries(entries: &[LogEntry]) -> io::Result<()> {
    for entry in entries {
        write_stdout_line(&render_log_entry(entry))?;
    }
    Ok(())
}

fn render_log_entry(entry: &LogEntry) -> String {
    match (entry.process.is_empty(), entry.pid) {
        (true, None) => format!("{} {}", entry.timestamp, entry.message),
        (true, Some(pid)) => format!("{} [{}] {}", entry.timestamp, pid, entry.message),
        (false, None) => format!("{} {} {}", entry.timestamp, entry.process, entry.message),
        (false, Some(pid)) => {
            format!(
                "{} {}[{}] {}",
                entry.timestamp, entry.process, pid, entry.message
            )
        }
    }
}

/// Render the human device table. Column widths follow the content; long
/// names/types are truncated to keep lines bounded.
fn render_table(devices: &[SimDevice]) -> String {
    if devices.is_empty() {
        return "(no simulators)".to_string();
    }
    let rows: Vec<(String, String, String, String, String)> = devices
        .iter()
        .map(|d| {
            let name = if d.is_available {
                d.name.clone()
            } else {
                format!("{} (unavailable)", d.name)
            };
            // "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro" ->
            // "iPhone-16-Pro"; keep the raw identifier when there is no dot.
            let device_type = d
                .device_type
                .rsplit('.')
                .next()
                .unwrap_or(&d.device_type)
                .to_string();
            (
                d.state.to_string(),
                name,
                d.udid.clone(),
                d.os_version.clone(),
                device_type,
            )
        })
        .collect();

    let w_state = rows
        .iter()
        .map(|r| r.0.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    let w_name = rows
        .iter()
        .map(|r| r.1.chars().count())
        .max()
        .unwrap_or(0)
        .max(4);
    let w_os = rows
        .iter()
        .map(|r| r.3.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    let w_type = rows
        .iter()
        .map(|r| r.4.chars().count())
        .max()
        .unwrap_or(0)
        .max(4);
    // UDIDs are fixed-width UUID strings.
    const UDID_W: usize = 36;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<w_state$}  {:<w_name$}  {:<UDID_W$}  {:<w_os$}  {:<w_type$}",
        "STATE", "NAME", "UDID", "OS", "TYPE"
    );
    for (state, name, udid, os, dtype) in &rows {
        let _ = writeln!(
            out,
            "{:<w_state$}  {:<w_name$}  {:<UDID_W$}  {:<w_os$}  {:<w_type$}",
            truncate(state, 16),
            truncate(name, 48),
            udid,
            truncate(os, 16),
            truncate(dtype, 40),
        );
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

fn to_json<T: Serialize>(value: &T) -> io::Result<String> {
    serde_json::to_string(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write one line to stdout and flush. A closed pipe stops output silently
/// (the consumer went away); every other I/O failure is reported.
fn write_stdout_line(line: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match writeln!(out, "{line}").and_then(|_| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}

/// Write one diagnostic line to stderr and flush.
fn write_stderr_line(line: &str) -> io::Result<()> {
    let mut err = io::stderr().lock();
    match writeln!(err, "{line}").and_then(|_| err.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::model::{DeviceSnapshot, DeviceState};

    fn sample_device() -> SimDevice {
        SimDevice {
            udid: "01234567-89AB-CDEF-0123-456789ABCDEF".to_string(),
            name: "iPhone 16 Pro".to_string(),
            state: DeviceState::Booted,
            device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro".to_string(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-18-0".to_string(),
            os_version: "18.0".to_string(),
            is_available: true,
        }
    }

    #[test]
    fn success_envelope_shape() {
        let data = UdidData {
            udid: "ABCD".to_string(),
        };
        let env = Envelope {
            schema: SCHEMA_VERSION,
            command: "boot",
            ok: true,
            data: Some(&data),
            error: None,
        };
        let json: serde_json::Value = serde_json::from_str(&to_json(&env).unwrap()).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["command"], "boot");
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["udid"], "ABCD");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn error_envelope_shape() {
        let err = SimtopError::new(ErrorCode::DeviceNotFound, "no such device");
        let report = err.report();
        let env = Envelope::<'_, ()> {
            schema: SCHEMA_VERSION,
            command: "boot",
            ok: false,
            data: None,
            error: Some(&report),
        };
        let json: serde_json::Value = serde_json::from_str(&to_json(&env).unwrap()).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["command"], "boot");
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "DEVICE_NOT_FOUND");
        assert_eq!(json["error"]["exit_code"], 8);
        assert!(json.get("data").is_none());
    }

    #[test]
    fn snapshot_table_lists_devices() {
        let snap = DeviceSnapshot::new(1, "2026-08-08T00:00:00Z", vec![sample_device()]);
        let out = Output::new(false);
        let table = render_table(&snap.devices);
        assert!(table.contains("iPhone 16 Pro"));
        assert!(table.contains("01234567-89AB-CDEF-0123-456789ABCDEF"));
        assert!(table.contains("Booted"));
        assert!(!out.is_json());
    }

    #[test]
    fn empty_table_placeholder() {
        let table = render_table(&[]);
        assert_eq!(table, "(no simulators)");
    }
}
