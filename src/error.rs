//! Error taxonomy with stable machine codes and deterministic exit codes.
//!
//! Every failure is a [`SimtopError`] carrying an [`ErrorCode`] category.
//! Each category maps to a stable machine code ([`ErrorCode::code`]) and a
//! deterministic process exit code ([`ErrorCode::exit_code`]), so scripts and
//! CI can branch on failures without parsing messages.

use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::fmt;

/// Convenience alias used across the crate: `Result<T, SimtopError>`.
pub type Result<T> = std::result::Result<T, SimtopError>;

/// Deterministic process exit codes, one per [`ErrorCode`] category.
///
/// `0` is success; `1` is reserved for the shell; `2+` are allocated per
/// category so automation can distinguish failure classes.
pub mod exit_code {
    pub const OK: i32 = 0;
    /// Invalid CLI arguments or configuration.
    pub const INVALID_ARGUMENT: i32 = 2;
    /// Host platform cannot drive CoreSimulator.
    pub const PLATFORM_UNSUPPORTED: i32 = 3;
    /// No Xcode installation found.
    pub const XCODE_NOT_FOUND: i32 = 4;
    /// `--developer-dir` / `DEVELOPER_DIR` points at an invalid directory.
    pub const INVALID_DEVELOPER_DIR: i32 = 5;
    /// The CoreSimulator framework could not be loaded.
    pub const NATIVE_BRIDGE_UNAVAILABLE: i32 = 6;
    /// The operation is not supported by the available backend.
    pub const UNSUPPORTED_OPERATION: i32 = 7;
    /// The requested device does not exist.
    pub const DEVICE_NOT_FOUND: i32 = 8;
    /// An underlying command or operation failed.
    pub const COMMAND_FAILED: i32 = 9;
    /// An operation timed out.
    pub const TIMEOUT: i32 = 10;
    /// Filesystem I/O failure.
    pub const IO_ERROR: i32 = 11;
    /// Failed to parse input or output data.
    pub const PARSE_ERROR: i32 = 12;
    /// Unexpected internal failure.
    pub const INTERNAL: i32 = 70;
}

/// Stable error categories.
///
/// The serde name of each variant (SCREAMING_SNAKE_CASE) is the machine code,
/// identical to [`ErrorCode::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    PlatformUnsupported,
    XcodeNotFound,
    InvalidDeveloperDir,
    NativeBridgeUnavailable,
    UnsupportedOperation,
    DeviceNotFound,
    CommandFailed,
    Timeout,
    IoError,
    ParseError,
    Internal,
}

impl ErrorCode {
    /// Stable machine code — safe to persist, match on, and emit in `--json`.
    pub fn code(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::PlatformUnsupported => "PLATFORM_UNSUPPORTED",
            ErrorCode::XcodeNotFound => "XCODE_NOT_FOUND",
            ErrorCode::InvalidDeveloperDir => "INVALID_DEVELOPER_DIR",
            ErrorCode::NativeBridgeUnavailable => "NATIVE_BRIDGE_UNAVAILABLE",
            ErrorCode::UnsupportedOperation => "UNSUPPORTED_OPERATION",
            ErrorCode::DeviceNotFound => "DEVICE_NOT_FOUND",
            ErrorCode::CommandFailed => "COMMAND_FAILED",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::IoError => "IO_ERROR",
            ErrorCode::ParseError => "PARSE_ERROR",
            ErrorCode::Internal => "INTERNAL",
        }
    }

    /// Deterministic process exit code for this category.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::InvalidArgument => exit_code::INVALID_ARGUMENT,
            ErrorCode::PlatformUnsupported => exit_code::PLATFORM_UNSUPPORTED,
            ErrorCode::XcodeNotFound => exit_code::XCODE_NOT_FOUND,
            ErrorCode::InvalidDeveloperDir => exit_code::INVALID_DEVELOPER_DIR,
            ErrorCode::NativeBridgeUnavailable => exit_code::NATIVE_BRIDGE_UNAVAILABLE,
            ErrorCode::UnsupportedOperation => exit_code::UNSUPPORTED_OPERATION,
            ErrorCode::DeviceNotFound => exit_code::DEVICE_NOT_FOUND,
            ErrorCode::CommandFailed => exit_code::COMMAND_FAILED,
            ErrorCode::Timeout => exit_code::TIMEOUT,
            ErrorCode::IoError => exit_code::IO_ERROR,
            ErrorCode::ParseError => exit_code::PARSE_ERROR,
            ErrorCode::Internal => exit_code::INTERNAL,
        }
    }

    /// Human-readable category label for terminal messages.
    pub fn label(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "invalid argument",
            ErrorCode::PlatformUnsupported => "unsupported platform",
            ErrorCode::XcodeNotFound => "xcode not found",
            ErrorCode::InvalidDeveloperDir => "invalid developer directory",
            ErrorCode::NativeBridgeUnavailable => "native bridge unavailable",
            ErrorCode::UnsupportedOperation => "unsupported operation",
            ErrorCode::DeviceNotFound => "device not found",
            ErrorCode::CommandFailed => "command failed",
            ErrorCode::Timeout => "timeout",
            ErrorCode::IoError => "i/o error",
            ErrorCode::ParseError => "parse error",
            ErrorCode::Internal => "internal error",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The crate-wide error type.
///
/// Construct with [`SimtopError::new`] or [`SimtopError::with_source`]; the
/// code drives machine-readable output and the process exit status.
#[derive(Debug)]
pub struct SimtopError {
    code: ErrorCode,
    message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl SimtopError {
    /// Build an error with a category and a human-readable detail message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        SimtopError {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// Build an error with an underlying cause (e.g. a bridge or I/O error).
    pub fn with_source(
        code: ErrorCode,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        SimtopError {
            code,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// The error category.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Stable machine code string (SCREAMING_SNAKE_CASE).
    pub fn machine_code(&self) -> &'static str {
        self.code.code()
    }

    /// Deterministic process exit code.
    pub fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }

    /// The human-readable detail message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Machine-readable view for `--json` output.
    pub fn report(&self) -> ErrorReport {
        ErrorReport {
            code: self.code.code(),
            message: self.to_string(),
            exit_code: self.exit_code(),
        }
    }
}

impl fmt::Display for SimtopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.label(), self.message)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl StdError for SimtopError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        // The boxed error is `dyn StdError + Send + Sync`; drop the auto
        // traits with an explicit unsizing cast to the standard trait-object
        // type (coercions do not propagate through `Option`).
        self.source
            .as_deref()
            .map(|e| e as &(dyn StdError + 'static))
    }
}

impl From<std::io::Error> for SimtopError {
    fn from(err: std::io::Error) -> Self {
        SimtopError::new(ErrorCode::IoError, err.to_string())
    }
}

impl From<serde_json::Error> for SimtopError {
    fn from(err: serde_json::Error) -> Self {
        SimtopError::new(ErrorCode::ParseError, err.to_string())
    }
}

/// Machine-readable error envelope for `--json` output.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorReport {
    /// Stable machine code (SCREAMING_SNAKE_CASE).
    pub code: &'static str,
    /// Human-readable message, including the underlying cause when present.
    pub message: String,
    /// Deterministic process exit code.
    pub exit_code: i32,
}
