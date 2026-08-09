//! Safe Rust bindings for the native CoreSimulator bridge.
//!
//! This module is the Rust half of `native/SimtopCoreSimulator.{h,m}`, a
//! small Objective-C C-ABI bridge that drives Apple's private
//! CoreSimulator.framework through dynamic loading:
//!
//! - **No static private-framework linkage.** The framework is `dlopen(3)`'d
//!   at runtime from the resolved Xcode developer directory (with a fallback
//!   to `/Library/Developer/PrivateFrameworks`, where newer Xcode ships it).
//! - **No Objective-C crosses the FFI boundary.** The C side returns only
//!   plain C records with owned UTF-8 strings; this module converts them to
//!   owned Rust values and frees every record with its matching free
//!   function (RAII guards guarantee cleanup even on panic).
//! - **Capability degradation, never crashes.** Every class/selector is
//!   probed at load; operations a given Xcode cannot serve report a typed
//!   [`NativeError`] with code [`NativeErrorCode::Unsupported`] **before any
//!   CoreSimulator call is made**, so a caller may route that operation to
//!   the `simctl` fallback exactly once without risking a duplicated
//!   mutation. Callers should query [`NativeSimulator::capabilities`] before
//!   dispatch and fall back to `simctl` where native support is absent;
//!   every other failure is authoritative and must propagate.
//! - **Exceptions are contained.** Objective-C exceptions are caught inside
//!   the bridge and surface as [`NativeErrorCode::Exception`].
//!
//! Thread safety: [`NativeSimulator`] is `Send + Sync`; a mutex serializes
//! access to the underlying C handle, so the async backend can wrap calls in
//! `tokio::task::spawn_blocking` without further synchronization.
//!
//! The methods are intentionally synchronous: the backend wraps them in
//! `spawn_blocking`. `boot`/`shutdown` initiate the state transition and
//! return once CoreSimulator accepted it; the new state is visible in the
//! next [`NativeSimulator::list_devices`] snapshot.

