//! Cancellable project build, install, and launch orchestration.
//!
//! A [`ProjectRunCoordinator`] owns the simulator backend and Xcode build
//! runner used for one end-to-end project operation.  The operation is
//! deliberately event driven: callers receive bounded stage/output events
//! immediately and may cancel without blocking the input loop.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use crate::backend::SimulatorBackend;
use crate::build::{BuildEvent, BuildProduct, BuildRequest, BuildStage, XcodeBuildRunner};
use crate::error::{ErrorCode, SimtopError};

/// Maximum number of project-run events buffered for a consumer.
///
/// Build output is already line bounded by [`crate::build`].  Keeping this
/// channel bounded as well prevents an unattended project operation from
/// accumulating output indefinitely.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Stage of an end-to-end project operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRunStage {
    /// Ensure the selected simulator is booted.
    Booting,
    /// Run the regular xcodebuild action.
    Building,
    /// Resolve the built application product from build settings.
    ResolvingProduct,
    /// Install the resolved application on the simulator.
    Installing,
    /// Launch the installed application.
    Launching,
}

/// Event emitted by a running project operation.
#[derive(Debug)]
pub enum ProjectRunEvent {
    /// The operation entered a stage.
    Stage(ProjectRunStage),
    /// One bounded line forwarded from xcodebuild.
    Output(String),
    /// Terminal event.  Exactly one is emitted while the receiver remains
    /// connected.
    Finished(Result<ProjectRunResult, SimtopError>),
}

/// Successful result of a project operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRunResult {
    /// The application product that was built and launched.
    pub product: BuildProduct,
}

impl ProjectRunResult {
    /// Path of the application product.
    pub fn app_path(&self) -> &Path {
        &self.product.app_path
    }

    /// Bundle identifier launched on the simulator.
    pub fn bundle_id(&self) -> &str {
        &self.product.bundle_id
    }
}

/// Translate a build stage into its project-operation stage.
///
/// Keeping this mapping pure makes the stage contract independently testable
/// and ensures build events cannot leak the lower-level [`BuildStage`] type to
/// UI consumers.
pub fn map_build_stage(stage: BuildStage) -> ProjectRunStage {
    match stage {
        BuildStage::Building => ProjectRunStage::Building,
        BuildStage::ResolvingProduct => ProjectRunStage::ResolvingProduct,
    }
}

#[derive(Debug)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Debug, Clone)]
struct RunCancellation {
    inner: Arc<CancellationInner>,
}

impl RunCancellation {
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
        // check and await cannot leave this future asleep forever.
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Handle for a running project operation.  Dropping it requests
/// cancellation of both the coordinator task and any active xcodebuild.
pub struct ProjectRunHandle {
    receiver: mpsc::Receiver<ProjectRunEvent>,
    cancellation: RunCancellation,
    task: Option<JoinHandle<()>>,
}

impl ProjectRunHandle {
    /// Receive the next project event.
    pub async fn recv(&mut self) -> Option<ProjectRunEvent> {
        self.receiver.recv().await
    }

    /// Access the bounded event receiver directly.
    pub fn events(&mut self) -> &mut mpsc::Receiver<ProjectRunEvent> {
        &mut self.receiver
    }

    /// Cooperatively cancel the current operation.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Cancel (if necessary), then await the coordinator task.
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        self.cancel();
        // A caller may join without consuming events.  Drop the receiver so a
        // full bounded channel cannot keep the task blocked forever.
        let (_sender, replacement) = mpsc::channel(1);
        let receiver = std::mem::replace(&mut self.receiver, replacement);
        drop(receiver);
        self.task
            .take()
            .expect("project run task is present until ProjectRunHandle is consumed")
            .await
    }
}

impl Drop for ProjectRunHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Coordinates one cancellable build/install/launch operation at a time.
#[derive(Clone)]
pub struct ProjectRunCoordinator {
    backend: Arc<dyn SimulatorBackend>,
    build_runner: XcodeBuildRunner,
    active: Arc<AtomicBool>,
    current: Arc<Mutex<Option<RunCancellation>>>,
}

