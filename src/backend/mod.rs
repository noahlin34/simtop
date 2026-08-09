//! The `SimulatorBackend` contract and backend construction.
//!
//! [`SimulatorBackend`] is the single async interface the CLI and TUI use to
//! drive iOS Simulators. It is object-safe (all methods take `&self`, all
//! inputs are borrowed) so it can be stored as `Box<dyn SimulatorBackend>`.
//!
//! Watch-style refresh is expressed as polling [`SimulatorBackend::snapshot`]:
//! every call performs a full refresh and returns a [`DeviceSnapshot`] with a
//! monotonically increasing `generation`. Consumers needing a stream simply
//! poll at their own interval.
//!
//! The first release ships one implementation, [`HybridBackend`], which
//! prefers the dynamically loaded CoreSimulator bridge ([`crate::native`])
//! for discovery and lifecycle operations and falls back to a narrowly typed
//! `simctl` client ([`simctl::SimctlClient`]) for everything else. There is
//! no arbitrary simctl passthrough: every operation is a typed method with
//! validated, deterministic arguments.

pub mod hybrid;
pub mod simctl;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{ErrorCode, SimtopError};
use crate::model::{DeviceLog, DeviceSnapshot, LaunchInfo, SimDevice};

pub use hybrid::HybridBackend;

/// Async contract for iOS Simulator control. All UDIDs are UUID strings.
#[async_trait]
pub trait SimulatorBackend: Send + Sync {
    /// Full device snapshot: list plus schema version, generation, and a
    /// wall-clock timestamp. Every call performs a fresh refresh.
    async fn snapshot(&self) -> Result<DeviceSnapshot, SimtopError>;

    /// Convenience: current devices only (equivalent to `snapshot().devices`).
    async fn list_devices(&self) -> Result<Vec<SimDevice>, SimtopError>;

    /// Boot a device (idempotent for an already-booted device).
    async fn boot(&self, udid: &str) -> Result<(), SimtopError>;

    /// Shut down a device (idempotent for an already-shutdown device).
    async fn shutdown(&self, udid: &str) -> Result<(), SimtopError>;

    /// Open the Simulator UI focused on the device.
    async fn open(&self, udid: &str) -> Result<(), SimtopError>;

    /// Create a device. `device_type` and `runtime` are identifiers
    /// (e.g. `com.apple.CoreSimulator.SimDeviceType.iPhone-16` and
    /// `com.apple.CoreSimulator.SimRuntime.iOS-18-0`), not display names.
    async fn create(
        &self,
        name: &str,
        device_type: &str,
        runtime: &str,
    ) -> Result<SimDevice, SimtopError>;

    /// Delete a device.
    async fn delete(&self, udid: &str) -> Result<(), SimtopError>;

    /// Install an `.app` bundle into the device.
    async fn install(&self, udid: &str, app_path: &Path) -> Result<(), SimtopError>;

    /// Launch an installed app; returns its pid when the device reports one.
    async fn launch(&self, udid: &str, bundle_id: &str) -> Result<LaunchInfo, SimtopError>;

    /// Terminate a running app.
    async fn terminate(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError>;

    /// Uninstall an app from the device.
    async fn uninstall(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError>;

    /// Open a URL (http/https or custom scheme) inside the device.
    async fn open_url(&self, udid: &str, url: &str) -> Result<(), SimtopError>;

    /// Capture a PNG screenshot of the device screen to `out_path`.
    async fn screenshot(&self, udid: &str, out_path: &Path) -> Result<(), SimtopError>;

    /// Snapshot of recent device log entries.
    async fn logs(&self, udid: &str) -> Result<DeviceLog, SimtopError>;
}

/// Construct the first-release backend: native CoreSimulator where available,
/// narrowly typed `simctl` otherwise. `developer_dir` must already be resolved
/// (see [`crate::xcode::XcodeEnvironment::resolve`]); `timeout` bounds every
/// spawned subprocess.
pub fn connect(
    developer_dir: &Path,
    timeout: Duration,
) -> Result<Box<dyn SimulatorBackend>, SimtopError> {
    Ok(Box::new(HybridBackend::new(developer_dir, timeout)?))
}

// ---------------------------------------------------------------------------
// Deterministic input validation shared by every backend path. These keep
// device selection unambiguous (UDIDs only, never names or "booted") and
// prevent option injection into spawned processes.
// ---------------------------------------------------------------------------

/// A device UDID is a lowercase UUID string (8-4-4-4-12 hex).
pub(crate) fn validate_udid(udid: &str) -> Result<(), SimtopError> {
    let parts: Vec<&str> = udid.split('-').collect();
    let ok = parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            format!("invalid device UDID (expected a UUID, got {udid:?})"),
        ))
    }
}

/// Bundle identifiers are reverse-DNS-ish: `[A-Za-z0-9._-]+`, no leading or
/// trailing separator.
pub(crate) fn validate_bundle_id(bundle_id: &str) -> Result<(), SimtopError> {
    let ok = !bundle_id.is_empty()
        && bundle_id.len() <= 255
        && bundle_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && !bundle_id.starts_with('.')
        && !bundle_id.starts_with('-')
        && !bundle_id.starts_with('_')
        && !bundle_id.ends_with('.')
        && !bundle_id.ends_with('-')
        && !bundle_id.ends_with('_');
    if ok {
        Ok(())
    } else {
        Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            format!("invalid bundle identifier {bundle_id:?}"),
        ))
    }
}

