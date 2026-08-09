//! Hybrid backend: native CoreSimulator bridge where available, narrowly
//! typed `simctl` otherwise.
//!
//! Dispatch policy (per operation, in order):
//! 1. **Fallback**: when the bridge is not loaded, or it does not advertise
//!    the capability the operation needs, route to the matching typed simctl
//!    call (exactly one attempt).
//! 2. **Fallback once**: when the bridge is loaded and advertises the
//!    capability but the call reports [`NativeErrorCode::Unsupported`],
//!    route to simctl. The C bridge only reports `Unsupported` *before*
//!    touching CoreSimulator (missing class/selector or cleared capability
//!    bit), so the operation provably never executed natively and one simctl
//!    attempt cannot duplicate it.
//! 3. **Propagate**: any other native failure (CoreSimulator rejection,
//!    missing device, exception, bridge fault) is authoritative — the
//!    operation may or may not have executed — and is surfaced through
//!    [`map_native_error`], never retried.
//!
//! Discovery and lifecycle (list/boot/shutdown/create/delete) go through
//! [`NativeSimulator`] when the bridge covers them. Native calls are
//! synchronous C-ABI calls, so each one runs on a blocking thread via
//! `spawn_blocking`; the bridge serializes access internally, so concurrent
//! calls are safe. App operations (install/launch/terminate/uninstall), open
//! URL, screenshots, and logs go through the simctl client — the bridge has
//! no capabilities for them. `open` (Simulator UI) is implemented with
//! `/usr/bin/open` against the derived Simulator.app path.
//!
//! When the bridge cannot be loaded (framework missing, dylib error), the
//! backend degrades to simctl for everything — never to fake success.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::backend::simctl::SimctlClient;
use crate::backend::{
    validate_bundle_id, validate_identifier, validate_udid, validate_url, SimulatorBackend,
};
use crate::error::{ErrorCode, SimtopError};
use crate::model::{DeviceLog, DeviceSnapshot, DeviceState, LaunchInfo, SimDevice, SCHEMA_VERSION};
use crate::native::{
    CreateOptions, NativeCapabilities, NativeDevice, NativeDeviceState, NativeError,
    NativeErrorCode, NativeSimulator,
};
use crate::xcode::XcodeEnvironment;

/// First-release backend. `developer_dir` must already be resolved (see
/// [`crate::xcode::XcodeEnvironment::resolve`]); `timeout` bounds every
/// spawned subprocess.
pub struct HybridBackend {
    xcode: XcodeEnvironment,
    native: Option<Arc<NativeSimulator>>,
    simctl: SimctlClient,
    timeout: Duration,
    generation: AtomicU64,
}

