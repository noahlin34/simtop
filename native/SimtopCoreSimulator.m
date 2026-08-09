// simtop — CoreSimulator C ABI bridge implementation.
//
// Compiled with -fno-objc-arc: all Objective-C object ownership is manual.
// Every exported function runs inside an @autoreleasepool and wraps all
// message sends in an Objective-C exception handler, so no exception can
// cross the C ABI. All classes/selectors are probed once at simtop_create();
// missing pieces simply clear capability bits.
//
// See SimtopCoreSimulator.h for the ownership contract.

#import <Foundation/Foundation.h>
#import <objc/message.h>
#import <objc/runtime.h>

#import <dlfcn.h>
#import <limits.h>
#import <string.h>
#import <sys/stat.h>

#import "SimtopCoreSimulator.h"

struct simtop_handle {
    void *dl; // dlopen handle of CoreSimulator.framework

    // Probed classes (nil when absent in this Xcode).
    Class clsServiceContext;
    Class clsDeviceSet;
    Class clsDevice;
    Class clsDeviceType;
    Class clsRuntime;

    // Probed selectors (NULL when absent in this Xcode).
    SEL selSharedContext;        // +[SimServiceContext sharedServiceContextForDeveloperDir:error:]
    SEL selDefaultDeviceSet;     // -[SimServiceContext defaultDeviceSetWithError:]
    SEL selDefaultSet;           // +[SimDeviceSet defaultSetWithError:] (legacy)
    SEL selDevices;              // -[SimDeviceSet devices]
    SEL selUDID;                 // -[SimDevice UDID]
    SEL selUdid;                 // -[SimDevice udid] (legacy)
    SEL selIdentifier;           // -identifier (SimDevice/SimDeviceType/SimRuntime)
    SEL selUUIDString;           // -[NSUUID UUIDString]
    SEL selName;                 // -name
    SEL selState;                // -[SimDevice state]
    SEL selStateString;          // -[SimDevice stateString]
    SEL selDeviceType;           // -[SimDevice deviceType]
    SEL selRuntime;              // -[SimDevice runtime]
    SEL selVersionString;        // -[SimRuntime versionString]
    SEL selBuildVersionString;   // -[SimRuntime buildVersionString]
    SEL selAvailable;            // -[SimDevice available]
    SEL selIsAvailable;          // -[SimDevice isAvailable] (legacy)
    SEL selSetPath;              // -[SimDeviceSet setPath]
    SEL selSupportedDeviceTypes; // -[SimServiceContext supportedDeviceTypes] / +[SimDeviceType supportedDeviceTypes]
    SEL selSupportedRuntimes;    // -[SimServiceContext supportedRuntimes] / +[SimRuntime supportedRuntimes]
    SEL selCreateDevice;         // -[SimDeviceSet createDeviceWithType:runtime:name:error:]
    SEL selDeleteDevice;         // -[SimDeviceSet deleteDevice:error:]
    SEL selBootWithOptions;      // -[SimDevice bootWithOptions:error:]
    SEL selBootDevice;           // -[SimDevice bootDevice:] (legacy)
    SEL selShutdownWithError;    // -[SimDevice shutdownWithError:]
    SEL selShutdownDevice;       // -[SimDevice shutdownDevice:] (legacy)

    // Resolved objects (retained; released in simtop_destroy).
    id context;    // SimServiceContext
    id deviceSet;  // SimDeviceSet

    NSString *developerDir; // retained copy of the developer directory
    BOOL setResolved;
    simtop_error *setError; // owned; cached device-set resolution failure

    uint32_t capabilities;
};

#pragma mark - Error helpers

static simtop_error *simtop_error_make(simtop_error_code code, const char *message,
                                       const char *detail) {
    simtop_error *e = calloc(1, sizeof(*e));
    if (!e) return NULL;
    e->code = code;
    e->message = message ? strdup(message) : NULL;
    if (!e->message) e->message = strdup("unknown error");
    e->detail = detail ? strdup(detail) : NULL;
    return e;
}

static simtop_error *simtop_error_copy(const simtop_error *src) {
    if (!src) return NULL;
    return simtop_error_make(src->code, src->message, src->detail);
}