impl ProjectRunCoordinator {
    /// Construct a coordinator from a shared simulator backend and build
    /// runner.  The backend may be shared with simulator polling tasks.
    pub fn new(backend: Arc<dyn SimulatorBackend>, build_runner: XcodeBuildRunner) -> Self {
        Self {
            backend,
            build_runner,
            active: Arc::new(AtomicBool::new(false)),
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// Start a project operation and return its bounded event stream
    /// immediately.
    pub fn start(&self, request: BuildRequest) -> Result<ProjectRunHandle, SimtopError> {
        if self.active.swap(true, Ordering::AcqRel) {
            return Err(SimtopError::new(
                ErrorCode::InvalidArgument,
                "a project run is already active on this coordinator",
            ));
        }

        let cancellation = RunCancellation::new();
        {
            let mut current = self
                .current
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *current = Some(cancellation.clone());
        }
        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let backend = Arc::clone(&self.backend);
        let build_runner = self.build_runner.clone();
        let task_cancellation = cancellation.clone();
        let active = Arc::clone(&self.active);
        let current = Arc::clone(&self.current);
        let task = tokio::spawn(async move {
            execute(
                request,
                backend,
                build_runner,
                task_cancellation.clone(),
                sender,
            )
            .await;
            let mut slot = current.lock().unwrap_or_else(|error| error.into_inner());
            *slot = None;
            active.store(false, Ordering::Release);
        });

        Ok(ProjectRunHandle {
            receiver,
            cancellation,
            task: Some(task),
        })
    }

    /// Request cancellation of the current operation, if any.
    pub fn cancel(&self) {
        let current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(cancellation) = current.as_ref() {
            cancellation.cancel();
        }
    }
}

async fn execute(
    request: BuildRequest,
    backend: Arc<dyn SimulatorBackend>,
    build_runner: XcodeBuildRunner,
    cancellation: RunCancellation,
    sender: mpsc::Sender<ProjectRunEvent>,
) {
    let result = execute_inner(&request, backend, build_runner, &cancellation, &sender).await;
    // A cancellation while boot/install/launch is in progress is observed
    // immediately after that backend call and must never become success.
    let result = match result {
        Ok(_) if cancellation.is_cancelled() => Err(cancelled_error(ProjectRunStage::Launching)),
        other => other,
    };
    let _ = send_finished(&sender, result).await;
}

async fn execute_inner(
    request: &BuildRequest,
    backend: Arc<dyn SimulatorBackend>,
    build_runner: XcodeBuildRunner,
    cancellation: &RunCancellation,
    sender: &mpsc::Sender<ProjectRunEvent>,
) -> Result<ProjectRunResult, SimtopError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error(ProjectRunStage::Booting));
    }

    if !send_event(
        sender,
        cancellation,
        ProjectRunEvent::Stage(ProjectRunStage::Booting),
    )
    .await
    {
        return Err(cancelled_error(ProjectRunStage::Booting));
    }
    backend
        .boot(&request.simulator_udid)
        .await
        .map_err(|error| stage_error(ProjectRunStage::Booting, error))?;
    if cancellation.is_cancelled() {
        return Err(cancelled_error(ProjectRunStage::Booting));
    }

    let mut build = match build_runner.start(request.clone()) {
        Ok(handle) => handle,
        Err(error) => return Err(stage_error(ProjectRunStage::Building, error)),
    };
    let mut current_stage = ProjectRunStage::Building;
    let product = loop {
        let event = tokio::select! {
            event = build.recv() => event,
            _ = cancellation.cancelled() => {
                build.cancel();
                return Err(cancelled_error(current_stage));
            }
        };
        let Some(event) = event else {
            return Err(stage_error(
                current_stage,
                SimtopError::new(
                    ErrorCode::Internal,
                    "build event stream ended before Finished",
                ),
            ));
        };
        match event {
            BuildEvent::Stage(stage) => {
                current_stage = map_build_stage(stage);
                if !send_event(sender, cancellation, ProjectRunEvent::Stage(current_stage)).await {
                    build.cancel();
                    return Err(cancelled_error(current_stage));
                }
            }
            BuildEvent::Output(output) => {
                if !send_event(sender, cancellation, ProjectRunEvent::Output(output)).await {
                    build.cancel();
                    return Err(cancelled_error(current_stage));
                }
            }
            BuildEvent::Finished(result) => {
                if cancellation.is_cancelled() {
                    build.cancel();
                    return Err(cancelled_error(current_stage));
                }
                break result.map_err(|error| stage_error(current_stage, error))?;
            }
        }
    };
    // Keep the build handle alive through the event loop only.  Its Drop
    // implementation requests cancellation after the terminal build event.
    drop(build);

    if cancellation.is_cancelled() {
        return Err(cancelled_error(ProjectRunStage::Installing));
    }
    if !send_event(
        sender,
        cancellation,
        ProjectRunEvent::Stage(ProjectRunStage::Installing),
    )
    .await
    {
        return Err(cancelled_error(ProjectRunStage::Installing));
    }
    backend
        .install(&request.simulator_udid, &product.app_path)
        .await
        .map_err(|error| stage_error(ProjectRunStage::Installing, error))?;
    if cancellation.is_cancelled() {
        return Err(cancelled_error(ProjectRunStage::Installing));
    }

    if !send_event(
        sender,
        cancellation,
        ProjectRunEvent::Stage(ProjectRunStage::Launching),
    )
    .await
    {
        return Err(cancelled_error(ProjectRunStage::Launching));
    }
    backend
        .launch(&request.simulator_udid, &product.bundle_id)
        .await
        .map_err(|error| stage_error(ProjectRunStage::Launching, error))?;
    if cancellation.is_cancelled() {
        return Err(cancelled_error(ProjectRunStage::Launching));
    }

    Ok(ProjectRunResult { product })
}

