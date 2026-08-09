// simtop — CoreSimulator C ABI bridge.
//
// A small, self-contained C ABI over Apple's private CoreSimulator.framework,
// so the simtop Rust crate can drive iOS Simulators without linking the
// private framework and without exposing any Objective-C object across the
// FFI boundary.
//
// Loading
// -------
// The framework is NOT linked at build time (no static private-framework
// linkage). It is discovered at runtime from the caller-supplied Xcode
// developer directory and loaded with dlopen(3):
//
//   1. <developer_dir>/Library/PrivateFrameworks/CoreSimulator.framework/CoreSimulator
//   2. /Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator
//      (system-wide location used by newer Xcode releases)
//
// If neither exists, simtop_create() returns NULL with
// SIMTOP_ERR_FRAMEWORK_LOAD and dlerror() text in the error record.
//
// Capability degradation
// ----------------------
// CoreSimulator is private API that changes between Xcode releases. Every
// class and selector is probed before use; anything missing degrades the
// capability set instead of crashing. Call simtop_capabilities() and gate
// operations on it: an operation whose capability bit is clear returns
// SIMTOP_ERR_UNSUPPORTED. Unknown or future Xcode versions therefore degrade
// gracefully (the caller falls back to `simctl`) rather than crash.
//
// Exceptions
// ----------
// Objective-C exceptions cannot cross this ABI. Every entry point wraps its
// work in an Objective-C exception handler; a caught exception is reported
// as SIMTOP_ERR_EXCEPTION with the exception name/reason in the error
// record. Callers never observe a non-local jump.
//
// Ownership model
// ---------------
//  1. All strings are UTF-8, NUL-terminated char*. A NULL string field means
//     "not available" (missing API or missing value), never an error.
//  2. Every pointer returned by a simtop_* function is owned by the caller
//     and must be released exactly once with its matching free function:
//         simtop_handle           -> simtop_destroy()
//         simtop_error            -> simtop_error_free()
//         simtop_copy_device_set_info -> simtop_device_set_info_free()
//         simtop_device_list      -> simtop_device_list_free()
//         simtop_device           -> simtop_device_free()
//  3. All char* fields inside a record are owned by that record and are freed
//     by the record's free function. Callers must not free or mutate them.
//  4. simtop_create_options is borrowed: its strings must remain valid only
//     for the duration of the simtop_create_device() call.
//  5. out_error parameters: *out_error is set to NULL on entry; on failure it
//     receives an owned simtop_error the caller must free with
//     simtop_error_free(). Pass NULL if the error detail is not needed.
//  6. simtop_device_list owns a contiguous array of simtop_device records
//     (list->devices[0..list->count)); simtop_device_list_free() releases the
//     array, every record, and every string inside them.
//
// Thread safety
// -------------
// A simtop_handle is not internally synchronized: callers must serialize
// access to a single handle (the Rust wrapper does this with a mutex).
// Distinct handles are fully independent. The handle stays valid until
// simtop_destroy(), which must be called exactly once.
//
// Semantics
// ---------
//  - Device enumeration returns every device in the default device set,
//    including unavailable ones (see `available`).
//  - simtop_boot_device()/simtop_shutdown_device() initiate the transition
//    and return once CoreSimulator accepted it; the state change completes
//    asynchronously and is visible in the next snapshot.
//  - simtop_create_device() requires a device type identifier and an
//    optional runtime identifier; NULL runtime lets CoreSimulator pick the
//    default runtime for the device type.
//
// ABI versioning
// --------------
// SIMTOP_ABI_VERSION must match the Rust binding; call simtop_abi_version()
// at load time and refuse to proceed on mismatch.

#ifndef SIMTOP_CORE_SIMULATOR_H
#define SIMTOP_CORE_SIMULATOR_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SIMTOP_ABI_VERSION 1u

/* Opaque session: the loaded framework, probed classes/selectors, the
 * resolved device set, and the capability bitmap. Created by
 * simtop_create(), destroyed by simtop_destroy(). */
typedef struct simtop_handle simtop_handle;

/* Stable machine codes shared with the Rust binding. Values are fixed; new
 * codes may be appended, existing ones never renumbered. */
typedef enum simtop_error_code {
    SIMTOP_ERR_NONE = 0,        /* success; never stored in an error record */
    SIMTOP_ERR_INVALID_ARG = 1, /* NULL/empty argument (developer dir, UDID, ...) */
    SIMTOP_ERR_FRAMEWORK_LOAD = 2, /* CoreSimulator.framework not loadable */
    SIMTOP_ERR_UNSUPPORTED = 3, /* required class/selector absent (capability off) */
    SIMTOP_ERR_DEVICE_SET = 4,  /* device set discovery/resolution failed */
    SIMTOP_ERR_ENUMERATION = 5, /* device enumeration failed */
    SIMTOP_ERR_OPERATION = 6,   /* CoreSimulator rejected an operation (NSError) */
    SIMTOP_ERR_NOT_FOUND = 7,   /* device/device type/runtime not found */
    SIMTOP_ERR_EXCEPTION = 8,   /* Objective-C exception caught at the boundary */
    SIMTOP_ERR_ALLOC = 9,       /* out of memory building a record */
} simtop_error_code;

/* Device state. SIMTOP_STATE_UNKNOWN covers anything unrecognized. Mapping
 * uses -[SimDevice stateString] when available (version-agnostic), falling
 * back to the raw legacy enum values on older Xcode. */
