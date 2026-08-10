//! Cancellable, event-driven Xcode builds.

use crate::error::{ErrorCode, SimtopError};
use crate::project::{ProjectId, XcodeContainer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

const EVENT_CHANNEL_CAPACITY: usize = 256;
const MAX_OUTPUT_LINE_BYTES: usize = 32 * 1024;
const MAX_STDERR_BYTES: usize = 256 * 1024;
const MAX_SETTINGS_BYTES: usize = 8 * 1024 * 1024;

/// The complete selection and filesystem context for one Xcode build.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub project_id: ProjectId,
    pub container: XcodeContainer,
    pub scheme: String,
    pub configuration: String,
    pub simulator_udid: String,
    pub developer_dir: PathBuf,
    pub cache_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStage {
    Building,
    ResolvingProduct,
}

#[derive(Debug)]
pub enum BuildEvent {
    Stage(BuildStage),
    /// One bounded line from either stdout or stderr.
    Output(String),
    Finished(Result<BuildProduct, SimtopError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildProduct {
    pub app_path: PathBuf,
    pub bundle_id: String,
}

/// Result returned by callers which do not consume the event stream directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    pub product: BuildProduct,
}

impl BuildResult {
    pub fn app_path(&self) -> &Path {
        &self.product.app_path
    }

    pub fn bundle_id(&self) -> &str {
        &self.product.bundle_id
    }
}

#[derive(Debug)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Debug, Clone)]
struct BuildCancellation {
    inner: Arc<CancellationInner>,
}

impl BuildCancellation {
    fn new() -> Self {
        Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        // Register before checking the flag so a cancellation between the
        // check and the await cannot leave this future asleep forever.
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Handle for a running build. Dropping it requests cancellation.
pub struct BuildHandle {
    receiver: mpsc::Receiver<BuildEvent>,
    cancellation: BuildCancellation,
    task: Option<JoinHandle<()>>,
}

impl BuildHandle {
    pub async fn recv(&mut self) -> Option<BuildEvent> {
        self.receiver.recv().await
    }

    pub fn events(&mut self) -> &mut mpsc::Receiver<BuildEvent> {
        &mut self.receiver
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        // A caller may use `join` only to await completion and never consume
        // events. Drop the receiver first so a full bounded channel cannot
        // keep the build task blocked forever.
        let (_sender, replacement) = mpsc::channel(1);
        let receiver = std::mem::replace(&mut self.receiver, replacement);
        drop(receiver);
        self.task
            .take()
            .expect("build task is present until BuildHandle is consumed")
            .await
    }
}

impl Drop for BuildHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Runs one Xcode build at a time and provides a bounded event stream.
#[derive(Debug, Clone)]
pub struct XcodeBuildRunner {
    active: Arc<AtomicBool>,
    current: Arc<Mutex<Option<BuildCancellation>>>,
}

impl Default for XcodeBuildRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl XcodeBuildRunner {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// Start a build and return its bounded event stream immediately.
    pub fn start(&self, request: BuildRequest) -> Result<BuildHandle, SimtopError> {
        validate_request(&request)?;
        if self.active.swap(true, Ordering::AcqRel) {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                "an Xcode build is already running on this runner",
            ));
        }

        let cancellation = BuildCancellation::new();
        {
            let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
            *current = Some(cancellation.clone());
        }

        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let active = Arc::clone(&self.active);
        let current = Arc::clone(&self.current);
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            execute(request, task_cancellation.clone(), sender).await;
            task_cancellation.cancel();
            // Clear the cancellation slot before publishing the inactive
            // state. A new start cannot race with stale cleanup and lose its
            // cancellation handle.
            let mut slot = current.lock().unwrap_or_else(|e| e.into_inner());
            *slot = None;
            active.store(false, Ordering::Release);
        });

        Ok(BuildHandle {
            receiver,
            cancellation,
            task: Some(task),
        })
    }

    pub fn cancel(&self) {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cancellation) = current.as_ref() {
            cancellation.cancel();
        }
    }
}