use std::ffi::{CStr, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;

/// ABI version of the C bridge; must match `SIMTOP_ABI_VERSION` in the
/// header. Refuse to operate on mismatch.
pub const ABI_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// C ABI declarations (mirrors native/SimtopCoreSimulator.h)
// ---------------------------------------------------------------------------

#[cfg(not(simtop_no_native))]
mod ffi {
    #![allow(non_camel_case_types, dead_code)]
    use libc::size_t;
    use std::os::raw::{c_char, c_uint};

    pub const SIMTOP_ABI_VERSION: c_uint = 1;

    pub const SIMTOP_CAP_DEVICE_SET: c_uint = 1 << 0;
    pub const SIMTOP_CAP_ENUMERATE: c_uint = 1 << 1;
    pub const SIMTOP_CAP_BOOT: c_uint = 1 << 2;
    pub const SIMTOP_CAP_SHUTDOWN: c_uint = 1 << 3;
    pub const SIMTOP_CAP_CREATE: c_uint = 1 << 4;
    pub const SIMTOP_CAP_DELETE: c_uint = 1 << 5;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum simtop_error_code {
        SIMTOP_ERR_NONE = 0,
        SIMTOP_ERR_INVALID_ARG = 1,
        SIMTOP_ERR_FRAMEWORK_LOAD = 2,
        SIMTOP_ERR_UNSUPPORTED = 3,
        SIMTOP_ERR_DEVICE_SET = 4,
        SIMTOP_ERR_ENUMERATION = 5,
        SIMTOP_ERR_OPERATION = 6,
        SIMTOP_ERR_NOT_FOUND = 7,
        SIMTOP_ERR_EXCEPTION = 8,
        SIMTOP_ERR_ALLOC = 9,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum simtop_device_state {
        SIMTOP_STATE_UNKNOWN = 0,
        SIMTOP_STATE_SHUTDOWN = 1,
        SIMTOP_STATE_BOOTED = 2,
        SIMTOP_STATE_BOOTING = 3,
        SIMTOP_STATE_SHUTTING_DOWN = 4,
        SIMTOP_STATE_CREATING = 5,
    }

    #[repr(C)]
    pub struct simtop_error {
        pub code: simtop_error_code,
        pub message: *mut c_char,
        pub detail: *mut c_char,
    }

    #[repr(C)]
    pub struct simtop_device_set_info {
        pub path: *mut c_char,
        pub name: *mut c_char,
    }

    #[repr(C)]
    pub struct simtop_device {
        pub udid: *mut c_char,
        pub name: *mut c_char,
        pub state: simtop_device_state,
        pub available: bool,
        pub runtime_identifier: *mut c_char,
        pub runtime_name: *mut c_char,
        pub runtime_version: *mut c_char,
        pub runtime_build: *mut c_char,
        pub device_type_identifier: *mut c_char,
        pub device_type_name: *mut c_char,
    }

    #[repr(C)]
    pub struct simtop_device_list {
        pub count: size_t,
        pub devices: *mut simtop_device,
    }

    #[repr(C)]
    pub struct simtop_create_options {
        pub name: *const c_char,
        pub device_type: *const c_char,
        pub runtime: *const c_char,
    }

    #[repr(C)]
    pub struct simtop_handle {
        _private: [u8; 0],
    }

    extern "C" {
        pub fn simtop_abi_version() -> c_uint;
        pub fn simtop_create(
            developer_dir: *const c_char,
            out_error: *mut *mut simtop_error,
        ) -> *mut simtop_handle;
        pub fn simtop_destroy(handle: *mut simtop_handle);
        pub fn simtop_capabilities(handle: *const simtop_handle) -> c_uint;

        pub fn simtop_copy_device_set_info(
            handle: *const simtop_handle,
            out_error: *mut *mut simtop_error,
        ) -> *mut simtop_device_set_info;
        pub fn simtop_device_set_info_free(info: *mut simtop_device_set_info);

        pub fn simtop_list_devices(
            handle: *const simtop_handle,
            out_error: *mut *mut simtop_error,
        ) -> *mut simtop_device_list;
        pub fn simtop_device_for_udid(
            handle: *const simtop_handle,
            udid: *const c_char,
            out_error: *mut *mut simtop_error,
        ) -> *mut simtop_device;
        pub fn simtop_device_list_free(list: *mut simtop_device_list);
        pub fn simtop_device_free(device: *mut simtop_device);

        pub fn simtop_boot_device(
            handle: *mut simtop_handle,
            udid: *const c_char,
            out_error: *mut *mut simtop_error,
        ) -> simtop_error_code;
        pub fn simtop_shutdown_device(
            handle: *mut simtop_handle,
            udid: *const c_char,
            out_error: *mut *mut simtop_error,
        ) -> simtop_error_code;
        pub fn simtop_create_device(
            handle: *mut simtop_handle,
            options: *const simtop_create_options,
            out_error: *mut *mut simtop_error,
        ) -> *mut simtop_device;
        pub fn simtop_delete_device(
            handle: *mut simtop_handle,
            udid: *const c_char,
            out_error: *mut *mut simtop_error,
        ) -> simtop_error_code;

        pub fn simtop_error_free(error: *mut simtop_error);
    }
}

// ---------------------------------------------------------------------------
// Shared public types
// ---------------------------------------------------------------------------

/// Stable machine codes produced by the native bridge. Values are fixed and
/// shared with the C side; the hybrid backend maps them onto
/// `crate::error::SimtopError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NativeErrorCode {
    /// A NULL/empty argument (developer dir, UDID, ...).
    InvalidArg = 1,
    /// CoreSimulator.framework could not be loaded from either known path.
    FrameworkLoad = 2,
    /// The loaded Xcode lacks the API for the requested operation
    /// (capability bit clear — query [`NativeSimulator::capabilities`]).
    /// Guaranteed to be reported before the operation touches CoreSimulator,
    /// so a caller may safely retry the operation once through `simctl`.
    Unsupported = 3,
    /// Device set discovery/resolution failed.
    DeviceSet = 4,
    /// Device enumeration failed.
    Enumeration = 5,
    /// CoreSimulator rejected an operation (carries its NSError message).
    Operation = 6,
    /// Device / device type / runtime not found.
    NotFound = 7,
    /// An Objective-C exception was caught at the bridge boundary.
    Exception = 8,
    /// Out of memory while building a C record.
    Alloc = 9,
    /// An unrecognized code from the bridge (a future ABI revision). Never
    /// emitted by the current C side; kept distinct so unknown failures are
    /// surfaced as internal instead of masquerading as operational errors.
    Unknown = 10,
}

impl NativeErrorCode {
    fn from_raw(raw: i32) -> NativeErrorCode {
        match raw {
            1 => NativeErrorCode::InvalidArg,
            2 => NativeErrorCode::FrameworkLoad,
            3 => NativeErrorCode::Unsupported,
            4 => NativeErrorCode::DeviceSet,
            5 => NativeErrorCode::Enumeration,
            6 => NativeErrorCode::Operation,
            7 => NativeErrorCode::NotFound,
            8 => NativeErrorCode::Exception,
            9 => NativeErrorCode::Alloc,
            _ => NativeErrorCode::Unknown,
        }
    }
}

/// A typed native-bridge failure. `message` is always populated; `detail`
/// carries extra context (dlerror text, NSError domain/code, exception name)
/// when the C side supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeError {
    pub code: NativeErrorCode,
    pub message: String,
    pub detail: Option<String>,
}