impl HybridBackend {
    /// Construct the backend. Native bridge load failure is non-fatal: the
    /// backend degrades to the simctl client (a warning goes to stderr).
    /// This is safe because every operation the bridge could serve
    /// (discovery, boot, shutdown, create, delete) has a typed simctl
    /// fallback; the bridge is never a hard dependency.
    pub fn new(developer_dir: &Path, timeout: Duration) -> Result<Self, SimtopError> {
        if timeout.is_zero() {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                "backend timeout must be non-zero".to_string(),
            ));
        }
        let xcode = XcodeEnvironment::from_developer_dir(developer_dir)?;
        let simctl = SimctlClient::new(xcode.simctl_path().to_path_buf(), timeout);
        let native = match NativeSimulator::load(xcode.developer_dir()) {
            Ok(n) => Some(Arc::new(n)),
            Err(e) => {
                eprintln!(
                    "simtop: native CoreSimulator bridge unavailable ({}); using simctl",
                    e.message
                );
                None
            }
        };
        Ok(Self {
            xcode,
            native,
            simctl,
            timeout,
            generation: AtomicU64::new(0),
        })
    }

    /// Run a native bridge operation off the async runtime.
    ///
    /// Returns `None` when the caller must try the simctl fallback exactly
    /// once: the bridge is missing, `cap` is not advertised, or the native
    /// call reported [`NativeErrorCode::Unsupported`] (which the C bridge
    /// only produces *before* touching CoreSimulator, so nothing executed).
    /// Returns `Some(Err(..))` for every other native failure — those are
    /// authoritative and are surfaced through [`map_native_error`], never
    /// retried. A `spawn_blocking` join error also propagates, because the
    /// operation may already have executed.
    async fn native_call<T>(
        &self,
        cap: bool,
        op: &str,
        f: impl FnOnce(Arc<NativeSimulator>) -> Result<T, NativeError> + Send + 'static,
    ) -> Option<Result<T, SimtopError>>
    where
        T: Send + 'static,
    {
        let native = self.native.clone()?;
        if !cap {
            return None;
        }
        match tokio::task::spawn_blocking(move || f(native)).await {
            Err(join) => Some(Err(SimtopError::new(
                ErrorCode::Internal,
                format!("native {op} task failed: {join}"),
            ))),
            Ok(res) => match classify_native_result(res) {
                NativeAttempt::Fallback => None,
                NativeAttempt::Native(outcome) => Some(outcome),
            },
        }
    }

    /// Whether the loaded bridge advertises `cap` (never true without a
    /// bridge). Pure policy gate, shared with tests.
    fn has_cap(&self, cap: impl Fn(NativeCapabilities) -> bool) -> bool {
        capability_present(self.caps(), cap)
    }

    fn caps(&self) -> Option<NativeCapabilities> {
        self.native.as_ref().map(|n| n.capabilities())
    }

    /// Device list: native when the bridge advertises enumeration, else
    /// simctl.
    async fn list_devices_impl(&self) -> Result<Vec<SimDevice>, SimtopError> {
        if let Some(res) = self
            .native_call(self.has_cap(|c| c.enumerate), "list_devices", |n| {
                n.list_devices()
            })
            .await
        {
            return res.map(|devs| devs.into_iter().map(native_device_to_model).collect());
        }
        self.simctl.list_devices().await
    }
}
/// Whether the native bridge should serve an operation: only when it is
/// loaded AND advertises the required capability. `None` (no bridge) is
/// always a fallback — a load failure is nonfatal precisely because every
/// native-routed operation has a typed simctl fallback.
fn capability_present(
    caps: Option<NativeCapabilities>,
    cap: impl Fn(NativeCapabilities) -> bool,
) -> bool {
    match caps {
        Some(c) => cap(c),
        None => false,
    }
}

/// Outcome of a single native bridge attempt, decided by pure policy.
#[derive(Debug)]
enum NativeAttempt<T> {
    /// Nothing executed natively (capability absent or the bridge reported
    /// `Unsupported` before touching CoreSimulator): the caller should make
    /// exactly one simctl attempt.
    Fallback,
    /// The native attempt is authoritative: either it succeeded, or it
    /// failed operationally and the operation may have executed — surface
    /// the result, never retry.
    Native(Result<T, SimtopError>),
}

/// Classify a native call result per the dispatch policy: `Unsupported` is
/// the only code that selects the fallback, because the C bridge guarantees
/// it means the operation never ran. Every other failure is authoritative.
fn classify_native_result<T>(res: Result<T, NativeError>) -> NativeAttempt<T> {
    match res {
        Ok(value) => NativeAttempt::Native(Ok(value)),
        Err(e) if e.code == NativeErrorCode::Unsupported => NativeAttempt::Fallback,
        Err(e) => NativeAttempt::Native(Err(map_native_error(e))),
    }
}

#[async_trait]
impl SimulatorBackend for HybridBackend {
    async fn snapshot(&self) -> Result<DeviceSnapshot, SimtopError> {
        let devices = self.list_devices_impl().await?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(DeviceSnapshot {
            schema_version: SCHEMA_VERSION,
            generation,
            timestamp: now_rfc3339(),
            devices,
        })
    }