fn validate_request(request: &BuildRequest) -> Result<(), SimtopError> {
    if request.scheme.trim().is_empty() {
        return Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            "build scheme must not be empty",
        ));
    }
    if request.configuration.trim().is_empty() {
        return Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            "build configuration must not be empty",
        ));
    }
    if request.simulator_udid.trim().is_empty() {
        return Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            "simulator UDID must not be empty",
        ));
    }
    if request.developer_dir.as_os_str().is_empty() {
        return Err(SimtopError::new(
            ErrorCode::InvalidDeveloperDir,
            "developer directory must not be empty",
        ));
    }
    if request.cache_root.as_os_str().is_empty() {
        return Err(SimtopError::new(
            ErrorCode::InvalidArgument,
            "build cache root must not be empty",
        ));
    }
    Ok(())
}

async fn execute(
    request: BuildRequest,
    cancellation: BuildCancellation,
    sender: mpsc::Sender<BuildEvent>,
) {
    let event = match execute_inner(&request, &cancellation, &sender).await {
        Ok(result) => BuildEvent::Finished(Ok(result.product)),
        Err(error) => BuildEvent::Finished(Err(error)),
    };
    // Terminal delivery must observe cancellation as well: if the bounded
    // event stream is full and the handle was dropped, waiting on send alone
    // would keep the task (and runner) alive indefinitely.
    let _ = send_event(&sender, &cancellation, event).await;
}

async fn execute_inner(
    request: &BuildRequest,
    cancellation: &BuildCancellation,
    sender: &mpsc::Sender<BuildEvent>,
) -> Result<BuildResult, SimtopError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }

    let derived_data = derived_data_path(request);
    tokio::fs::create_dir_all(&derived_data)
        .await
        .map_err(|error| {
            SimtopError::with_source(
                ErrorCode::IoError,
                format!(
                    "failed to create derived data directory {}",
                    derived_data.display()
                ),
                error,
            )
        })?;

    if !send_event(
        sender,
        cancellation,
        BuildEvent::Stage(BuildStage::Building),
    )
    .await
    {
        return Err(cancelled_error());
    }
    run_build_command(request, &derived_data, cancellation, sender).await?;

    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    if !send_event(
        sender,
        cancellation,
        BuildEvent::Stage(BuildStage::ResolvingProduct),
    )
    .await
    {
        return Err(cancelled_error());
    }
    let settings = run_settings_command(request, &derived_data, cancellation, sender).await?;
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let product = resolve_application_product(&settings)?;
    if !product.app_path.is_dir() {
        return Err(SimtopError::new(
            ErrorCode::CommandFailed,
            format!(
                "xcodebuild reported application product {}, but that path does not exist",
                product.app_path.display()
            ),
        ));
    }
    Ok(BuildResult { product })
}

async fn send_event(
    sender: &mpsc::Sender<BuildEvent>,
    cancellation: &BuildCancellation,
    event: BuildEvent,
) -> bool {
    tokio::select! {
        result = sender.send(event) => result.is_ok(),
        _ = cancellation.cancelled() => false,
    }
}

async fn run_build_command(
    request: &BuildRequest,
    derived_data: &Path,
    cancellation: &BuildCancellation,
    sender: &mpsc::Sender<BuildEvent>,
) -> Result<(), SimtopError> {
    let args = build_arguments(request, derived_data);
    let mut child = spawn_xcodebuild(request, &args)?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SimtopError::new(
                ErrorCode::Internal,
                "xcodebuild stdout pipe was not available",
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SimtopError::new(
                ErrorCode::Internal,
                "xcodebuild stderr pipe was not available",
            ));
        }
    };
    let stdout_task = tokio::spawn(stream_output(
        stdout,
        sender.clone(),
        cancellation.clone(),
        false,
    ));
    let stderr_task = tokio::spawn(stream_output(
        stderr,
        sender.clone(),
        cancellation.clone(),
        true,
    ));
    let (status, was_cancelled) = wait_for_child(&mut child, cancellation).await;
    let stdout_result = stdout_task.await.map_err(join_error)?;
    let stderr_result = stderr_task.await.map_err(join_error)?;
    let stderr_text = stderr_result.diagnostics;
    let _ = stdout_result;

    if was_cancelled || cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let status = status.map_err(|error| {
        SimtopError::with_source(
            ErrorCode::CommandFailed,
            "failed waiting for xcodebuild",
            error,
        )
    })?;
    if !status.success() {
        return Err(command_failed(
            format!("xcodebuild build failed with status {}", status),
            &stderr_text,
        ));
    }
    Ok(())
}