static simtop_error *simtop_error_from_nserror(simtop_error_code code, NSError *error) {
    if (!error) return simtop_error_make(code, "CoreSimulator operation failed", NULL);
    NSString *msg = [error localizedDescription];
    NSString *detail = [NSString stringWithFormat:@"domain=%@ code=%ld",
                                                  [error domain], (long)[error code]];
    return simtop_error_make(code, msg ? [msg UTF8String] : "CoreSimulator operation failed",
                             detail ? [detail UTF8String] : NULL);
}

static simtop_error *simtop_error_from_exception(NSException *e) {
    NSString *name = [e name];
    NSString *reason = [e reason];
    NSString *msg = reason ? reason : name;
    NSString *detail = name ? [NSString stringWithFormat:@"exception=%@", name] : NULL;
    return simtop_error_make(SIMTOP_ERR_EXCEPTION,
                             msg ? [msg UTF8String] : "Objective-C exception",
                             detail ? [detail UTF8String] : NULL);
}

#pragma mark - Message-send helpers (guarded)

static BOOL simtop_responds(id obj, SEL sel) {
    return obj != nil && sel != NULL && [obj respondsToSelector:sel];
}

static BOOL simtop_class_responds(Class cls, SEL sel) {
    if (!cls || !sel) return NO;
    return class_respondsToSelector(objc_getMetaClass(class_getName(cls)), sel);
}

static id simtop_msg_id(id obj, SEL sel) {
    if (!simtop_responds(obj, sel)) return nil;
    return ((id(*)(id, SEL))objc_msgSend)(obj, sel);
}

static NSInteger simtop_msg_int(id obj, SEL sel) {
    if (!simtop_responds(obj, sel)) return 0;
    return ((NSInteger(*)(id, SEL))objc_msgSend)(obj, sel);
}

static BOOL simtop_msg_bool(id obj, SEL sel) {
    if (!simtop_responds(obj, sel)) return NO;
    return ((BOOL(*)(id, SEL))objc_msgSend)(obj, sel);
}

static char *simtop_strdup_objc(id obj) {
    if (![obj isKindOfClass:[NSString class]]) return NULL;
    const char *utf8 = [obj UTF8String];
    return utf8 ? strdup(utf8) : NULL;
}

#pragma mark - Probing

static void simtop_probe_classes(simtop_handle *h) {
    h->clsServiceContext = objc_getClass("SimServiceContext");
    h->clsDeviceSet = objc_getClass("SimDeviceSet");
    h->clsDevice = objc_getClass("SimDevice");
    h->clsDeviceType = objc_getClass("SimDeviceType");
    h->clsRuntime = objc_getClass("SimRuntime");
}

static void simtop_probe_selectors(simtop_handle *h) {
    h->selSharedContext = sel_registerName("sharedServiceContextForDeveloperDir:error:");
    h->selDefaultDeviceSet = sel_registerName("defaultDeviceSetWithError:");
    h->selDefaultSet = sel_registerName("defaultSetWithError:");
    h->selDevices = sel_registerName("devices");
    h->selUDID = sel_registerName("UDID");
    h->selUdid = sel_registerName("udid");
    h->selIdentifier = sel_registerName("identifier");
    h->selUUIDString = sel_registerName("UUIDString");
    h->selName = sel_registerName("name");
    h->selState = sel_registerName("state");
    h->selStateString = sel_registerName("stateString");
    h->selDeviceType = sel_registerName("deviceType");
    h->selRuntime = sel_registerName("runtime");
    h->selVersionString = sel_registerName("versionString");
    h->selBuildVersionString = sel_registerName("buildVersionString");
    h->selAvailable = sel_registerName("available");
    h->selIsAvailable = sel_registerName("isAvailable");
    h->selSetPath = sel_registerName("setPath");
    h->selSupportedDeviceTypes = sel_registerName("supportedDeviceTypes");
    h->selSupportedRuntimes = sel_registerName("supportedRuntimes");
    h->selCreateDevice = sel_registerName("createDeviceWithType:runtime:name:error:");
    h->selDeleteDevice = sel_registerName("deleteDevice:error:");
    h->selBootWithOptions = sel_registerName("bootWithOptions:error:");
    h->selBootDevice = sel_registerName("bootDevice:");
    h->selShutdownWithError = sel_registerName("shutdownWithError:");
    h->selShutdownDevice = sel_registerName("shutdownDevice:");
}