impl NativeError {
    pub fn new(code: NativeErrorCode, message: impl Into<String>) -> Self {
        NativeError {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// Convenience constructor for operations gated behind a capability bit.
    pub fn unsupported(operation: &str) -> Self {
        NativeError::new(
            NativeErrorCode::Unsupported,
            format!("{operation} is not supported by this Xcode's CoreSimulator"),
        )
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({})", self.message, detail),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for NativeError {}

/// Capability report for the loaded Xcode. Every native operation maps to
/// exactly one bit; operations whose bit is clear return
/// [`NativeErrorCode::Unsupported`] and should be routed to the `simctl`
/// fallback instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeCapabilities {
    /// Device set resolved; [`NativeSimulator::device_set`] is usable.
    pub device_set: bool,
    /// [`NativeSimulator::list_devices`] / [`NativeSimulator::device`] usable.
    pub enumerate: bool,
    /// [`NativeSimulator::boot`] usable.
    pub boot: bool,
    /// [`NativeSimulator::shutdown`] usable.
    pub shutdown: bool,
    /// [`NativeSimulator::create`] usable (device-type and runtime registries
    /// available in the loaded Xcode).
    pub create: bool,
    /// [`NativeSimulator::delete`] usable.
    pub delete: bool,
}

impl NativeCapabilities {
    pub fn any(&self) -> bool {
        self.device_set
            || self.enumerate
            || self.boot
            || self.shutdown
            || self.create
            || self.delete
    }

    pub fn none(&self) -> bool {
        !self.any()
    }
}

/// Device state as reported by CoreSimulator. Unknown covers anything a
/// future Xcode may add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeDeviceState {
    Unknown,
    Shutdown,
    Booted,
    Booting,
    ShuttingDown,
    Creating,
}

impl NativeDeviceState {
    fn from_raw(raw: i32) -> NativeDeviceState {
        match raw {
            1 => NativeDeviceState::Shutdown,
            2 => NativeDeviceState::Booted,
            3 => NativeDeviceState::Booting,
            4 => NativeDeviceState::ShuttingDown,
            5 => NativeDeviceState::Creating,
            _ => NativeDeviceState::Unknown,
        }
    }
}

impl fmt::Display for NativeDeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            NativeDeviceState::Unknown => "Unknown",
            NativeDeviceState::Shutdown => "Shutdown",
            NativeDeviceState::Booted => "Booted",
            NativeDeviceState::Booting => "Booting",
            NativeDeviceState::ShuttingDown => "Shutting Down",
            NativeDeviceState::Creating => "Creating",
        };
        f.write_str(s)
    }
}

/// Runtime metadata attached to a device (may be absent for unavailable
/// devices or older Xcode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// e.g. `com.apple.CoreSimulator.SimRuntime.iOS-18-0`
    pub identifier: Option<String>,
    /// e.g. `iOS 18.0`
    pub name: Option<String>,
    /// e.g. `18.0`
    pub version: Option<String>,
    /// e.g. `22A3351`
    pub build: Option<String>,
}