async fn run_settings_command(
    request: &BuildRequest,
    derived_data: &Path,
    cancellation: &BuildCancellation,
    sender: &mpsc::Sender<BuildEvent>,
) -> Result<String, SimtopError> {
    let args = settings_arguments(request, derived_data);
    let mut child = spawn_xcodebuild(request, &args)?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SimtopError::new(
                ErrorCode::Internal,
                "xcodebuild stdout pipe was not available",
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SimtopError::new(
                ErrorCode::Internal,
                "xcodebuild stderr pipe was not available",
            ));
        }
    };
    let stdout_task = tokio::spawn(read_settings_stdout(stdout));
    let stderr_task = tokio::spawn(stream_output(
        stderr,
        sender.clone(),
        cancellation.clone(),
        true,
    ));
    let (status, was_cancelled) = wait_for_child(&mut child, cancellation).await;
    let stdout_result = stdout_task.await.map_err(join_error)?;
    let stderr_result = stderr_task.await.map_err(join_error)?;
    let stderr_text = stderr_result.diagnostics;

    if was_cancelled || cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let status = status.map_err(|error| {
        SimtopError::with_source(
            ErrorCode::CommandFailed,
            "failed waiting for xcodebuild build settings",
            error,
        )
    })?;
    let settings = stdout_result?;
    if !status.success() {
        return Err(command_failed(
            format!("xcodebuild build settings failed with status {}", status),
            &stderr_text,
        ));
    }
    Ok(settings)
}

fn spawn_xcodebuild(request: &BuildRequest, args: &[String]) -> Result<Child, SimtopError> {
    let executable = request.developer_dir.join("usr/bin/xcodebuild");
    let mut command = Command::new(&executable);
    command
        .args(args)
        .env("DEVELOPER_DIR", &request.developer_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command.spawn().map_err(|error| {
        SimtopError::with_source(
            ErrorCode::XcodeNotFound,
            format!("failed to launch {}", executable.display()),
            error,
        )
    })
}

async fn wait_for_child(
    child: &mut Child,
    cancellation: &BuildCancellation,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    if cancellation.is_cancelled() {
        let _ = child.kill().await;
        return (child.wait().await, true);
    }
    tokio::select! {
        result = child.wait() => (result, false),
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            (child.wait().await, true)
        }
    }
}

#[derive(Debug)]
struct OutputCapture {
    diagnostics: String,
}

async fn stream_output<R>(
    reader: R,
    sender: mpsc::Sender<BuildEvent>,
    cancellation: BuildCancellation,
    capture_diagnostics: bool,
) -> OutputCapture
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut reader = reader;
    let mut bytes = [0_u8; 8192];
    let mut line = Vec::with_capacity(MAX_OUTPUT_LINE_BYTES);
    let mut truncated = false;
    let mut diagnostics = String::new();

    loop {
        if cancellation.is_cancelled() {
            break;
        }
        let read = match reader.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in bytes[..read].iter().copied() {
            if byte == b'\n' {
                let text = line_to_string(&line, truncated);
                if capture_diagnostics {
                    append_diagnostic(&mut diagnostics, &text);
                }
                if !send_event(&sender, &cancellation, BuildEvent::Output(text)).await {
                    return OutputCapture { diagnostics };
                }
                line.clear();
                truncated = false;
            } else if line.len() < MAX_OUTPUT_LINE_BYTES {
                line.push(byte);
            } else {
                truncated = true;
            }
        }
    }

    if !line.is_empty() || truncated {
        let text = line_to_string(&line, truncated);
        if capture_diagnostics {
            append_diagnostic(&mut diagnostics, &text);
        }
        let _ = send_event(&sender, &cancellation, BuildEvent::Output(text)).await;
    }
    OutputCapture { diagnostics }
}

fn line_to_string(bytes: &[u8], truncated: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.ends_with('\r') {
        text.pop();
    }
    if truncated {
        text.push_str(" [line truncated]");
    }
    text
}

fn append_diagnostic(diagnostics: &mut String, line: &str) {
    if diagnostics.len() >= MAX_STDERR_BYTES {
        return;
    }
    let remaining = MAX_STDERR_BYTES - diagnostics.len();
    if line.len() < remaining {
        diagnostics.push_str(line);
        diagnostics.push('\n');
        return;
    }
    let keep = remaining.saturating_sub(24).min(line.len());
    let mut end = keep;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    diagnostics.push_str(&line[..end]);
    diagnostics.push_str("\n[stderr truncated]\n");
}

