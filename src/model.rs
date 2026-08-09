//! Serializable domain models shared across the backend, CLI, and TUI.
//!
//! Every serializable payload carries the schema version ([`SCHEMA_VERSION`])
//! and stable identities: simulator devices are addressed by their UDID
//! string, apps by bundle identifier, runtimes by CoreSimulator identifier.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Schema version carried by all serializable payloads.
pub const SCHEMA_VERSION: u32 = 1;

/// Lifecycle state of a simulator device, mirroring CoreSimulator/simctl
/// state names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Booted,
    Booting,
    ShuttingDown,
    Shutdown,
    Creating,
    /// Any state not yet modeled, carrying the raw CoreSimulator string.
    Unknown(String),
}

impl DeviceState {
    /// Whether the device is fully booted.
    pub fn is_booted(&self) -> bool {
        matches!(self, DeviceState::Booted)
    }

    /// Whether the device is transitioning or booted (not shut down).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            DeviceState::Booted
                | DeviceState::Booting
                | DeviceState::ShuttingDown
                | DeviceState::Creating
        )
    }
}

impl From<&str> for DeviceState {
    fn from(raw: &str) -> Self {
        let key = raw.trim().to_ascii_lowercase();
        match key.as_str() {
            "booted" => DeviceState::Booted,
            "booting" => DeviceState::Booting,
            "shutting down" | "shuttingdown" => DeviceState::ShuttingDown,
            "shutdown" => DeviceState::Shutdown,
            "creating" => DeviceState::Creating,
            _ => DeviceState::Unknown(raw.trim().to_owned()),
        }
    }
}

impl From<String> for DeviceState {
    fn from(raw: String) -> Self {
        DeviceState::from(raw.as_str())
    }
}

impl fmt::Display for DeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceState::Booted => f.write_str("Booted"),
            DeviceState::Booting => f.write_str("Booting"),
            DeviceState::ShuttingDown => f.write_str("Shutting Down"),
            DeviceState::Shutdown => f.write_str("Shutdown"),
            DeviceState::Creating => f.write_str("Creating"),
            DeviceState::Unknown(raw) => f.write_str(raw),
        }
    }
}

/// A simulator device in the current device set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimDevice {
    /// Stable CoreSimulator UDID; the domain identity of the device.
    pub udid: String,
    /// User-visible name, e.g. "iPhone 16 Pro".
    pub name: String,
    pub state: DeviceState,
    /// CoreSimulator device-type identifier, e.g.
    /// "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro".
    pub device_type: String,
    /// CoreSimulator runtime identifier, e.g.
    /// "com.apple.CoreSimulator.SimRuntime.iOS-18-0".
    pub runtime: String,
    /// Runtime marketing version, e.g. "18.0".
    pub os_version: String,
    /// Whether the device is available for use (not deleted/invalid).
    pub is_available: bool,
}

/// A simulator runtime (OS version) installed in the device set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// CoreSimulator runtime identifier, e.g.
    /// "com.apple.CoreSimulator.SimRuntime.iOS-18-0".
    pub identifier: String,
    /// Display name, e.g. "iOS 18.0".
    pub name: String,
    /// Marketing version, e.g. "18.0".
    pub version: String,
    /// Build number, e.g. "22A3351".
    pub build: String,
    /// Platform family, e.g. "iOS", "tvOS", "watchOS", "visionOS".
    pub platform: String,
    pub is_available: bool,
    #[serde(default)]
    pub supported_device_types: Vec<String>,
}

/// An app installed on a simulator device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// UDID of the device the app is installed on.
    pub device_udid: String,
    /// Reverse-DNS bundle identifier.
    pub bundle_id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<String>,
    /// Installed bundle path on the simulator data volume.
    #[serde(default)]
    pub path: Option<String>,
    /// App data-container path on the simulator data volume.
    #[serde(default)]
    pub data_path: Option<String>,
}

/// A process running inside a simulator device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    /// UDID of the device the process runs on.
    pub device_udid: String,
    pub pid: u32,
    pub name: String,
    #[serde(default)]
    pub bundle_id: Option<String>,
    /// Unix timestamp (seconds) of process start, when known.
    #[serde(default)]
    pub start_time: Option<u64>,
}

/// Hardware capabilities of a simulator device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    /// Host architecture exposed to the device, e.g. "arm64" or "x86_64".
    pub arch: String,
    /// Device supports 64-bit processes.
    pub is_64_bit: bool,
    /// GPU family as reported by CoreSimulator, when known.
    #[serde(default)]
    pub gpu: Option<String>,
}

impl Default for DeviceCapabilities {
    fn default() -> Self {
        DeviceCapabilities {
            arch: String::new(),
            is_64_bit: false,
            gpu: None,
        }
    }
}

/// A point-in-time view of the simulator device set (schema v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub schema_version: u32,
    /// Monotonic generation counter; increments whenever the snapshot changes.
    pub generation: u64,
    /// Capture time (RFC 3339).
    pub timestamp: String,
    pub devices: Vec<SimDevice>,
}

impl DeviceSnapshot {
    /// Build a snapshot at `generation`/`timestamp`, stamped with
    /// [`SCHEMA_VERSION`].
    pub fn new(generation: u64, timestamp: impl Into<String>, devices: Vec<SimDevice>) -> Self {
        DeviceSnapshot {
            schema_version: SCHEMA_VERSION,
            generation,
            timestamp: timestamp.into(),
            devices,
        }
    }
}

/// Result of launching an app on a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchInfo {
    pub udid: String,
    pub bundle_id: String,
    /// Process id of the launched app, when the backend could observe it.
    pub pid: Option<u32>,
}

/// A single log line captured from a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Emission time (RFC 3339).
    pub timestamp: String,
    /// Emitting process name, empty when unknown.
    pub process: String,
    #[serde(default)]
    pub pid: Option<u32>,
    pub message: String,
}

/// Recent log lines for one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLog {
    pub udid: String,
    pub entries: Vec<LogEntry>,
}

/// Watch-style change notification derived from snapshot polling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SimulatorEvent {
    DeviceAdded(SimDevice),
    DeviceRemoved { udid: String },
    DeviceStateChanged { udid: String, state: DeviceState },
    Snapshot(DeviceSnapshot),
}
