//! Build script: compiles the Objective-C CoreSimulator bridge.
//!
//! The bridge (`native/SimtopCoreSimulator.m`) is compiled into a small
//! static archive with the `cc` crate. It deliberately does NOT link
//! CoreSimulator.framework — that framework is private API and is loaded at
//! runtime with dlopen(3) from the selected Xcode developer directory (see
//! `native/SimtopCoreSimulator.h`). Only Foundation (and libobjc) are
//! linked here.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=native/SimtopCoreSimulator.h");
    println!("cargo:rerun-if-changed=native/SimtopCoreSimulator.m");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");

    // The `simtop_no_native` cfg is set below on non-macOS targets and
    // consulted throughout src/native.rs; register it so rustc's
    // unexpected_cfgs lint does not flag it.
    println!("cargo:rustc-check-cfg=cfg(simtop_no_native)");

    // The bridge is macOS-only. On other platforms compile the crate with a
    // stub `native` module (cfg `simtop_no_native`) so documentation builds
    // and cross-compilation checks still work; runtime operation is
    // unsupported there anyway.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=simtop's native CoreSimulator bridge requires macOS; compiling without it");
        println!("cargo:rustc-cfg=simtop_no_native");
        return;
    }

    // simtop targets macOS 15+; honor an explicit deployment-target override.
    let min_version = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "15.0".to_string());

    cc::Build::new()
        .file("native/SimtopCoreSimulator.m")
        // The bridge manages Objective-C ownership manually (C-ABI records).
        .flag("-fno-objc-arc")
        // ...and relies on @try/@catch (NSException), which requires
        // Objective-C exception support.
        .flag("-fobjc-exceptions")
        .flag(&format!("-mmacosx-version-min={min_version}"))
        .compile("simtop_native");

    // The `cc` crate has no framework API (cc 1.x), so emit the link
    // directives directly. Foundation is public API; CoreSimulator is NOT
    // linked here (it is dlopen(3)'d at runtime).
    println!("cargo:rustc-link-lib=framework=Foundation");

    // libobjc is pulled in transitively by Foundation; keep the dependency
    // explicit so the archive always resolves its runtime symbols.
    println!("cargo:rustc-link-lib=objc");
}
