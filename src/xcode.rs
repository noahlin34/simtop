//! Xcode environment resolution and tool-path derivation.
//!
//! The developer directory is resolved in strict precedence order:
//! 1. explicit `--developer-dir` override,
//! 2. the `DEVELOPER_DIR` environment variable,
//! 3. `/usr/bin/xcode-select -p`.
//!
//! A resolved directory is validated before use, and all tool locations are
//! derived from it so the rest of the program never consults `xcrun` or other
//! ambient machinery. `xcode-select` is invoked only here, once, at startup.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ErrorCode, SimtopError};

/// A validated Xcode developer directory plus the tool locations derived
/// from it.
#[derive(Debug, Clone)]
pub struct XcodeEnvironment {
    developer_dir: PathBuf,
    simctl: PathBuf,
    simulator_app: PathBuf,
    coresimulator_framework: PathBuf,
}

impl XcodeEnvironment {
    /// Resolve the developer directory using the documented precedence:
    /// explicit override, then `DEVELOPER_DIR`, then `xcode-select -p`.
    pub fn resolve(override_dir: Option<&Path>) -> Result<Self, SimtopError> {
        if let Some(dir) = override_dir {
            return Self::from_developer_dir(dir);
        }
        if let Some(dir) = env::var_os("DEVELOPER_DIR") {
            let dir = PathBuf::from(dir);
            if !dir.as_os_str().is_empty() {
                return Self::from_developer_dir(&dir);
            }
        }
        let output = Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| {
                SimtopError::with_source(
                    ErrorCode::XcodeNotFound,
                    "failed to run /usr/bin/xcode-select -p".to_string(),
                    e,
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SimtopError::new(
                ErrorCode::XcodeNotFound,
                format!(
                    "xcode-select -p failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if path.as_os_str().is_empty() {
            return Err(SimtopError::new(
                ErrorCode::XcodeNotFound,
                "xcode-select -p returned an empty path".to_string(),
            ));
        }
        Self::from_developer_dir(&path)
    }

    /// Validate an already-resolved developer directory and derive tool
    /// locations from it. Accepts either the `Contents/Developer` directory
    /// itself or the containing `.app` bundle; the latter is normalized.
    pub fn from_developer_dir(dir: &Path) -> Result<Self, SimtopError> {
        if !dir.is_dir() {
            return Err(SimtopError::new(
                ErrorCode::InvalidDeveloperDir,
                format!("developer directory does not exist: {}", dir.display()),
            ));
        }
        // Accept `…/Xcode.app` as well as `…/Xcode.app/Contents/Developer`.
        let developer_dir = if dir.join("usr/bin/simctl").is_file() {
            dir.to_path_buf()
        } else {
            let nested = dir.join("Contents/Developer");
            if nested.join("usr/bin/simctl").is_file() {
                nested
            } else {
                return Err(SimtopError::new(
                    ErrorCode::InvalidDeveloperDir,
                    format!(
                        "{} does not contain a usable Xcode developer directory (no usr/bin/simctl)",
                        dir.display()
                    ),
                ));
            }
        };
        let simctl = developer_dir.join("usr/bin/simctl");
        if !simctl.is_file() {
            return Err(SimtopError::new(
                ErrorCode::InvalidDeveloperDir,
                format!("simctl not found at {}", simctl.display()),
            ));
        }
        let simulator_app = developer_dir.join("Applications/Simulator.app");
        let coresimulator_framework =
            developer_dir.join("Library/PrivateFrameworks/CoreSimulator.framework");
        Ok(Self {
            developer_dir,
            simctl,
            simulator_app,
            coresimulator_framework,
        })
    }

    /// The resolved developer directory (`…/Contents/Developer`).
    pub fn developer_dir(&self) -> &Path {
        &self.developer_dir
    }

    /// Path to the `simctl` binary.
    pub fn simctl_path(&self) -> &Path {
        &self.simctl
    }

    /// Path to the Simulator.app UI bundle.
    pub fn simulator_app_path(&self) -> &Path {
        &self.simulator_app
    }

    /// Path to the CoreSimulator.framework used by the native bridge.
    pub fn coresimulator_framework_path(&self) -> &Path {
        &self.coresimulator_framework
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Serializes the tests that read/write `DEVELOPER_DIR`; libtest runs
    /// tests on parallel threads and `std::env` is process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restores `DEVELOPER_DIR` on drop so tests never leak process env.
    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            EnvGuard { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    /// A unique scratch root under the system temp dir, cleaned before use
    /// so a stale leftover from a previous run cannot leak into the test.
    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "simtop-xcode-test-{}-{}-{nanos}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create scratch dir");
        root
    }

    /// A fake but structurally valid developer directory with a simctl
    /// binary in place; returns the directory itself.
    fn fake_developer_dir(tag: &str) -> PathBuf {
        let root = scratch(tag);
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/simctl"), "").unwrap();
        root
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn from_developer_dir_rejects_missing_directory() {
        let missing = std::env::temp_dir().join("simtop-xcode-test-does-not-exist");
        let _ = fs::remove_dir_all(&missing);
        let err = XcodeEnvironment::from_developer_dir(&missing).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidDeveloperDir);
        assert!(err.message().contains("does not exist"));
    }

    #[test]
    fn from_developer_dir_rejects_directory_without_simctl() {
        let root = scratch("no-simctl");
        let err = XcodeEnvironment::from_developer_dir(&root).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidDeveloperDir);
        assert!(err.message().contains("no usr/bin/simctl"));
        cleanup(&root);
    }

    #[test]
    fn from_developer_dir_accepts_developer_directory() {
        let root = fake_developer_dir("developer-dir");
        let env = XcodeEnvironment::from_developer_dir(&root).unwrap();
        assert_eq!(env.developer_dir(), root.as_path());
        assert_eq!(env.simctl_path(), root.join("usr/bin/simctl").as_path());
        assert_eq!(
            env.simulator_app_path(),
            root.join("Applications/Simulator.app").as_path()
        );
        assert_eq!(
            env.coresimulator_framework_path(),
            root.join("Library/PrivateFrameworks/CoreSimulator.framework")
                .as_path()
        );
        cleanup(&root);
    }

    #[test]
    fn from_developer_dir_normalizes_app_bundle() {
        let root = scratch("app-bundle");
        let developer = root.join("Xcode.app/Contents/Developer");
        fs::create_dir_all(developer.join("usr/bin")).unwrap();
        fs::write(developer.join("usr/bin/simctl"), "").unwrap();
        let env = XcodeEnvironment::from_developer_dir(&root.join("Xcode.app")).unwrap();
        assert_eq!(env.developer_dir(), developer.as_path());
        assert_eq!(
            env.simctl_path(),
            developer.join("usr/bin/simctl").as_path()
        );
        cleanup(&root);
    }

    #[test]
    fn resolve_override_wins_over_environment() {
        // Recover from poisoning rather than unwrap: a panicked test holds
        // the lock only for the env-mutation window, and the env is restored
        // by the EnvGuard before any successor test runs.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dev = fake_developer_dir("override");
        // Point DEVELOPER_DIR at a bogus path; the explicit override must
        // win without consulting the environment.
        let _env = EnvGuard::set("DEVELOPER_DIR", "/nonexistent/simtop-bogus");
        let env = XcodeEnvironment::resolve(Some(&dev)).unwrap();
        assert_eq!(env.developer_dir(), dev.as_path());
        cleanup(&dev);
    }

    #[test]
    fn resolve_honors_developer_dir_environment() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dev = fake_developer_dir("env-dir");
        let _env = EnvGuard::set("DEVELOPER_DIR", &dev);
        let env = XcodeEnvironment::resolve(None).unwrap();
        assert_eq!(env.developer_dir(), dev.as_path());
        cleanup(&dev);
    }
}
