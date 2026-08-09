//! simtop — high-performance iOS Simulator management (TUI + automation CLI).
//!
//! Module map:
//! - `backend`: the async `SimulatorBackend` trait plus the hybrid
//!   CoreSimulator-native / `simctl` implementation
//! - `cli`: argument parsing and command dispatch (owns the `run` entry point)
//! - `error`: error taxonomy with stable machine codes and deterministic exit codes
//! - `model`: serializable domain models, schema version 1
//! - `native`: Objective-C CoreSimulator bridge (macOS only)
//! - `output`: human and `--json` output formatting
//! - `tui`: interactive terminal UI
//! - `xcode`: Xcode developer-directory resolution

pub mod backend;
pub mod cli;
pub mod error;
pub mod model;
pub mod native;
pub mod output;
pub mod tui;
pub mod xcode;

pub use error::{ErrorCode, Result, SimtopError};

/// Verify the host platform can drive CoreSimulator.
///
/// Returns `Ok(())` on macOS and a [`ErrorCode::PlatformUnsupported`] error
/// everywhere else, so the binary reports unsupported platforms instead of
/// pretending to succeed. Call this early in `cli::run` (after argument
/// parsing) so `--help`/`--version` still work on any platform.
pub fn require_macos() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SimtopError::new(
            ErrorCode::PlatformUnsupported,
            "simtop requires macOS 15+ with Xcode 16 (CoreSimulator); this platform is unsupported",
        ))
    }
}