/// One simulator device from the default device set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDevice {
    /// UDID string (the domain identifier used across simtop).
    pub udid: String,
    pub name: String,
    pub state: NativeDeviceState,
    /// False when the device has no installed runtime (CoreSimulator marks
    /// these unavailable); default true on Xcode versions without the flag.
    pub available: bool,
    pub runtime: Option<RuntimeInfo>,
    /// SimDeviceType identifier, e.g. `com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro`.
    pub device_type: Option<String>,
    pub device_type_name: Option<String>,
}

/// Snapshot of the default device set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSetInfo {
    /// Set directory; `None` on Xcode versions that do not expose it.
    pub path: Option<PathBuf>,
    /// Set display name; `None` when the Xcode version has no name.
    pub name: Option<String>,
}

/// Parameters for [`NativeSimulator::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOptions {
    /// Display name for the new device.
    pub name: String,
    /// SimDeviceType identifier (required).
    pub device_type: String,
    /// SimRuntime identifier; `None` lets CoreSimulator choose the default
    /// runtime for the device type.
    pub runtime: Option<String>,
}

// ---------------------------------------------------------------------------
// Real implementation (macOS)
// ---------------------------------------------------------------------------

/// RAII guard: frees a C error record even if a later read panics.
#[cfg(not(simtop_no_native))]
struct ErrorGuard(*mut ffi::simtop_error);

#[cfg(not(simtop_no_native))]
impl Drop for ErrorGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::simtop_error_free(self.0) };
        }
    }
}

/// RAII guard for a `simtop_device_list` record.
#[cfg(not(simtop_no_native))]
struct DeviceListGuard(*mut ffi::simtop_device_list);

#[cfg(not(simtop_no_native))]
impl Drop for DeviceListGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::simtop_device_list_free(self.0) };
        }
    }
}

/// RAII guard for a single `simtop_device` record.
#[cfg(not(simtop_no_native))]
struct DeviceGuard(*mut ffi::simtop_device);

#[cfg(not(simtop_no_native))]
impl Drop for DeviceGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::simtop_device_free(self.0) };
        }
    }
}

/// RAII guard for a `simtop_device_set_info` record.
#[cfg(not(simtop_no_native))]
struct SetInfoGuard(*mut ffi::simtop_device_set_info);

#[cfg(not(simtop_no_native))]
impl Drop for SetInfoGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::simtop_device_set_info_free(self.0) };
        }
    }
}

/// Opaque ownership of the C handle. `Send` is sound because every access is
/// serialized through the mutex in [`NativeSimulator`] and the handle is
/// destroyed exactly once in `Drop`.
#[cfg(not(simtop_no_native))]
struct NativeHandle(*mut ffi::simtop_handle);

#[cfg(not(simtop_no_native))]
unsafe impl Send for NativeHandle {}

/// Safe, RAII wrapper over the CoreSimulator bridge.
///
/// Construct with [`NativeSimulator::load`]; the handle is closed on drop.
/// All methods are synchronous C calls serialized internally — safe to call
/// from multiple threads, and suitable for `tokio::task::spawn_blocking`.
#[cfg(not(simtop_no_native))]
pub struct NativeSimulator {
    inner: Mutex<NativeHandle>,
}

#[cfg(not(simtop_no_native))]
impl NativeSimulator {
    /// Load CoreSimulator.framework for the given *resolved* Xcode developer
    /// directory and resolve the default device set.
    ///
    /// Fails with [`NativeErrorCode::FrameworkLoad`] when the framework is
    /// missing from both known locations — callers should treat that as
    /// "native unavailable" and route everything to the `simctl` fallback.
    /// Device-set resolution failure does *not* fail the load; it clears the
    /// `device_set`/`enumerate` capability bits instead.
    pub fn load(developer_dir: &Path) -> Result<NativeSimulator, NativeError> {
        unsafe {
            if ffi::simtop_abi_version() != ABI_VERSION {
                return Err(NativeError::new(
                    NativeErrorCode::Unsupported,
                    format!(
                        "native bridge ABI mismatch: Rust expects {ABI_VERSION}, C reports {}",
                        ffi::simtop_abi_version()
                    ),
                ));
            }
            let dir = CString::new(developer_dir.as_os_str().as_encoded_bytes()).map_err(|_| {
                NativeError::new(
                    NativeErrorCode::InvalidArg,
                    "developer directory contains a NUL byte",
                )
            })?;
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let handle = ffi::simtop_create(dir.as_ptr(), &mut err);
            if handle.is_null() {
                return Err(Self::take_error(err));
            }
            Ok(NativeSimulator {
                inner: Mutex::new(NativeHandle(handle)),
            })
        }
    }