typedef enum simtop_device_state {
    SIMTOP_STATE_UNKNOWN = 0,
    SIMTOP_STATE_SHUTDOWN = 1,
    SIMTOP_STATE_BOOTED = 2,
    SIMTOP_STATE_BOOTING = 3,
    SIMTOP_STATE_SHUTTING_DOWN = 4,
    SIMTOP_STATE_CREATING = 5,
} simtop_device_state;

/* Capability bits returned by simtop_capabilities(); each operation checks
 * its bit and returns SIMTOP_ERR_UNSUPPORTED when clear. */
typedef enum simtop_capability {
    SIMTOP_CAP_DEVICE_SET = 1u << 0,  /* device set resolved; set info available */
    SIMTOP_CAP_ENUMERATE  = 1u << 1,  /* list_devices / device_for_udid */
    SIMTOP_CAP_BOOT       = 1u << 2,  /* boot_device */
    SIMTOP_CAP_SHUTDOWN   = 1u << 3,  /* shutdown_device */
    SIMTOP_CAP_CREATE     = 1u << 4,  /* create_device */
    SIMTOP_CAP_DELETE     = 1u << 5,  /* delete_device */
} simtop_capability;

/* Owned error record. `message` is always non-NULL (possibly empty);
 * `detail` may be NULL. Free with simtop_error_free(). */
typedef struct simtop_error {
    simtop_error_code code;
    char *message;
    char *detail;
} simtop_error;

/* Owned snapshot of the default device set. Free with
 * simtop_device_set_info_free(). */
typedef struct simtop_device_set_info {
    char *path; /* set directory, e.g. ~/Library/Developer/CoreSimulator/Devices */
    char *name; /* may be NULL (absent on newer Xcode) */
} simtop_device_set_info;

/* Owned device record. All char* fields are owned and may be NULL when the
 * metadata is unavailable. Free with simtop_device_free(). */
typedef struct simtop_device {
    char *udid;                   /* e.g. "95E9ED35-3168-4318-9AC0-A9DD7C533A6F" */
    char *name;                   /* e.g. "iPhone 16 Pro" */
    simtop_device_state state;
    bool available;               /* false for devices without an installed runtime */
    char *runtime_identifier;     /* e.g. "com.apple.CoreSimulator.SimRuntime.iOS-18-0" */
    char *runtime_name;           /* e.g. "iOS 18.0" */
    char *runtime_version;        /* e.g. "18.0" */
    char *runtime_build;          /* e.g. "22A3351" */
    char *device_type_identifier; /* e.g. "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro" */
    char *device_type_name;       /* e.g. "iPhone 16 Pro" */
} simtop_device;

/* Owned device list: a contiguous array of `count` device records. Free with
 * simtop_device_list_free(). */
typedef struct simtop_device_list {
    size_t count;
    simtop_device *devices;
} simtop_device_list;

/* Borrowed create parameters; valid only for the duration of the call.
 * `name` and `device_type` are required, `runtime` is optional (NULL = let
 * CoreSimulator choose the default runtime for the device type). */
typedef struct simtop_create_options {
    const char *name;
    const char *device_type;
    const char *runtime;
} simtop_create_options;

/* ABI version; the Rust binding refuses to proceed on mismatch. */
uint32_t simtop_abi_version(void);

/* Load the framework and resolve the default device set. Returns NULL with
 * *out_error set for SIMTOP_ERR_INVALID_ARG, SIMTOP_ERR_ALLOC, or
 * SIMTOP_ERR_FRAMEWORK_LOAD. Otherwise returns a handle whose capability
 * bitmap reflects what the loaded Xcode actually supports. */
simtop_handle *simtop_create(const char *developer_dir, simtop_error **out_error);
void simtop_destroy(simtop_handle *handle);

/* Capability bitmap (simtop_capability bits). Safe on NULL handle (returns 0). */
uint32_t simtop_capabilities(const simtop_handle *handle);

simtop_device_set_info *simtop_copy_device_set_info(const simtop_handle *handle,
                                               simtop_error **out_error);
void simtop_device_set_info_free(simtop_device_set_info *info);

/* Snapshot of all devices in the default set. Requires SIMTOP_CAP_ENUMERATE. */
simtop_device_list *simtop_list_devices(const simtop_handle *handle,
                                        simtop_error **out_error);
void simtop_device_list_free(simtop_device_list *list);

/* Copy of a single device looked up by UDID (case-insensitive).
 * Requires SIMTOP_CAP_ENUMERATE; SIMTOP_ERR_NOT_FOUND when absent. */
simtop_device *simtop_device_for_udid(const simtop_handle *handle,
                                      const char *udid,
                                      simtop_error **out_error);
void simtop_device_free(simtop_device *device);

/* Lifecycle. Returns SIMTOP_ERR_NONE on success; *out_error is set on
 * failure. Requires the matching capability bit and a resolved device set. */
simtop_error_code simtop_boot_device(simtop_handle *handle, const char *udid,
                                     simtop_error **out_error);
simtop_error_code simtop_shutdown_device(simtop_handle *handle, const char *udid,
                                         simtop_error **out_error);
simtop_error_code simtop_delete_device(simtop_handle *handle, const char *udid,
                                       simtop_error **out_error);

/* Create a device and return its record (includes the new UDID).
 * Requires SIMTOP_CAP_CREATE. */
simtop_device *simtop_create_device(simtop_handle *handle,
                                    const simtop_create_options *options,
                                    simtop_error **out_error);

void simtop_error_free(simtop_error *error);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* SIMTOP_CORE_SIMULATOR_H */