async fn send_event(
    sender: &mpsc::Sender<ProjectRunEvent>,
    cancellation: &RunCancellation,
    event: ProjectRunEvent,
) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    tokio::select! {
        result = sender.send(event) => result.is_ok(),
        _ = cancellation.cancelled() => false,
    }
}

async fn send_finished(
    sender: &mpsc::Sender<ProjectRunEvent>,
    result: Result<ProjectRunResult, SimtopError>,
) -> bool {
    // Unlike progress events, the terminal event must still be delivered
    // after cancellation so consumers can clear their active state.  Dropping
    // the receiver makes this send return immediately when the handle is
    // abandoned, so an explicit cancellation branch is unnecessary.
    sender.send(ProjectRunEvent::Finished(result)).await.is_ok()
}

fn stage_error(stage: ProjectRunStage, error: SimtopError) -> SimtopError {
    let code = error.code();
    SimtopError::with_source(
        code,
        format!("project run {stage:?} failed: {}", error.message()),
        error,
    )
}

fn cancelled_error(stage: ProjectRunStage) -> SimtopError {
    SimtopError::new(
        ErrorCode::Timeout,
        format!("project run cancelled during {stage:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_build_stages_without_leaking_build_types() {
        assert_eq!(
            map_build_stage(BuildStage::Building),
            ProjectRunStage::Building
        );
        assert_eq!(
            map_build_stage(BuildStage::ResolvingProduct),
            ProjectRunStage::ResolvingProduct
        );
    }

    #[test]
    fn stage_context_preserves_error_code() {
        let error = stage_error(
            ProjectRunStage::Installing,
            SimtopError::new(ErrorCode::DeviceNotFound, "missing simulator"),
        );
        assert_eq!(error.code(), ErrorCode::DeviceNotFound);
        assert!(error.message().contains("Installing"));
        assert!(error.message().contains("missing simulator"));
    }
}