    /// Capability report for the loaded Xcode. Always safe to call; the
    /// report never changes for the lifetime of the handle.
    pub fn capabilities(&self) -> NativeCapabilities {
        let guard = self.inner.lock().expect("native mutex poisoned");
        let flags = unsafe { ffi::simtop_capabilities(guard.0) };
        NativeCapabilities {
            device_set: flags & ffi::SIMTOP_CAP_DEVICE_SET != 0,
            enumerate: flags & ffi::SIMTOP_CAP_ENUMERATE != 0,
            boot: flags & ffi::SIMTOP_CAP_BOOT != 0,
            shutdown: flags & ffi::SIMTOP_CAP_SHUTDOWN != 0,
            create: flags & ffi::SIMTOP_CAP_CREATE != 0,
            delete: flags & ffi::SIMTOP_CAP_DELETE != 0,
        }
    }

    /// Info about the default device set.
    pub fn device_set(&self) -> Result<DeviceSetInfo, NativeError> {
        let guard = self.inner.lock().expect("native mutex poisoned");
        unsafe {
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let info = ffi::simtop_copy_device_set_info(guard.0, &mut err);
            if info.is_null() {
                return Err(Self::take_error(err));
            }
            let _info_guard = SetInfoGuard(info);
            Ok(DeviceSetInfo {
                path: read_cstr((*info).path).map(PathBuf::from),
                name: read_cstr((*info).name),
            })
        }
    }

    /// Snapshot of every device in the default set (including unavailable).
    pub fn list_devices(&self) -> Result<Vec<NativeDevice>, NativeError> {
        let guard = self.inner.lock().expect("native mutex poisoned");
        unsafe {
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let list = ffi::simtop_list_devices(guard.0, &mut err);
            if list.is_null() {
                return Err(Self::take_error(err));
            }
            let _list_guard = DeviceListGuard(list);
            let count = (*list).count;
            let devices = std::slice::from_raw_parts((*list).devices, count);
            Ok(devices.iter().map(|d| read_device(d)).collect())
        }
    }

    /// Copy of a single device looked up by UDID (case-insensitive).
    pub fn device(&self, udid: &str) -> Result<NativeDevice, NativeError> {
        let udid = cstring_arg(udid, "UDID")?;
        let guard = self.inner.lock().expect("native mutex poisoned");
        unsafe {
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let dev = ffi::simtop_device_for_udid(guard.0, udid.as_ptr(), &mut err);
            if dev.is_null() {
                return Err(Self::take_error(err));
            }
            let _dev_guard = DeviceGuard(dev);
            Ok(read_device(&*dev))
        }
    }

    /// Initiate boot of the device. Returns once CoreSimulator accepted the
    /// transition; poll [`NativeSimulator::list_devices`] for completion.
    pub fn boot(&self, udid: &str) -> Result<(), NativeError> {
        self.state_op(udid, ffi::simtop_boot_device)
    }

    /// Initiate shutdown of the device.
    pub fn shutdown(&self, udid: &str) -> Result<(), NativeError> {
        self.state_op(udid, ffi::simtop_shutdown_device)
    }

    /// Delete a device from the default set.
    pub fn delete(&self, udid: &str) -> Result<(), NativeError> {
        self.state_op(udid, ffi::simtop_delete_device)
    }

