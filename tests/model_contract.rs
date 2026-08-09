//! Contract tests for the serializable domain models (schema v1).
//!
//! These pin the *observable* wire format: the schema version, JSON field and
//! enum names, lossless round-trips (including unknown device states), and the
//! `DeviceSnapshot` schema invariant. They exercise only public library APIs
//! and are deterministic on any platform.

use serde_json::{json, Value};
use simtop::model::{
    App, DeviceCapabilities, DeviceSnapshot, DeviceState, LaunchInfo, LogEntry, Process, Runtime,
    SimDevice, SimulatorEvent, SCHEMA_VERSION,
};

fn device() -> SimDevice {
    SimDevice {
        udid: "AAAABBBB-CCCC-DDDD-EEEE-FFFF00001111".to_owned(),
        name: "iPhone 16 Pro".to_owned(),
        state: DeviceState::Booted,
        device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro".to_owned(),
        runtime: "com.apple.CoreSimulator.SimRuntime.iOS-18-0".to_owned(),
        os_version: "18.0".to_owned(),
        is_available: true,
    }
}

#[test]
fn schema_version_is_stable() {
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn device_state_serializes_to_snake_case_names() {
    assert_eq!(to_json_string(&DeviceState::Booted), "\"booted\"");
    assert_eq!(to_json_string(&DeviceState::Booting), "\"booting\"");
    assert_eq!(
        to_json_string(&DeviceState::ShuttingDown),
        "\"shutting_down\""
    );
    assert_eq!(to_json_string(&DeviceState::Shutdown), "\"shutdown\"");
    assert_eq!(to_json_string(&DeviceState::Creating), "\"creating\"");
}

#[test]
fn device_state_unknown_serializes_as_tagged_raw_string() {
    assert_eq!(
        serde_json::to_value(&DeviceState::Unknown("frobnicating".to_owned())).unwrap(),
        json!({ "unknown": "frobnicating" })
    );
}

#[test]
fn device_state_deserializes_from_snake_case_names() {
    assert_eq!(deserialize_state("\"booted\""), DeviceState::Booted);
    assert_eq!(deserialize_state("\"booting\""), DeviceState::Booting);
    assert_eq!(
        deserialize_state("\"shutting_down\""),
        DeviceState::ShuttingDown
    );
    assert_eq!(deserialize_state("\"shutdown\""), DeviceState::Shutdown);
    assert_eq!(deserialize_state("\"creating\""), DeviceState::Creating);
    // Unknown states survive deserialization with the raw string intact.
    assert_eq!(
        deserialize_state(&json!({ "unknown": "frobnicating" }).to_string()),
        DeviceState::Unknown("frobnicating".to_owned())
    );
}

#[test]
fn device_state_parse_is_case_insensitive_and_trims() {
    assert_eq!(DeviceState::from("Booted"), DeviceState::Booted);
    assert_eq!(DeviceState::from("  booted  "), DeviceState::Booted);
    assert_eq!(
        DeviceState::from("SHUTTING DOWN"),
        DeviceState::ShuttingDown
    );
    assert_eq!(DeviceState::from("shuttingdown"), DeviceState::ShuttingDown);
    assert_eq!(DeviceState::from("Creating"), DeviceState::Creating);
}

#[test]
fn unknown_state_parse_preserves_raw_payload() {
    let raw = "  Half-Open  ";
    match DeviceState::from(raw) {
        DeviceState::Unknown(kept) => assert_eq!(kept, "Half-Open"),
        other => panic!("expected Unknown, got {other:?}"),
    }
    assert_eq!(
        DeviceState::from(String::from("weird")),
        DeviceState::Unknown("weird".to_owned())
    );
}

#[test]
fn unknown_state_display_prints_raw_payload() {
    assert_eq!(
        DeviceState::Unknown("frobnicating".to_owned()).to_string(),
        "frobnicating"
    );
    assert_eq!(DeviceState::Booted.to_string(), "Booted");
    assert_eq!(DeviceState::ShuttingDown.to_string(), "Shutting Down");
}

#[test]
fn unknown_state_round_trips_through_sim_device_json_without_loss() {
    let dev = SimDevice {
        state: DeviceState::Unknown("frobnicating".to_owned()),
        ..device()
    };
    let value = serde_json::to_value(&dev).unwrap();
    assert_eq!(value["state"], json!({ "unknown": "frobnicating" }));
    let back: SimDevice = serde_json::from_value(value).unwrap();
    assert_eq!(back, dev);
    assert_eq!(back.state, DeviceState::Unknown("frobnicating".to_owned()));
}

#[test]
fn sim_device_field_names_are_stable() {
    let value = serde_json::to_value(&device()).unwrap();
    assert_eq!(
        value,
        json!({
            "udid": "AAAABBBB-CCCC-DDDD-EEEE-FFFF00001111",
            "name": "iPhone 16 Pro",
            "state": "booted",
            "device_type": "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro",
            "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-18-0",
            "os_version": "18.0",
            "is_available": true,
        })
    );
}

#[test]
fn device_snapshot_new_stamps_schema_version() {
    // The schema version is stamped by the constructor, not carried by the
    // caller — a caller-supplied version must never leak in.
    let snapshot = DeviceSnapshot::new(7, "2026-08-08T12:00:00Z", vec![device()]);
    assert_eq!(snapshot.schema_version, SCHEMA_VERSION);
    assert_eq!(snapshot.generation, 7);
    assert_eq!(snapshot.timestamp, "2026-08-08T12:00:00Z");
    assert_eq!(snapshot.devices.len(), 1);
}

#[test]
fn device_snapshot_json_shape_and_round_trip() {
    let snapshot = DeviceSnapshot::new(3, "2026-08-08T12:00:00Z", vec![device()]);
    let value = serde_json::to_value(&snapshot).unwrap();
    let obj = value
        .as_object()
        .expect("snapshot must serialize to an object");
    for key in ["schema_version", "generation", "timestamp", "devices"] {
        assert!(
            obj.contains_key(key),
            "snapshot JSON is missing field `{key}`"
        );
    }
    assert_eq!(
        obj.len(),
        4,
        "snapshot JSON must contain exactly the schema fields"
    );
    assert_eq!(value["schema_version"], SCHEMA_VERSION);
    assert_eq!(value["generation"], 3);
    assert_eq!(value["timestamp"], "2026-08-08T12:00:00Z");
    assert_eq!(value["devices"][0]["udid"], device().udid);

    let back: DeviceSnapshot = serde_json::from_value(value).unwrap();
    assert_eq!(back, snapshot);
}

#[test]
fn device_snapshot_deserializes_from_persisted_json() {
    // JSON written by an earlier release must keep deserializing.
    let value = json!({
        "schema_version": 1,
        "generation": 9,
        "timestamp": "2026-08-08T12:00:00Z",
        "devices": [{
            "udid": "X",
            "name": "N",
            "state": { "unknown": "provisioning" },
            "device_type": "T",
            "runtime": "R",
            "os_version": "18.0",
            "is_available": false
        }]
    });
    let snapshot: DeviceSnapshot = serde_json::from_value(value).unwrap();
    assert_eq!(snapshot.schema_version, 1);
    assert_eq!(
        snapshot.devices[0].state,
        DeviceState::Unknown("provisioning".to_owned())
    );
}

#[test]
fn simulator_event_uses_type_data_envelope_and_snake_case_tags() {
    let added = SimulatorEvent::DeviceAdded(device());
    let value = serde_json::to_value(&added).unwrap();
    assert_eq!(value["type"], "device_added");
    assert_eq!(value["data"]["udid"], device().udid);
    let back: SimulatorEvent = serde_json::from_value(value).unwrap();
    assert_eq!(back, SimulatorEvent::DeviceAdded(device()));

    let changed = SimulatorEvent::DeviceStateChanged {
        udid: "U".to_owned(),
        state: DeviceState::ShuttingDown,
    };
    let value = serde_json::to_value(&changed).unwrap();
    assert_eq!(value["type"], "device_state_changed");
    assert_eq!(value["data"]["state"], "shutting_down");
    let back: SimulatorEvent = serde_json::from_value(value).unwrap();
    assert_eq!(back, changed);
}

#[test]
fn model_structs_round_trip_with_optional_fields_defaulting() {
    // `App`, `Process`, `Runtime`, `DeviceCapabilities`, `LaunchInfo`,
    // `LogEntry` round-trip losslessly; absent optional fields deserialize to
    // `None`/empty rather than failing.
    let app = App {
        device_udid: "U".to_owned(),
        bundle_id: "com.example.app".to_owned(),
        name: "App".to_owned(),
        version: None,
        build: None,
        path: None,
        data_path: None,
    };
    let value = serde_json::to_value(&app).unwrap();
    assert_eq!(value["version"], Value::Null);
    let back: App = serde_json::from_value(value).unwrap();
    assert_eq!(back, app);

    let minimal = json!({
        "device_udid": "U",
        "bundle_id": "com.example.app",
        "name": "App"
    });
    let app: App = serde_json::from_value(minimal).unwrap();
    assert_eq!(app.version, None);
    assert_eq!(app.path, None);

    let caps: DeviceCapabilities = serde_json::from_value(json!({
        "arch": "arm64",
        "is_64_bit": true
    }))
    .unwrap();
    assert_eq!(caps.arch, "arm64");
    assert_eq!(caps.gpu, None);

    let runtime: Runtime = serde_json::from_value(json!({
        "identifier": "R",
        "name": "iOS 18.0",
        "version": "18.0",
        "build": "22A3351",
        "platform": "iOS",
        "is_available": true
    }))
    .unwrap();
    assert_eq!(runtime.supported_device_types, Vec::<String>::new());

    let launch = LaunchInfo {
        udid: "U".to_owned(),
        bundle_id: "B".to_owned(),
        pid: None,
    };
    let back: LaunchInfo = serde_json::from_value(serde_json::to_value(&launch).unwrap()).unwrap();
    assert_eq!(back, launch);

    let log = LogEntry {
        timestamp: "t".to_owned(),
        process: "p".to_owned(),
        pid: None,
        message: "m".to_owned(),
    };
    let back: LogEntry = serde_json::from_value(serde_json::to_value(&log).unwrap()).unwrap();
    assert_eq!(back, log);

    let proc = Process {
        device_udid: "U".to_owned(),
        pid: 42,
        name: "n".to_owned(),
        bundle_id: None,
        start_time: None,
    };
    let back: Process = serde_json::from_value(serde_json::to_value(&proc).unwrap()).unwrap();
    assert_eq!(back, proc);
}

fn to_json_string(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

fn deserialize_state(json: &str) -> DeviceState {
    serde_json::from_str(json).unwrap()
}