// Does this Xcode expose a device-type registry?
static BOOL simtop_has_device_type_registry(const simtop_handle *h) {
    if (h->context && simtop_responds(h->context, h->selSupportedDeviceTypes)) return YES;
    return simtop_class_responds(h->clsDeviceType, h->selSupportedDeviceTypes);
}

// Does this Xcode expose a runtime registry?
static BOOL simtop_has_runtime_registry(const simtop_handle *h) {
    if (h->context && simtop_responds(h->context, h->selSupportedRuntimes)) return YES;
    return simtop_class_responds(h->clsRuntime, h->selSupportedRuntimes);
}

#pragma mark - Device set resolution

// Resolves the default device set once; caches the result (success or error).
static void simtop_resolve_device_set(simtop_handle *h) {
    if (h->setResolved) return;
    h->setResolved = YES;
    @autoreleasepool {
        NSError *error = nil;
        @try {
            if (h->clsServiceContext && h->selSharedContext &&
                simtop_class_responds(h->clsServiceContext, h->selSharedContext)) {
                id context = ((id(*)(id, SEL, id, NSError **))objc_msgSend)(
                    (id)h->clsServiceContext, h->selSharedContext, h->developerDir, &error);
                if (context) {
                    h->context = [context retain];
                    if (h->selDefaultDeviceSet && simtop_responds(context, h->selDefaultDeviceSet)) {
                        id set = ((id(*)(id, SEL, NSError **))objc_msgSend)(
                            context, h->selDefaultDeviceSet, &error);
                        if (set) {
                            h->deviceSet = [set retain];
                            return;
                        }
                    }
                }
            }
            // Legacy fallback: +[SimDeviceSet defaultSetWithError:].
            if (h->clsDeviceSet && h->selDefaultSet &&
                simtop_class_responds(h->clsDeviceSet, h->selDefaultSet)) {
                id set = ((id(*)(id, SEL, NSError **))objc_msgSend)(
                    (id)h->clsDeviceSet, h->selDefaultSet, &error);
                if (set) {
                    h->deviceSet = [set retain];
                    return;
                }
            }
        } @catch (NSException *e) {
            h->setError = simtop_error_from_exception(e);
            return;
        }
        if (error) {
            h->setError = simtop_error_from_nserror(SIMTOP_ERR_DEVICE_SET, error);
        } else {
            h->setError = simtop_error_make(SIMTOP_ERR_DEVICE_SET,
                                            "CoreSimulator device set discovery failed",
                                            "no known device-set API is available in this Xcode");
        }
    }
}

// Copies the cached set error (or a generic one) into *out_error.
static BOOL simtop_require_device_set(const simtop_handle *h, simtop_error **out_error) {
    if (h->deviceSet) return YES;
    if (out_error) {
        *out_error = h->setError ? simtop_error_copy(h->setError)
                                 : simtop_error_make(SIMTOP_ERR_DEVICE_SET,
                                                     "CoreSimulator device set is unavailable",
                                                     NULL);
    }
    return NO;
}

#pragma mark - Capabilities

static uint32_t simtop_compute_capabilities(const simtop_handle *h) {
    uint32_t caps = 0;
    if (h->deviceSet) caps |= SIMTOP_CAP_DEVICE_SET;
    if (h->deviceSet && h->selDevices && h->clsDevice &&
        (h->selUDID || h->selUdid || h->selIdentifier) &&
        h->selName && h->selState) {
        caps |= SIMTOP_CAP_ENUMERATE;
    }
    if (h->clsDevice && (h->selBootWithOptions || h->selBootDevice)) caps |= SIMTOP_CAP_BOOT;
    if (h->clsDevice && (h->selShutdownWithError || h->selShutdownDevice)) caps |= SIMTOP_CAP_SHUTDOWN;
    if (h->selCreateDevice && simtop_has_device_type_registry(h) && simtop_has_runtime_registry(h)) {
        caps |= SIMTOP_CAP_CREATE;
    }
    if (h->selDeleteDevice) caps |= SIMTOP_CAP_DELETE;
    return caps;
}

#pragma mark - Device introspection