    /// Create a device and return its record (including the new UDID).
    pub fn create(&self, options: &CreateOptions) -> Result<NativeDevice, NativeError> {
        let name = cstring_arg(&options.name, "device name")?;
        let device_type = cstring_arg(&options.device_type, "device type")?;
        let runtime = match &options.runtime {
            Some(r) => Some(cstring_arg(r, "runtime")?),
            None => None,
        };
        let c_options = ffi::simtop_create_options {
            name: name.as_ptr(),
            device_type: device_type.as_ptr(),
            runtime: runtime.as_ref().map_or(ptr::null(), |r| r.as_ptr()),
        };
        let guard = self.inner.lock().expect("native mutex poisoned");
        unsafe {
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let dev = ffi::simtop_create_device(guard.0, &c_options, &mut err);
            if dev.is_null() {
                return Err(Self::take_error(err));
            }
            let _dev_guard = DeviceGuard(dev);
            Ok(read_device(&*dev))
        }
    }

    /// Consumes an out-parameter error record (frees it) or synthesizes one.
    unsafe fn take_error(err: *mut ffi::simtop_error) -> NativeError {
        if err.is_null() {
            return NativeError::new(
                NativeErrorCode::Operation,
                "native bridge failed without an error record",
            );
        }
        let _guard = ErrorGuard(err);
        let code = NativeErrorCode::from_raw((*err).code as i32);
        let message = read_cstr((*err).message)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("CoreSimulator error (code {})", (*err).code as i32));
        NativeError {
            code,
            message,
            detail: read_cstr((*err).detail),
        }
    }

    /// Shared plumbing for boot/shutdown/delete (same C signature).
    fn state_op(
        &self,
        udid: &str,
        op: unsafe extern "C" fn(
            *mut ffi::simtop_handle,
            *const std::os::raw::c_char,
            *mut *mut ffi::simtop_error,
        ) -> ffi::simtop_error_code,
    ) -> Result<(), NativeError> {
        let udid = cstring_arg(udid, "UDID")?;
        let guard = self.inner.lock().expect("native mutex poisoned");
        unsafe {
            let mut err: *mut ffi::simtop_error = ptr::null_mut();
            let code = op(guard.0, udid.as_ptr(), &mut err);
            if code == ffi::simtop_error_code::SIMTOP_ERR_NONE {
                if !err.is_null() {
                    ffi::simtop_error_free(err); // defensive: success carries no error
                }
                return Ok(());
            }
            Err(Self::take_error(err))
        }
    }
}

#[cfg(not(simtop_no_native))]
impl Drop for NativeSimulator {
    fn drop(&mut self) {
        let NativeHandle(handle) = *self.inner.get_mut().expect("native mutex poisoned");
        if !handle.is_null() {
            unsafe { ffi::simtop_destroy(handle) };
        }
    }
}

// ---------------------------------------------------------------------------
// Stub implementation (non-macOS builds)
// ---------------------------------------------------------------------------

/// Stub used when the bridge was not compiled (`simtop_no_native`, i.e.
/// non-macOS targets). Every operation reports [`NativeErrorCode::Unsupported`].
#[cfg(simtop_no_native)]
pub struct NativeSimulator {
    _private: (),
}

#[cfg(simtop_no_native)]
impl NativeSimulator {
    pub fn load(developer_dir: &Path) -> Result<NativeSimulator, NativeError> {
        let _ = developer_dir;
        Err(NativeError::new(
            NativeErrorCode::Unsupported,
            "the native CoreSimulator bridge is not compiled on this platform",
        ))
    }

    pub fn capabilities(&self) -> NativeCapabilities {
        NativeCapabilities::default()
    }

    pub fn device_set(&self) -> Result<DeviceSetInfo, NativeError> {
        Err(NativeError::unsupported("device set discovery"))
    }

    pub fn list_devices(&self) -> Result<Vec<NativeDevice>, NativeError> {
        Err(NativeError::unsupported("device enumeration"))
    }

    pub fn device(&self, udid: &str) -> Result<NativeDevice, NativeError> {
        let _ = udid;
        Err(NativeError::unsupported("device lookup"))
    }

    pub fn boot(&self, udid: &str) -> Result<(), NativeError> {
        let _ = udid;
        Err(NativeError::unsupported("boot"))
    }

    pub fn shutdown(&self, udid: &str) -> Result<(), NativeError> {
        let _ = udid;
        Err(NativeError::unsupported("shutdown"))
    }

    pub fn delete(&self, udid: &str) -> Result<(), NativeError> {
        let _ = udid;
        Err(NativeError::unsupported("delete"))
    }