/// A URL must carry an explicit scheme (`scheme:rest`) with no whitespace and
/// must not start with `-` (option injection guard).
pub(crate) fn validate_url(url: &str) -> Result<(), SimtopError> {
    let colon = match url.find(':') {
        Some(i) => i,
        None => {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                format!("invalid URL {url:?}: no scheme"),
            ))
        }
    };
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    let ok = colon > 0
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
        && colon + 1 < url.len()
        && !url.contains(char::is_whitespace)
        && !url.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            format!("invalid URL {url:?}"),
        ))
    }
}

/// CoreSimulator identifiers must carry the framework prefix so callers pass
/// identifiers, not display names or free-form strings.
pub(crate) fn validate_identifier(id: &str, prefix: &str, what: &str) -> Result<(), SimtopError> {
    let ok = id.len() > prefix.len()
        && id.starts_with(prefix)
        && !id[prefix.len()..].contains(char::is_whitespace);
    if ok {
        Ok(())
    } else {
        Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            format!("invalid {what} identifier {id:?} (expected a {prefix}… identifier)"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udid_accepts_canonical_uuid() {
        assert!(validate_udid("0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d").is_ok());
    }

    #[test]
    fn udid_rejects_malformed_shapes() {
        // Wrong group sizes: 8-4-4-4-12 hex groups only.
        assert!(validate_udid("0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5").is_err());
        assert!(validate_udid("0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d").is_err());
        assert!(validate_udid("0a1b2c3d-4e5f-6a7b-8c9d").is_err());
        // Non-hex characters.
        assert!(validate_udid("0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5g").is_err());
        assert!(validate_udid("0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4!5").is_err());
        assert!(validate_udid("").is_err());
    }

    #[test]
    fn udid_rejects_option_like_values() {
        // UDIDs are passed positionally to simctl; anything that could be
        // parsed as an option or a convenience name must never pass.
        assert!(validate_udid("--help").is_err());
        assert!(validate_udid("-x").is_err());
        assert!(validate_udid("booted").is_err());
    }

    #[test]
    fn bundle_id_accepts_reverse_dns_style() {
        assert!(validate_bundle_id("com.example.MyApp").is_ok());
        assert!(validate_bundle_id("com.example.app-2.extension").is_ok());
        assert!(validate_bundle_id("a").is_ok());
    }

    #[test]
    fn bundle_id_rejects_separator_edges() {
        assert!(validate_bundle_id(".com.example").is_err());
        assert!(validate_bundle_id("-com.example").is_err());
        assert!(validate_bundle_id("_com.example").is_err());
        assert!(validate_bundle_id("com.example.").is_err());
        assert!(validate_bundle_id("com.example-").is_err());
        assert!(validate_bundle_id("com.example_").is_err());
    }

    #[test]
    fn bundle_id_rejects_empty_whitespace_and_oversize() {
        assert!(validate_bundle_id("").is_err());
        assert!(validate_bundle_id("com example").is_err());
        assert!(validate_bundle_id(&"a".repeat(256)).is_err());
        // 255 characters is the documented ceiling.
        assert!(validate_bundle_id(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn url_accepts_http_and_custom_schemes() {
        assert!(validate_url("https://example.com/path?q=1#frag").is_ok());
        assert!(validate_url("myapp://deeplink/route?x=1").is_ok());
        assert!(validate_url("tel:+15551234567").is_ok());
    }

    #[test]
    fn url_rejects_missing_or_malformed_scheme() {
        assert!(validate_url("example.com/path").is_err());
        assert!(validate_url("1http://example.com").is_err());
        assert!(validate_url("ht_tp://example.com").is_err());
        assert!(validate_url("https:").is_err());
    }

    #[test]
    fn url_rejects_whitespace() {
        assert!(validate_url("https://exa mple.com").is_err());
        assert!(validate_url("https://example.com/a b").is_err());
        assert!(validate_url("my app://x").is_err());
    }

    #[test]
    fn url_rejects_option_injection() {
        // `simctl openurl` would treat a leading `-` as an option; the
        // validator is the injection boundary.
        assert!(validate_url("-https://example.com").is_err());
        assert!(validate_url("-ohttps://example.com").is_err());
        assert!(validate_url("--help").is_err());
    }

    #[test]
    fn identifier_requires_framework_prefix() {
        let prefix = "com.apple.CoreSimulator.SimDeviceType.";
        assert!(validate_identifier(
            "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro",
            prefix,
            "device type"
        )
        .is_ok());
        // Display names and bare strings are rejected.
        assert!(validate_identifier("iPhone 16 Pro", prefix, "device type").is_err());
        // The prefix alone carries no identifier.
        assert!(validate_identifier(prefix, prefix, "device type").is_err());
        // Whitespace is rejected so identifiers cannot smuggle extra args.
        assert!(validate_identifier(
            "com.apple.CoreSimulator.SimDeviceType.iPhone 16 Pro",
            prefix,
            "device type"
        )
        .is_err());
    }
}