// Version-agnostic state mapping: prefer -[SimDevice stateString], fall back
// to the raw legacy enum values (0=Shutdown .. 3=ShuttingDown) that older
// Xcode shipped; anything unrecognized maps to UNKNOWN.
static simtop_device_state simtop_map_state(id dev, const simtop_handle *h) {
    if (h->selStateString && simtop_responds(dev, h->selStateString)) {
        id s = ((id(*)(id, SEL))objc_msgSend)(dev, h->selStateString);
        if ([s isKindOfClass:[NSString class]]) {
            NSString *str = s;
            if ([str isEqualToString:@"Shutdown"]) return SIMTOP_STATE_SHUTDOWN;
            if ([str isEqualToString:@"Booted"]) return SIMTOP_STATE_BOOTED;
            if ([str isEqualToString:@"Booting"]) return SIMTOP_STATE_BOOTING;
            if ([str isEqualToString:@"Shutting Down"]) return SIMTOP_STATE_SHUTTING_DOWN;
            if ([str isEqualToString:@"Creating"]) return SIMTOP_STATE_CREATING;
            return SIMTOP_STATE_UNKNOWN;
        }
    }
    NSInteger raw = simtop_msg_int(dev, h->selState);
    switch (raw) {
        case 0: return SIMTOP_STATE_SHUTDOWN;
        case 1: return SIMTOP_STATE_BOOTED;
        case 2: return SIMTOP_STATE_BOOTING;
        case 3: return SIMTOP_STATE_SHUTTING_DOWN;
        default: return SIMTOP_STATE_UNKNOWN;
    }
}

static id simtop_device_udid_string(id dev, const simtop_handle *h) {
    id udid = simtop_msg_id(dev, h->selUDID);
    if (!udid) udid = simtop_msg_id(dev, h->selUdid);
    if (!udid) udid = simtop_msg_id(dev, h->selIdentifier);
    if ([udid isKindOfClass:[NSUUID class]] && h->selUUIDString) {
        udid = ((id(*)(id, SEL))objc_msgSend)(udid, h->selUUIDString);
    }
    return [udid isKindOfClass:[NSString class]] ? udid : nil;
}

// Fills a pre-zeroed simtop_device from a SimDevice. Returns NO only if an
// Objective-C exception escaped the property reads (caller aborts then).
static BOOL simtop_device_fill(simtop_device *out, const simtop_handle *h, id dev) {
    @try {
        out->udid = simtop_strdup_objc(simtop_device_udid_string(dev, h));
        out->name = simtop_strdup_objc(simtop_msg_id(dev, h->selName));
        out->state = simtop_map_state(dev, h);
        if (h->selAvailable || h->selIsAvailable) {
            out->available = simtop_msg_bool(dev, h->selAvailable) ||
                             simtop_msg_bool(dev, h->selIsAvailable);
        } else {
            out->available = YES; // selector absent: nothing known, assume available
        }

        id devType = simtop_msg_id(dev, h->selDeviceType);
        if (devType) {
            out->device_type_identifier = simtop_strdup_objc(simtop_msg_id(devType, h->selIdentifier));
            out->device_type_name = simtop_strdup_objc(simtop_msg_id(devType, h->selName));
        }

        id runtime = simtop_msg_id(dev, h->selRuntime);
        if (runtime) {
            out->runtime_identifier = simtop_strdup_objc(simtop_msg_id(runtime, h->selIdentifier));
            out->runtime_name = simtop_strdup_objc(simtop_msg_id(runtime, h->selName));
            out->runtime_version = simtop_strdup_objc(simtop_msg_id(runtime, h->selVersionString));
            out->runtime_build = simtop_strdup_objc(simtop_msg_id(runtime, h->selBuildVersionString));
        }
        return YES;
    } @catch (NSException *e) {
        return NO;
    }
}

// Linear scan for a device by UDID (case-insensitive). Caller must hold the
// set and wrap in @try.
static id simtop_find_device(const simtop_handle *h, const char *udid) {
    if (!h->deviceSet || !h->selDevices || !udid || !udid[0]) return nil;
    // Snapshot the live array: count and enumeration operate on one immutable copy.
    NSArray *devices = [ ((NSArray *(*)(id, SEL))objc_msgSend)(h->deviceSet, h->selDevices) copy ];
    [devices autorelease];
    NSString *target = [NSString stringWithUTF8String:udid];
    for (id dev in devices) {
        id s = simtop_device_udid_string(dev, h);
        if (s && [s caseInsensitiveCompare:target] == NSOrderedSame) return dev;
    }
    return nil;
}