async fn read_settings_stdout<R>(mut reader: R) -> Result<String, SimtopError>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut output = Vec::new();
    let mut bytes = [0_u8; 8192];
    loop {
        let read = reader.read(&mut bytes).await.map_err(SimtopError::from)?;
        if read == 0 {
            break;
        }
        if output.len() + read > MAX_SETTINGS_BYTES {
            return Err(SimtopError::new(
                ErrorCode::ParseError,
                format!("xcodebuild build settings exceeded {MAX_SETTINGS_BYTES} bytes"),
            ));
        }
        output.extend_from_slice(&bytes[..read]);
    }
    String::from_utf8(output).map_err(|error| {
        SimtopError::with_source(
            ErrorCode::ParseError,
            "xcodebuild build settings were not valid UTF-8",
            error,
        )
    })
}

fn join_error(error: tokio::task::JoinError) -> SimtopError {
    SimtopError::with_source(ErrorCode::Internal, "build output task failed", error)
}

fn command_failed(message: String, stderr: &str) -> SimtopError {
    if stderr.trim().is_empty() {
        SimtopError::new(ErrorCode::CommandFailed, message)
    } else {
        SimtopError::new(
            ErrorCode::CommandFailed,
            format!("{message}; stderr:\n{}", stderr.trim_end()),
        )
    }
}

fn cancelled_error() -> SimtopError {
    SimtopError::new(ErrorCode::Timeout, "xcodebuild was cancelled")
}

/// Compute the stable per-project derived data directory.
pub fn derived_data_path(request: &BuildRequest) -> PathBuf {
    let project_key = request.project_id.as_str();
    let readable = Path::new(project_key)
        .file_stem()
        .and_then(|name| name.to_str())
        .map(sanitize_component)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let hash = stable_hash(project_key.as_bytes());
    request
        .cache_root
        .join("derived-data")
        .join(format!("{readable}-{hash:016x}"))
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Construct the exact argument array for a regular build.
pub fn build_arguments(request: &BuildRequest, derived_data: &Path) -> Vec<String> {
    let mut args = selection_arguments(request);
    args.extend([
        "-derivedDataPath".to_string(),
        derived_data.to_string_lossy().into_owned(),
        "build".to_string(),
    ]);
    args
}

/// Construct the exact argument array for structured build settings.
pub fn settings_arguments(request: &BuildRequest, derived_data: &Path) -> Vec<String> {
    let mut args = selection_arguments(request);
    args.extend([
        "-derivedDataPath".to_string(),
        derived_data.to_string_lossy().into_owned(),
        "-showBuildSettings".to_string(),
        "-json".to_string(),
    ]);
    args
}

fn selection_arguments(request: &BuildRequest) -> Vec<String> {
    let (kind, path) = match &request.container {
        XcodeContainer::Workspace(path) => ("-workspace", path),
        XcodeContainer::Project(path) => ("-project", path),
    };
    vec![
        kind.to_string(),
        path.to_string_lossy().into_owned(),
        "-scheme".to_string(),
        request.scheme.clone(),
        "-configuration".to_string(),
        request.configuration.clone(),
        "-destination".to_string(),
        format!("id={}", request.simulator_udid),
    ]
}

/// Parse `xcodebuild -showBuildSettings -json` and resolve exactly one app.
/// No filesystem traversal is performed.
pub fn resolve_application_product(json: &str) -> Result<BuildProduct, SimtopError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        SimtopError::with_source(
            ErrorCode::ParseError,
            "failed to parse xcodebuild build settings JSON",
            error,
        )
    })?;
    let mut candidates = Vec::new();
    collect_candidates(&value, &mut candidates);
    candidates.sort_by(|left, right| {
        left.app_path
            .cmp(&right.app_path)
            .then_with(|| left.bundle_id.cmp(&right.bundle_id))
    });
    candidates.dedup();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(SimtopError::new(ErrorCode::ParseError, "xcodebuild settings contained no application product (expected TARGET_BUILD_DIR, WRAPPER_NAME, PRODUCT_BUNDLE_IDENTIFIER, and an application product indicator)")),
        count => {
            let products = candidates.iter().map(|candidate| format!("{} ({})", candidate.app_path.display(), candidate.bundle_id)).collect::<Vec<_>>().join(", ");
            Err(SimtopError::new(ErrorCode::ParseError, format!("xcodebuild settings contained {count} application products; expected exactly one: {products}")))
        }
    }
}