    pub fn create(&self, options: &CreateOptions) -> Result<NativeDevice, NativeError> {
        let _ = options;
        Err(NativeError::unsupported("create"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Converts an owned C string to `String`, tolerating NULL (`None`).
unsafe fn read_cstr(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
}

/// Converts a Rust string to a NUL-terminated C string.
fn cstring_arg(s: &str, what: &str) -> Result<CString, NativeError> {
    CString::new(s).map_err(|_| {
        NativeError::new(
            NativeErrorCode::InvalidArg,
            format!("{what} contains a NUL byte"),
        )
    })
}

/// Copies a C `simtop_device` record into an owned Rust value. The C record
/// stays owned by its guard; all strings are duplicated.
#[cfg(not(simtop_no_native))]
unsafe fn read_device(d: *const ffi::simtop_device) -> NativeDevice {
    let runtime_identifier = read_cstr((*d).runtime_identifier);
    let runtime_name = read_cstr((*d).runtime_name);
    let runtime_version = read_cstr((*d).runtime_version);
    let runtime_build = read_cstr((*d).runtime_build);
    let runtime = if runtime_identifier.is_none()
        && runtime_name.is_none()
        && runtime_version.is_none()
        && runtime_build.is_none()
    {
        None
    } else {
        Some(RuntimeInfo {
            identifier: runtime_identifier,
            name: runtime_name,
            version: runtime_version,
            build: runtime_build,
        })
    };
    NativeDevice {
        udid: read_cstr((*d).udid).unwrap_or_default(),
        name: read_cstr((*d).name).unwrap_or_default(),
        state: NativeDeviceState::from_raw((*d).state as i32),
        available: (*d).available,
        runtime,
        device_type: read_cstr((*d).device_type_identifier),
        device_type_name: read_cstr((*d).device_type_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_from_raw_maps_known_values() {
        let cases = [
            (1, NativeErrorCode::InvalidArg),
            (2, NativeErrorCode::FrameworkLoad),
            (3, NativeErrorCode::Unsupported),
            (4, NativeErrorCode::DeviceSet),
            (5, NativeErrorCode::Enumeration),
            (6, NativeErrorCode::Operation),
            (7, NativeErrorCode::NotFound),
            (8, NativeErrorCode::Exception),
            (9, NativeErrorCode::Alloc),
        ];
        for (raw, expected) in cases {
            assert_eq!(NativeErrorCode::from_raw(raw), expected, "raw {raw}");
        }
    }

    #[test]
    fn error_code_from_raw_keeps_unknown_codes_explicit() {
        for raw in [0, 10, 42, -1] {
            assert_eq!(
                NativeErrorCode::from_raw(raw),
                NativeErrorCode::Unknown,
                "raw {raw} must not masquerade as an operational failure"
            );
        }
    }

    #[test]
    fn device_state_from_raw_maps_known_values() {
        let cases = [
            (1, NativeDeviceState::Shutdown),
            (2, NativeDeviceState::Booted),
            (3, NativeDeviceState::Booting),
            (4, NativeDeviceState::ShuttingDown),
            (5, NativeDeviceState::Creating),
        ];
        for (raw, expected) in cases {
            assert_eq!(NativeDeviceState::from_raw(raw), expected, "raw {raw}");
        }
    }

    #[test]
    fn device_state_from_raw_maps_unrecognized_values_to_unknown() {
        for raw in [0, 6, 99, -1] {
            assert_eq!(
                NativeDeviceState::from_raw(raw),
                NativeDeviceState::Unknown,
                "raw {raw}"
            );
        }
    }

    #[test]
    fn unsupported_error_uses_typed_code() {
        let err = NativeError::unsupported("boot");
        assert_eq!(err.code, NativeErrorCode::Unsupported);
        assert!(err.message.contains("boot"));
        assert!(err.detail.is_none());
    }

    #[test]
    fn capabilities_any_none_follow_all_bits() {
        let empty = NativeCapabilities::default();
        assert!(empty.none());
        assert!(!empty.any());
        let boot_only = NativeCapabilities {
            boot: true,
            ..NativeCapabilities::default()
        };
        assert!(boot_only.any());
        assert!(!boot_only.none());
    }
}