#pragma mark - Registry lookups (for create)

static NSArray *simtop_device_type_registry(const simtop_handle *h) {
    if (h->context && simtop_responds(h->context, h->selSupportedDeviceTypes)) {
        return ((NSArray *(*)(id, SEL))objc_msgSend)(h->context, h->selSupportedDeviceTypes);
    }
    if (simtop_class_responds(h->clsDeviceType, h->selSupportedDeviceTypes)) {
        return ((NSArray *(*)(id, SEL))objc_msgSend)((id)h->clsDeviceType, h->selSupportedDeviceTypes);
    }
    return nil;
}

static NSArray *simtop_runtime_registry(const simtop_handle *h) {
    if (h->context && simtop_responds(h->context, h->selSupportedRuntimes)) {
        return ((NSArray *(*)(id, SEL))objc_msgSend)(h->context, h->selSupportedRuntimes);
    }
    if (simtop_class_responds(h->clsRuntime, h->selSupportedRuntimes)) {
        return ((NSArray *(*)(id, SEL))objc_msgSend)((id)h->clsRuntime, h->selSupportedRuntimes);
    }
    return nil;
}

static id simtop_lookup_by_identifier(NSArray *items, const char *identifier,
                                      const simtop_handle *h) {
    if (!items || !identifier || !identifier[0]) return nil;
    NSString *target = [NSString stringWithUTF8String:identifier];
    NSArray *snapshot = [items copy]; // registry arrays are live; enumerate an immutable copy
    [snapshot autorelease];
    for (id item in snapshot) {
        id ident = simtop_msg_id(item, h->selIdentifier);
        if ([ident isKindOfClass:[NSString class]] &&
            [ident caseInsensitiveCompare:target] == NSOrderedSame) {
            return item;
        }
    }
    return nil;
}

#pragma mark - Exported API

uint32_t simtop_abi_version(void) { return SIMTOP_ABI_VERSION; }

simtop_handle *simtop_create(const char *developer_dir, simtop_error **out_error) {
    if (out_error) *out_error = NULL;

    if (!developer_dir || !developer_dir[0]) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG,
                                           "developer directory must not be empty", NULL);
        }
        return NULL;
    }
    struct stat st;
    if (stat(developer_dir, &st) != 0 || !S_ISDIR(st.st_mode)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG,
                                           "developer directory does not exist", developer_dir);
        }
        return NULL;
    }

    simtop_handle *h = calloc(1, sizeof(*h));
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
        return NULL;
    }

    char path[PATH_MAX];
    int n = snprintf(path, sizeof(path),
                     "%s/Library/PrivateFrameworks/CoreSimulator.framework/CoreSimulator",
                     developer_dir);
    if (n < 0 || (size_t)n >= sizeof(path)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG,
                                           "developer directory path is too long", developer_dir);
        }
        free(h);
        return NULL;
    }

    h->dl = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
    if (!h->dl) {
        // Newer Xcode ships the framework system-wide.
        h->dl = dlopen("/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator",
                       RTLD_NOW | RTLD_GLOBAL);
    }
    if (!h->dl) {
        const char *why = dlerror();
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_FRAMEWORK_LOAD,
                                           "CoreSimulator.framework could not be loaded",
                                           why ? why : path);
        }
        free(h);
        return NULL;
    }

    @autoreleasepool {
        @try {
            h->developerDir = [[NSString alloc] initWithUTF8String:developer_dir];
            simtop_probe_classes(h);
            simtop_probe_selectors(h);
            simtop_resolve_device_set(h);
            h->capabilities = simtop_compute_capabilities(h);
        } @catch (NSException *e) {
            // Contain every Objective-C exception: surface it as an owned C
            // error and destroy the partial handle.
            simtop_destroy(h);
            if (out_error) *out_error = simtop_error_from_exception(e);
            return NULL;
        }
    }
    return h;
}