fn collect_candidates(value: &serde_json::Value, candidates: &mut Vec<BuildProduct>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_candidates(value, candidates);
            }
        }
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::Object(settings)) = object.get("buildSettings") {
                if let Some(product) = candidate_from_settings(settings) {
                    candidates.push(product);
                }
            }
            for value in object.values() {
                collect_candidates(value, candidates);
            }
        }
        _ => {}
    }
}

fn candidate_from_settings(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Option<BuildProduct> {
    let target_dir = setting_string(settings, "TARGET_BUILD_DIR")?;
    let wrapper = setting_string(settings, "WRAPPER_NAME")?;
    let bundle_id = setting_string(settings, "PRODUCT_BUNDLE_IDENTIFIER")?;
    if target_dir.is_empty() || wrapper.is_empty() || bundle_id.is_empty() {
        return None;
    }
    let product_type = setting_string(settings, "PRODUCT_TYPE").unwrap_or_default();
    let wrapper_extension = setting_string(settings, "WRAPPER_EXTENSION").unwrap_or_default();
    let is_application = product_type == "com.apple.product-type.application"
        || wrapper_extension.eq_ignore_ascii_case("app")
        || Path::new(wrapper)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("app"))
            .unwrap_or(false);
    if !is_application {
        return None;
    }
    Some(BuildProduct {
        app_path: Path::new(target_dir).join(wrapper),
        bundle_id: bundle_id.to_string(),
    })
}

fn setting_string<'a>(
    settings: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    settings.get(key).and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn request(container: XcodeContainer) -> BuildRequest {
        BuildRequest {
            project_id: ProjectId("/tmp/test.xcodeproj".to_string()),
            container,
            scheme: "App".to_string(),
            configuration: "Debug".to_string(),
            simulator_udid: "SIM".to_string(),
            developer_dir: PathBuf::from("/Applications/Xcode.app/Contents/Developer"),
            cache_root: PathBuf::from("/tmp/simtop"),
        }
    }

    #[test]
    fn build_arguments_use_workspace_selection_and_no_shell() {
        let args = build_arguments(
            &request(XcodeContainer::Workspace(PathBuf::from("App.xcworkspace"))),
            Path::new("/tmp/derived"),
        );
        assert_eq!(
            args,
            vec![
                "-workspace",
                "App.xcworkspace",
                "-scheme",
                "App",
                "-configuration",
                "Debug",
                "-destination",
                "id=SIM",
                "-derivedDataPath",
                "/tmp/derived",
                "build"
            ]
        );
    }

    #[test]
    fn settings_resolve_one_application_without_searching() {
        let json = r#"[{"buildSettings":{"TARGET_BUILD_DIR":"/tmp/Build/Products/Debug","WRAPPER_NAME":"App.app","PRODUCT_BUNDLE_IDENTIFIER":"com.example.app","PRODUCT_TYPE":"com.apple.product-type.application"}}]"#;
        let product = resolve_application_product(json).unwrap();
        assert_eq!(
            product.app_path,
            PathBuf::from("/tmp/Build/Products/Debug/App.app")
        );
        assert_eq!(product.bundle_id, "com.example.app");
    }

    #[test]
    fn settings_reject_multiple_applications() {
        let json = r#"[{"buildSettings":{"TARGET_BUILD_DIR":"/tmp/a","WRAPPER_NAME":"A.app","PRODUCT_BUNDLE_IDENTIFIER":"a","PRODUCT_TYPE":"com.apple.product-type.application"}},{"buildSettings":{"TARGET_BUILD_DIR":"/tmp/b","WRAPPER_NAME":"B.app","PRODUCT_BUNDLE_IDENTIFIER":"b","PRODUCT_TYPE":"com.apple.product-type.application"}}]"#;
        let error = resolve_application_product(json).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ParseError);
        assert!(error.message().contains("exactly one"));
    }
}