    async fn list_devices(&self) -> Result<Vec<SimDevice>, SimtopError> {
        self.snapshot().await.map(|s| s.devices)
    }

    async fn boot(&self, udid: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        let udid_owned = udid.to_string();
        if let Some(res) = self
            .native_call(self.has_cap(|c| c.boot), "boot", move |n| {
                n.boot(&udid_owned)
            })
            .await
        {
            return match res {
                // Booting an already-booted device is idempotent success.
                Err(e) if already_in_state(&e, "Booted") => Ok(()),
                r => r,
            };
        }
        self.simctl.boot(udid).await
    }

    async fn shutdown(&self, udid: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        let udid_owned = udid.to_string();
        if let Some(res) = self
            .native_call(self.has_cap(|c| c.shutdown), "shutdown", move |n| {
                n.shutdown(&udid_owned)
            })
            .await
        {
            return match res {
                // Shutting down an already-shutdown device is idempotent.
                Err(e) if already_in_state(&e, "Shutdown") => Ok(()),
                r => r,
            };
        }
        self.simctl.shutdown(udid).await
    }

    async fn open(&self, udid: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        let app = self.xcode.simulator_app_path();
        if !app.is_dir() {
            return Err(SimtopError::new(
                ErrorCode::InvalidDeveloperDir,
                format!("Simulator.app not found at {}", app.display()),
            ));
        }
        let app_str = app.to_str().ok_or_else(|| {
            SimtopError::new(
                ErrorCode::InvalidArgument,
                format!("Simulator.app path is not valid UTF-8: {}", app.display()),
            )
        })?;
        let mut cmd = tokio::process::Command::new("/usr/bin/open");
        cmd.arg("-n")
            .arg("-a")
            .arg(app_str)
            .arg("--args")
            .arg("-CurrentDeviceUDID")
            .arg(udid)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            SimtopError::with_source(
                ErrorCode::IoError,
                "failed to spawn /usr/bin/open".to_string(),
                e,
            )
        })?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Err(_elapsed) => {
                return Err(SimtopError::new(
                    ErrorCode::Timeout,
                    format!("open Simulator.app exceeded the {:?} timeout", self.timeout),
                ))
            }
            Ok(res) => res.map_err(|e| {
                SimtopError::with_source(
                    ErrorCode::IoError,
                    "failed to read /usr/bin/open output".to_string(),
                    e,
                )
            })?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SimtopError::new(
                ErrorCode::CommandFailed,
                format!(
                    "open Simulator.app failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            ));
        }
        Ok(())
    }

    async fn create(
        &self,
        name: &str,
        device_type: &str,
        runtime: &str,
    ) -> Result<SimDevice, SimtopError> {
        validate_identifier(
            device_type,
            "com.apple.CoreSimulator.SimDeviceType.",
            "device type",
        )?;
        validate_identifier(runtime, "com.apple.CoreSimulator.SimRuntime.", "runtime")?;
        if name.trim().is_empty() {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                "device name must not be empty".to_string(),
            ));
        }
        if let Some(res) = self
            .native_call(self.has_cap(|c| c.create), "create", {
                let name = name.to_string();
                let device_type = device_type.to_string();
                let runtime = runtime.to_string();
                move |n| {
                    n.create(&CreateOptions {
                        name,
                        device_type,
                        runtime: Some(runtime),
                    })
                }
            })
            .await
        {
            return res.map(native_device_to_model);
        }
        self.simctl.create(name, device_type, runtime).await
    }

    async fn delete(&self, udid: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        let udid_owned = udid.to_string();
        if let Some(res) = self
            .native_call(self.has_cap(|c| c.delete), "delete", move |n| {
                n.delete(&udid_owned)
            })
            .await
        {
            return res;
        }
        self.simctl.delete(udid).await
    }

    async fn install(&self, udid: &str, app_path: &Path) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        self.simctl.install(udid, app_path).await
    }

    async fn launch(&self, udid: &str, bundle_id: &str) -> Result<LaunchInfo, SimtopError> {
        validate_udid(udid)?;
        validate_bundle_id(bundle_id)?;
        self.simctl.launch(udid, bundle_id).await
    }

    async fn terminate(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        validate_bundle_id(bundle_id)?;
        self.simctl.terminate(udid, bundle_id).await
    }

    async fn uninstall(&self, udid: &str, bundle_id: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        validate_bundle_id(bundle_id)?;
        self.simctl.uninstall(udid, bundle_id).await
    }

    async fn open_url(&self, udid: &str, url: &str) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        validate_url(url)?;
        self.simctl.open_url(udid, url).await
    }

    async fn screenshot(&self, udid: &str, out_path: &Path) -> Result<(), SimtopError> {
        validate_udid(udid)?;
        self.simctl.screenshot(udid, out_path).await
    }

    async fn logs(&self, udid: &str) -> Result<DeviceLog, SimtopError> {
        validate_udid(udid)?;
        self.simctl.logs(udid).await
    }
}