void simtop_destroy(simtop_handle *h) {
    if (!h) return;
    if (h->deviceSet) { [h->deviceSet release]; h->deviceSet = nil; }
    if (h->context) { [h->context release]; h->context = nil; }
    if (h->developerDir) { [h->developerDir release]; h->developerDir = nil; }
    if (h->setError) { simtop_error_free(h->setError); h->setError = NULL; }
    if (h->dl) dlclose(h->dl);
    free(h);
}

uint32_t simtop_capabilities(const simtop_handle *h) {
    return h ? h->capabilities : 0;
}

simtop_device_set_info *simtop_copy_device_set_info(const simtop_handle *h,
                                               simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return NULL;
    }
    if (!simtop_require_device_set(h, out_error)) return NULL;

    simtop_device_set_info *info = calloc(1, sizeof(*info));
    if (!info) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
        return NULL;
    }
    @autoreleasepool {
        @try {
            info->path = simtop_strdup_objc(simtop_msg_id(h->deviceSet, h->selSetPath));
            info->name = simtop_strdup_objc(simtop_msg_id(h->deviceSet, h->selName));
        } @catch (NSException *e) {
            simtop_device_set_info_free(info);
            if (out_error) *out_error = simtop_error_from_exception(e);
            return NULL;
        }
    }
    return info;
}

void simtop_device_set_info_free(simtop_device_set_info *info) {
    if (!info) return;
    free(info->path);
    free(info->name);
    free(info);
}

simtop_device_list *simtop_list_devices(const simtop_handle *h, simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return NULL;
    }
    if (!(h->capabilities & SIMTOP_CAP_ENUMERATE)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "device enumeration is not supported by this Xcode",
                                           NULL);
        }
        return NULL;
    }
    if (!simtop_require_device_set(h, out_error)) return NULL;

    simtop_device_list *list = calloc(1, sizeof(*list));
    if (!list) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
        return NULL;
    }
    @autoreleasepool {
        @try {
            // Snapshot the live array so count and enumeration agree; the copy
            // is immutable for the duration of the loop.
            NSArray *devices = [ ((NSArray *(*)(id, SEL))objc_msgSend)(h->deviceSet, h->selDevices) copy ];
            [devices autorelease];
            NSUInteger count = [devices count];
            simtop_device *arr = calloc(count ? count : 1, sizeof(simtop_device));
            if (!arr) {
                free(list);
                if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
                return NULL;
            }
            list->devices = arr;
            list->count = count;
            NSUInteger i = 0;
            for (id dev in devices) {
                if (i >= count) break; // guard: never write past the allocation
                if (!simtop_device_fill(&arr[i], h, dev)) {
                    simtop_device_list_free(list);
                    if (out_error) {
                        *out_error = simtop_error_make(SIMTOP_ERR_EXCEPTION,
                                                       "exception while reading device metadata",
                                                       NULL);
                    }
                    return NULL;
                }
                i++;
            }
        } @catch (NSException *e) {
            simtop_device_list_free(list);
            if (out_error) *out_error = simtop_error_from_exception(e);
            return NULL;
        }
    }
    return list;
}

// Frees the eight owned string fields of a device record and zeroes the
// struct. Does NOT free the record itself: it is safe on records embedded in
// a simtop_device_list array, where only the list frees the array once.
static void simtop_device_clear(simtop_device *device) {
    if (!device) return;
    free(device->udid);
    free(device->name);
    free(device->runtime_identifier);
    free(device->runtime_name);
    free(device->runtime_version);
    free(device->runtime_build);
    free(device->device_type_identifier);
    free(device->device_type_name);
    memset(device, 0, sizeof(*device));
}

void simtop_device_list_free(simtop_device_list *list) {
    if (!list) return;
    for (size_t i = 0; i < list->count; i++) {
        simtop_device_clear(&list->devices[i]);
    }
    free(list->devices);
    free(list);
}

