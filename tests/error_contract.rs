//! Contract tests for the stable error taxonomy.
//!
//! Every failure category has a stable machine code (SCREAMING_SNAKE_CASE,
//! identical to its serde name), a deterministic process exit code, and the
//! `--json` error envelope ([`simtop::error::ErrorReport`]) with a fixed
//! shape. These tests pin all three so automation can rely on them.

use serde_json::{json, Value};
use simtop::error::{exit_code, ErrorCode, ErrorReport, SimtopError};
use std::error::Error as _;

/// The full, authoritative mapping table: variant → machine code → exit code.
const MAPPING: &[(ErrorCode, &str, i32)] = &[
    (
        ErrorCode::InvalidArgument,
        "INVALID_ARGUMENT",
        exit_code::INVALID_ARGUMENT,
    ),
    (
        ErrorCode::PlatformUnsupported,
        "PLATFORM_UNSUPPORTED",
        exit_code::PLATFORM_UNSUPPORTED,
    ),
    (
        ErrorCode::XcodeNotFound,
        "XCODE_NOT_FOUND",
        exit_code::XCODE_NOT_FOUND,
    ),
    (
        ErrorCode::InvalidDeveloperDir,
        "INVALID_DEVELOPER_DIR",
        exit_code::INVALID_DEVELOPER_DIR,
    ),
    (
        ErrorCode::NativeBridgeUnavailable,
        "NATIVE_BRIDGE_UNAVAILABLE",
        exit_code::NATIVE_BRIDGE_UNAVAILABLE,
    ),
    (
        ErrorCode::UnsupportedOperation,
        "UNSUPPORTED_OPERATION",
        exit_code::UNSUPPORTED_OPERATION,
    ),
    (
        ErrorCode::DeviceNotFound,
        "DEVICE_NOT_FOUND",
        exit_code::DEVICE_NOT_FOUND,
    ),
    (
        ErrorCode::CommandFailed,
        "COMMAND_FAILED",
        exit_code::COMMAND_FAILED,
    ),
    (ErrorCode::Timeout, "TIMEOUT", exit_code::TIMEOUT),
    (ErrorCode::IoError, "IO_ERROR", exit_code::IO_ERROR),
    (ErrorCode::ParseError, "PARSE_ERROR", exit_code::PARSE_ERROR),
    (ErrorCode::Internal, "INTERNAL", exit_code::INTERNAL),
];

#[test]
fn machine_codes_are_stable() {
    for (code, expected, _) in MAPPING {
        assert_eq!(code.code(), *expected, "machine code for {code:?}");
    }
}

#[test]
fn serde_names_match_machine_codes_and_round_trip() {
    for (code, expected, _) in MAPPING {
        let serialized = serde_json::to_string(code).unwrap();
        assert_eq!(
            serialized,
            format!("\"{expected}\""),
            "serde name for {code:?}"
        );
        let back: ErrorCode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(back, *code);
    }
}

#[test]
fn exit_codes_are_deterministic_and_match_mapping() {
    for (code, _, expected) in MAPPING {
        assert_eq!(code.exit_code(), *expected, "exit code for {code:?}");
    }
}

#[test]
fn exit_codes_are_unique_and_avoid_reserved_values() {
    let mut seen = std::collections::HashSet::new();
    for (code, _, exit) in MAPPING {
        assert!(
            *exit >= 2,
            "{code:?} exit code {exit} collides with the reserved success (0) or shell (1) values"
        );
        assert!(
            seen.insert(*exit),
            "exit code {exit} is allocated to multiple categories"
        );
    }
    // The reserved values themselves stay reserved.
    assert_eq!(exit_code::OK, 0);
}

#[test]
fn exit_code_constants_are_stable() {
    assert_eq!(exit_code::INVALID_ARGUMENT, 2);
    assert_eq!(exit_code::PLATFORM_UNSUPPORTED, 3);
    assert_eq!(exit_code::XCODE_NOT_FOUND, 4);
    assert_eq!(exit_code::INVALID_DEVELOPER_DIR, 5);
    assert_eq!(exit_code::NATIVE_BRIDGE_UNAVAILABLE, 6);
    assert_eq!(exit_code::UNSUPPORTED_OPERATION, 7);
    assert_eq!(exit_code::DEVICE_NOT_FOUND, 8);
    assert_eq!(exit_code::COMMAND_FAILED, 9);
    assert_eq!(exit_code::TIMEOUT, 10);
    assert_eq!(exit_code::IO_ERROR, 11);
    assert_eq!(exit_code::PARSE_ERROR, 12);
    assert_eq!(exit_code::INTERNAL, 70);
}

#[test]
fn error_report_serializes_to_fixed_json_shape() {
    let err = SimtopError::new(ErrorCode::DeviceNotFound, "no such device");
    let value = serde_json::to_value(err.report()).unwrap();
    assert_eq!(
        value,
        json!({
            "code": "DEVICE_NOT_FOUND",
            "message": "device not found: no such device",
            "exit_code": 8,
        })
    );
    // The envelope is a flat object with exactly these three fields.
    let obj = value
        .as_object()
        .expect("report must serialize to an object");
    for key in ["code", "message", "exit_code"] {
        assert!(
            obj.contains_key(key),
            "report JSON is missing field `{key}`"
        );
    }
    assert_eq!(
        obj.len(),
        3,
        "report JSON must contain exactly the envelope fields"
    );
}

#[test]
fn error_report_fields_match_error_accessors() {
    let err = SimtopError::new(ErrorCode::Timeout, "waited too long");
    assert_eq!(err.code(), ErrorCode::Timeout);
    assert_eq!(err.machine_code(), "TIMEOUT");
    assert_eq!(err.exit_code(), 10);

    let report: ErrorReport = err.report();
    assert_eq!(report.code, err.machine_code());
    assert_eq!(report.message, err.to_string());
    assert_eq!(report.exit_code, err.exit_code());
    assert_eq!(report.message, "timeout: waited too long");
}

#[test]
fn error_report_includes_source_in_message() {
    let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
    let err = SimtopError::with_source(ErrorCode::IoError, "failed to open state file", io);
    let value = serde_json::to_value(err.report()).unwrap();
    assert_eq!(value["code"], "IO_ERROR");
    assert_eq!(value["exit_code"], 11);
    assert!(value["message"]
        .as_str()
        .unwrap()
        .contains("failed to open state file"));
    assert!(value["message"].as_str().unwrap().contains("no such file"));
    assert!(err.source().is_some());
}

#[test]
fn standard_conversions_map_to_stable_categories() {
    let io: SimtopError =
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into();
    assert_eq!(io.code(), ErrorCode::IoError);
    assert_eq!(io.exit_code(), 11);

    let parse: SimtopError = serde_json::from_str::<Value>("{not json")
        .unwrap_err()
        .into();
    assert_eq!(parse.code(), ErrorCode::ParseError);
    assert_eq!(parse.exit_code(), 12);
    assert_eq!(parse.machine_code(), "PARSE_ERROR");
}

#[test]
fn internal_error_uses_high_exit_code() {
    let err = SimtopError::new(ErrorCode::Internal, "invariant broken");
    assert_eq!(err.exit_code(), 70);
    assert_eq!(err.machine_code(), "INTERNAL");
    let value = serde_json::to_value(err.report()).unwrap();
    assert_eq!(value["exit_code"], 70);
}