/// Map a native bridge failure onto the shared error surface.
fn map_native_error(e: NativeError) -> SimtopError {
    let code = match &e.code {
        NativeErrorCode::Unsupported => ErrorCode::UnsupportedOperation,
        NativeErrorCode::FrameworkLoad => ErrorCode::NativeBridgeUnavailable,
        NativeErrorCode::DeviceSet | NativeErrorCode::Enumeration => ErrorCode::Internal,
        NativeErrorCode::Operation => ErrorCode::CommandFailed,
        NativeErrorCode::NotFound => ErrorCode::DeviceNotFound,
        NativeErrorCode::InvalidArg => ErrorCode::InvalidArgument,
        NativeErrorCode::Exception | NativeErrorCode::Alloc | NativeErrorCode::Unknown => {
            ErrorCode::Internal
        }
    };
    let message = match e.detail {
        Some(detail) => format!("native CoreSimulator: {} ({detail})", e.message),
        None => format!("native CoreSimulator: {}", e.message),
    };
    SimtopError::new(code, message)
}

/// simctl and the native CoreSimulator bridge both refuse boot/shutdown of a
/// device already in the target state; recognize either message shape and
/// treat that as idempotent success (the desired state already holds, and
/// nothing is retried).
fn already_in_state(err: &SimtopError, state: &str) -> bool {
    if err.code() != ErrorCode::CommandFailed {
        return false;
    }
    let msg = err.message().to_ascii_lowercase();
    let state = state.to_ascii_lowercase();
    // Message shapes seen across Xcode versions:
    // - simctl: "…current state: Booted…" (also inside "…in current state: …")
    // - CoreSimulator NSError localizedDescription (native bridge):
    //   "The current state of the device is Booted."
    [
        format!("current state: {state}"),
        format!("current state of the device is {state}"),
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// Map a native device record onto the domain model.
fn native_device_to_model(d: NativeDevice) -> SimDevice {
    SimDevice {
        udid: d.udid,
        name: d.name,
        state: match d.state {
            NativeDeviceState::Shutdown => DeviceState::Shutdown,
            NativeDeviceState::Booted => DeviceState::Booted,
            NativeDeviceState::Booting => DeviceState::Booting,
            NativeDeviceState::ShuttingDown => DeviceState::ShuttingDown,
            NativeDeviceState::Creating => DeviceState::Creating,
            // The bridge collapses unrecognized CoreSimulator states to
            // `Unknown`; keep a non-blank label so consumers never see a
            // silently emptied state.
            NativeDeviceState::Unknown => DeviceState::Unknown("Unknown".to_string()),
        },
        device_type: d.device_type.unwrap_or_default(),
        runtime: d
            .runtime
            .as_ref()
            .and_then(|r| r.identifier.clone())
            .unwrap_or_default(),
        os_version: d
            .runtime
            .as_ref()
            .and_then(|r| r.version.clone())
            .unwrap_or_default(),
        is_available: d.available,
    }
}

/// Current wall clock as an RFC 3339 UTC timestamp with millisecond
/// precision (no chrono dependency in the crate).
fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = (secs % 86_400) / 3_600;
    let minute = (secs % 3_600) / 60;
    let second = secs % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since epoch → (year, month, day). Howard Hinnant's civil calendar
/// algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::RuntimeInfo;

    fn native_err(code: NativeErrorCode) -> NativeError {
        NativeError::new(code, "boom")
    }

    #[test]
    fn map_native_error_assigns_stable_codes() {
        let cases = [
            (NativeErrorCode::InvalidArg, ErrorCode::InvalidArgument),
            (
                NativeErrorCode::FrameworkLoad,
                ErrorCode::NativeBridgeUnavailable,
            ),
            (
                NativeErrorCode::Unsupported,
                ErrorCode::UnsupportedOperation,
            ),
            (NativeErrorCode::DeviceSet, ErrorCode::Internal),
            (NativeErrorCode::Enumeration, ErrorCode::Internal),
            (NativeErrorCode::Operation, ErrorCode::CommandFailed),
            (NativeErrorCode::NotFound, ErrorCode::DeviceNotFound),
            (NativeErrorCode::Exception, ErrorCode::Internal),
            (NativeErrorCode::Alloc, ErrorCode::Internal),
            (NativeErrorCode::Unknown, ErrorCode::Internal),
        ];
        for (native, expected) in cases {
            let mapped = map_native_error(native_err(native));
            assert_eq!(mapped.code(), expected, "native code {native:?}");
            assert!(
                mapped.message().starts_with("native CoreSimulator: "),
                "message must stay attributed: {}",
                mapped.message()
            );
        }
    }

    #[test]
    fn map_native_error_keeps_detail_visible() {
        let mapped = map_native_error(NativeError {
            code: NativeErrorCode::Operation,
            message: "rejected".to_string(),
            detail: Some("domain=CoreSimulatorErrorDomain code=149".to_string()),
        });
        assert_eq!(mapped.code(), ErrorCode::CommandFailed);
        assert!(mapped.message().contains("rejected"));
        assert!(mapped
            .message()
            .contains("domain=CoreSimulatorErrorDomain code=149"));
    }

    #[test]
    fn classify_native_result_falls_back_only_on_unsupported() {
        // Success is authoritative.
        match classify_native_result::<u32>(Ok(7)) {
            NativeAttempt::Native(Ok(7)) => {}
            other => panic!("expected native success, got {other:?}"),
        }
        // Unsupported means the operation never ran: safe to fall back.
        match classify_native_result::<u32>(Err(native_err(NativeErrorCode::Unsupported))) {
            NativeAttempt::Fallback => {}
            other => panic!("expected fallback for Unsupported, got {other:?}"),
        }
        // Every operational failure is authoritative and must propagate:
        // the operation may have executed, so it must never be retried.
        for code in [
            NativeErrorCode::InvalidArg,
            NativeErrorCode::FrameworkLoad,
            NativeErrorCode::DeviceSet,
            NativeErrorCode::Enumeration,
            NativeErrorCode::Operation,
            NativeErrorCode::NotFound,
            NativeErrorCode::Exception,
            NativeErrorCode::Alloc,
            NativeErrorCode::Unknown,
        ] {
            match classify_native_result::<u32>(Err(native_err(code))) {
                NativeAttempt::Native(Err(_)) => {}
                other => panic!("expected propagated failure for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn capability_gate_selects_fallback_when_bridge_or_bit_missing() {
        let caps = NativeCapabilities {
            boot: true,
            ..NativeCapabilities::default()
        };
        // Bit set: native serves the operation.
        assert!(capability_present(Some(caps), |c| c.boot));
        // Bit clear: fallback even though the bridge is loaded.
        assert!(!capability_present(Some(caps), |c| c.shutdown));
        assert!(!capability_present(Some(caps), |c| c.enumerate));
        // No bridge: always fallback.
        assert!(!capability_present(None, |c| c.boot));
    }

    #[test]
    fn already_in_state_recognizes_both_message_shapes() {
        // simctl shape.
        let simctl = SimtopError::new(
            ErrorCode::CommandFailed,
            "simctl [\"boot\", \"u\"] failed (exit 149): Unable to boot device in current state: Booted"
                .to_string(),
        );
        assert!(already_in_state(&simctl, "Booted"));
        // CoreSimulator NSError shape (native bridge path).
        let native = SimtopError::new(
            ErrorCode::CommandFailed,
            "native CoreSimulator: The current state of the device is Booted. \
             (domain=CoreSimulatorErrorDomain code=149)"
                .to_string(),
        );
        assert!(already_in_state(&native, "Booted"));
        // Matching is case-insensitive.
        let mixed = SimtopError::new(
            ErrorCode::CommandFailed,
            "The current state of the device is booted.".to_string(),
        );
        assert!(already_in_state(&mixed, "Booted"));
    }

    #[test]
    fn already_in_state_rejects_other_failures() {
        // A different state name must not match.
        let other_state = SimtopError::new(
            ErrorCode::CommandFailed,
            "The current state of the device is Shutting Down.".to_string(),
        );
        assert!(!already_in_state(&other_state, "Booted"));
        // Non-CommandFailed codes never match.
        let not_found = SimtopError::new(
            ErrorCode::DeviceNotFound,
            "current state: Booted".to_string(),
        );
        assert!(!already_in_state(&not_found, "Booted"));
        // Unrelated operational failures stay visible.
        let refused = SimtopError::new(
            ErrorCode::CommandFailed,
            "native CoreSimulator: CoreSimulator failed to boot device (domain=... code=151)"
                .to_string(),
        );
        assert!(!already_in_state(&refused, "Booted"));
    }

    fn native_device(state: NativeDeviceState) -> NativeDevice {
        NativeDevice {
            udid: "a1b2c3d4-0000-0000-0000-000000000000".to_string(),
            name: "Test iPhone".to_string(),
            state,
            available: true,
            runtime: Some(RuntimeInfo {
                identifier: Some("com.apple.CoreSimulator.SimRuntime.iOS-18-0".to_string()),
                name: Some("iOS 18.0".to_string()),
                version: Some("18.0".to_string()),
                build: Some("22A3351".to_string()),
            }),
            device_type: Some("com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro".to_string()),
            device_type_name: Some("iPhone 16 Pro".to_string()),
        }
    }

    #[test]
    fn native_device_states_map_explicitly() {
        let cases = [
            (NativeDeviceState::Shutdown, DeviceState::Shutdown),
            (NativeDeviceState::Booted, DeviceState::Booted),
            (NativeDeviceState::Booting, DeviceState::Booting),
            (NativeDeviceState::ShuttingDown, DeviceState::ShuttingDown),
            (NativeDeviceState::Creating, DeviceState::Creating),
        ];
        for (native, expected) in cases {
            let model = native_device_to_model(native_device(native));
            assert_eq!(model.state, expected, "native state {native:?}");
        }
    }

    #[test]
    fn unknown_native_state_stays_explicit() {
        let model = native_device_to_model(native_device(NativeDeviceState::Unknown));
        assert_eq!(
            model.state,
            DeviceState::Unknown("Unknown".to_string()),
            "unknown state must not collapse to a blank payload"
        );
    }

    #[test]
    fn native_device_metadata_maps_onto_model() {
        let model = native_device_to_model(native_device(NativeDeviceState::Booted));
        assert_eq!(model.udid, "a1b2c3d4-0000-0000-0000-000000000000");
        assert_eq!(model.runtime, "com.apple.CoreSimulator.SimRuntime.iOS-18-0");
        assert_eq!(model.os_version, "18.0");
        assert_eq!(
            model.device_type,
            "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro"
        );
        assert!(model.is_available);
    }
}