simtop_device *simtop_device_for_udid(const simtop_handle *h, const char *udid,
                                      simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return NULL;
    }
    if (!udid || !udid[0]) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "UDID must not be empty", NULL);
        return NULL;
    }
    if (!(h->capabilities & SIMTOP_CAP_ENUMERATE)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "device lookup is not supported by this Xcode", NULL);
        }
        return NULL;
    }
    if (!simtop_require_device_set(h, out_error)) return NULL;

    @autoreleasepool {
        @try {
            id dev = simtop_find_device(h, udid);
            if (!dev) {
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                   "device not found in the default device set",
                                                   udid);
                }
                return NULL;
            }
            simtop_device *out = calloc(1, sizeof(*out));
            if (!out) {
                if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
                return NULL;
            }
            if (!simtop_device_fill(out, h, dev)) {
                simtop_device_free(out);
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_EXCEPTION,
                                                   "exception while reading device metadata", NULL);
                }
                return NULL;
            }
            return out;
        } @catch (NSException *e) {
            if (out_error) *out_error = simtop_error_from_exception(e);
            return NULL;
        }
    }
}

void simtop_device_free(simtop_device *device) {
    if (!device) return;
    simtop_device_clear(device);
    free(device);
}

simtop_error_code simtop_boot_device(simtop_handle *h, const char *udid,
                                     simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!udid || !udid[0]) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "UDID must not be empty", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!(h->capabilities & SIMTOP_CAP_BOOT)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "booting is not supported by this Xcode", NULL);
        }
        return SIMTOP_ERR_UNSUPPORTED;
    }
    if (!simtop_require_device_set(h, out_error)) return SIMTOP_ERR_DEVICE_SET;

    @autoreleasepool {
        @try {
            id dev = simtop_find_device(h, udid);
            if (!dev) {
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                   "device not found in the default device set", udid);
                }
                return SIMTOP_ERR_NOT_FOUND;
            }
            NSError *error = nil;
            if (h->selBootWithOptions && simtop_responds(dev, h->selBootWithOptions)) {
                BOOL ok = ((BOOL(*)(id, SEL, id, NSError **))objc_msgSend)(
                    dev, h->selBootWithOptions, nil, &error);
                if (!ok) {
                    if (out_error) *out_error = simtop_error_from_nserror(SIMTOP_ERR_OPERATION, error);
                    return SIMTOP_ERR_OPERATION;
                }
                return SIMTOP_ERR_NONE;
            }
            if (h->selBootDevice && simtop_responds(dev, h->selBootDevice)) {
                ((void(*)(id, SEL, id))objc_msgSend)(dev, h->selBootDevice, nil);
                return SIMTOP_ERR_NONE;
            }
            if (out_error) {
                *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                               "no boot API is available in this Xcode", NULL);
            }
            return SIMTOP_ERR_UNSUPPORTED;
        } @catch (NSException *e) {
            if (out_error) *out_error = simtop_error_from_exception(e);
            return SIMTOP_ERR_EXCEPTION;
        }
    }
}

simtop_error_code simtop_shutdown_device(simtop_handle *h, const char *udid,
                                         simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!udid || !udid[0]) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "UDID must not be empty", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!(h->capabilities & SIMTOP_CAP_SHUTDOWN)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "shutdown is not supported by this Xcode", NULL);
        }
        return SIMTOP_ERR_UNSUPPORTED;
    }
    if (!simtop_require_device_set(h, out_error)) return SIMTOP_ERR_DEVICE_SET;

    @autoreleasepool {
        @try {
            id dev = simtop_find_device(h, udid);
            if (!dev) {
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                   "device not found in the default device set", udid);
                }
                return SIMTOP_ERR_NOT_FOUND;
            }
            NSError *error = nil;
            if (h->selShutdownWithError && simtop_responds(dev, h->selShutdownWithError)) {
                BOOL ok = ((BOOL(*)(id, SEL, NSError **))objc_msgSend)(
                    dev, h->selShutdownWithError, &error);
                if (!ok) {
                    if (out_error) *out_error = simtop_error_from_nserror(SIMTOP_ERR_OPERATION, error);
                    return SIMTOP_ERR_OPERATION;
                }
                return SIMTOP_ERR_NONE;
            }
            if (h->selShutdownDevice && simtop_responds(dev, h->selShutdownDevice)) {
                ((void(*)(id, SEL, id))objc_msgSend)(dev, h->selShutdownDevice, nil);
                return SIMTOP_ERR_NONE;
            }
            if (out_error) {
                *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                               "no shutdown API is available in this Xcode", NULL);
            }
            return SIMTOP_ERR_UNSUPPORTED;
        } @catch (NSException *e) {
            if (out_error) *out_error = simtop_error_from_exception(e);
            return SIMTOP_ERR_EXCEPTION;
        }
    }
}

simtop_device *simtop_create_device(simtop_handle *h, const simtop_create_options *options,
                                    simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return NULL;
    }
    if (!options || !options->name || !options->name[0] ||
        !options->device_type || !options->device_type[0]) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG,
                                           "create requires a non-empty name and device type", NULL);
        }
        return NULL;
    }
    if (!(h->capabilities & SIMTOP_CAP_CREATE)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "device creation is not supported by this Xcode", NULL);
        }
        return NULL;
    }
    if (!simtop_require_device_set(h, out_error)) return NULL;

    @autoreleasepool {
        @try {
            NSArray *types = simtop_device_type_registry(h);
            id type = simtop_lookup_by_identifier(types, options->device_type, h);
            if (!type) {
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                   "unknown device type", options->device_type);
                }
                return NULL;
            }

            id runtime = nil;
            if (options->runtime && options->runtime[0]) {
                NSArray *runtimes = simtop_runtime_registry(h);
                runtime = simtop_lookup_by_identifier(runtimes, options->runtime, h);
                if (!runtime) {
                    if (out_error) {
                        *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                       "unknown runtime", options->runtime);
                    }
                    return NULL;
                }
            }

            NSString *name = [NSString stringWithUTF8String:options->name];
            NSError *error = nil;
            id device = ((id(*)(id, SEL, id, id, id, NSError **))objc_msgSend)(
                h->deviceSet, h->selCreateDevice, type, runtime, name, &error);
            if (!device) {
                if (out_error) *out_error = simtop_error_from_nserror(SIMTOP_ERR_OPERATION, error);
                return NULL;
            }

            simtop_device *out = calloc(1, sizeof(*out));
            if (!out) {
                if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_ALLOC, "out of memory", NULL);
                return NULL;
            }
            if (!simtop_device_fill(out, h, device)) {
                simtop_device_free(out);
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_EXCEPTION,
                                                   "exception while reading device metadata", NULL);
                }
                return NULL;
            }
            return out;
        } @catch (NSException *e) {
            if (out_error) *out_error = simtop_error_from_exception(e);
            return NULL;
        }
    }
}

simtop_error_code simtop_delete_device(simtop_handle *h, const char *udid,
                                       simtop_error **out_error) {
    if (out_error) *out_error = NULL;
    if (!h) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "NULL handle", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!udid || !udid[0]) {
        if (out_error) *out_error = simtop_error_make(SIMTOP_ERR_INVALID_ARG, "UDID must not be empty", NULL);
        return SIMTOP_ERR_INVALID_ARG;
    }
    if (!(h->capabilities & SIMTOP_CAP_DELETE)) {
        if (out_error) {
            *out_error = simtop_error_make(SIMTOP_ERR_UNSUPPORTED,
                                           "device deletion is not supported by this Xcode", NULL);
        }
        return SIMTOP_ERR_UNSUPPORTED;
    }
    if (!simtop_require_device_set(h, out_error)) return SIMTOP_ERR_DEVICE_SET;

    @autoreleasepool {
        @try {
            id dev = simtop_find_device(h, udid);
            if (!dev) {
                if (out_error) {
                    *out_error = simtop_error_make(SIMTOP_ERR_NOT_FOUND,
                                                   "device not found in the default device set", udid);
                }
                return SIMTOP_ERR_NOT_FOUND;
            }
            NSError *error = nil;
            BOOL ok = ((BOOL(*)(id, SEL, id, NSError **))objc_msgSend)(
                h->deviceSet, h->selDeleteDevice, dev, &error);
            if (!ok) {
                if (out_error) *out_error = simtop_error_from_nserror(SIMTOP_ERR_OPERATION, error);
                return SIMTOP_ERR_OPERATION;
            }
            return SIMTOP_ERR_NONE;
        } @catch (NSException *e) {
            if (out_error) *out_error = simtop_error_from_exception(e);
            return SIMTOP_ERR_EXCEPTION;
        }
    }
}

void simtop_error_free(simtop_error *error) {
    if (!error) return;
    free(error->message);
    free(error->detail);
    free(error);
}
