//! Interactive terminal UI for `simtop`.
//!
//! An event-driven, keyboard-only monitoring interface over the shared
//! [`SimulatorBackend`] contract:
//!
//! * **Event-driven refresh** — snapshots are polled on a configurable
//!   interval (default 2s) with no high-frequency redraw loop. Backend calls
//!   are spawned as Tokio tasks and their results are marshalled back over a
//!   bounded channel; refresh/log requests are coalesced with in-flight flags
//!   so at most one of each is outstanding. The screen is redrawn only when
//!   input arrives, state actually changes, or a refresh changed the data.
//! * **Navigation & actions** — device list with selection, searchable
//!   filtering (`/` plus a state filter), and capability-aware actions: the
//!   action set is derived from the selected device's state and availability,
//!   so boot/shutdown/open/screenshot/logs/app operations are only offered
//!   when they can succeed. Every operation result (including errors) is
//!   appended to a bounded activity history.
//! * **Bounded history** — event channel capacity, activity history, and the
//!   followed device-log buffer are all capped; nothing grows unboundedly.
//! * **Terminal hygiene** — raw mode + alternate screen are entered once and
//!   restored on every exit path (normal quit, I/O error, and panic via a
//!   chained panic hook). Small terminals degrade to a reduced layout or an
//!   explicit "too small" notice instead of rendering garbage.
//!
//! Entry points: [`run`] and [`run_with`]. Both consume the backend, manage
//! their own terminal setup, and must be called from within a Tokio runtime.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use tokio::sync::mpsc;

use crate::backend::SimulatorBackend;
use crate::error::{ErrorCode, SimtopError};
use crate::model::{DeviceLog, DeviceSnapshot, DeviceState, SimDevice};

/// Terminal backend used by the whole TUI.
type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Bounded event channel capacity between backend tasks and the UI loop.
const CHANNEL_CAPACITY: usize = 64;
/// Smallest usable terminal; anything smaller gets a "too small" notice.
const MIN_WIDTH: u16 = 52;
const MIN_HEIGHT: u16 = 10;
/// Default bounds for the activity and device-log histories.
const DEFAULT_ACTIVITY_CAP: usize = 200;
const DEFAULT_LOG_CAP: usize = 500;
/// Refresh intervals reachable with `+` / `-` (seconds).
const REFRESH_LADDER_SECS: [u64; 6] = [1, 2, 5, 10, 30, 60];

// List column widths (marker + state + name are always present).
const MARKER_W: u16 = 2;
const STATE_W: u16 = 14; // "shutting down" (13) + padding
const UDID_W: u16 = 37; // 36-char UUID + padding
const RT_W: u16 = 12; // "iOS-18-0" + padding
const OS_W: u16 = 9;
const AVAIL_W: u16 = 7;

// ---------------------------------------------------------------------------
// Theme tokens: a single set of named styles so the whole UI stays coherent.
// ---------------------------------------------------------------------------

const TITLE: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const MUTED: Style = Style::new().fg(Color::DarkGray);
const INFO: Style = Style::new().fg(Color::White);
const OK: Style = Style::new().fg(Color::Green);
const WARN: Style = Style::new().fg(Color::Yellow);
const ERR: Style = Style::new().fg(Color::Red);
const LABEL: Style = Style::new().fg(Color::DarkGray);
const HEADER: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::BOLD);
const BORDER: Style = Style::new().fg(Color::DarkGray);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);
const DISABLED: Style = Style::new().fg(Color::DarkGray);

// ---------------------------------------------------------------------------
// Public configuration and entry points.
// ---------------------------------------------------------------------------

/// Tunables for the TUI session.
#[derive(Clone, Debug)]
pub struct TuiConfig {
    /// Interval between snapshot polls; adjustable at runtime with `+`/`-`.
    pub refresh_interval: Duration,
    /// Bounded activity (operation results/errors) history size.
    pub activity_capacity: usize,
    /// Bounded device-log buffer size shown in the log pane.
    pub log_capacity: usize,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            refresh_interval: Duration::from_secs(2),
            activity_capacity: DEFAULT_ACTIVITY_CAP,
            log_capacity: DEFAULT_LOG_CAP,
        }
    }
}

/// Run the TUI with default configuration until the user quits.
///
/// Consumes the backend, manages its own terminal setup (raw mode, alternate
/// screen), and restores the terminal on every exit path including panics.
/// Must be awaited inside a Tokio runtime.
pub async fn run(backend: Box<dyn SimulatorBackend>) -> Result<(), SimtopError> {
    run_with(backend, TuiConfig::default()).await
}

/// Run the TUI with a custom [`TuiConfig`]; see [`run`].
pub async fn run_with(
    backend: Box<dyn SimulatorBackend>,
    config: TuiConfig,
) -> Result<(), SimtopError> {
    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = App::new(backend, config).run(&mut terminal).await;
    restore_terminal();
    result
}

// ---------------------------------------------------------------------------
// Terminal lifecycle.
// ---------------------------------------------------------------------------

/// Enter raw mode and the alternate screen, hiding the cursor.
fn init_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
        let _ = disable_raw_mode();
        return Err(error);
    }
    stdout.flush()?;
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            restore_terminal();
            Err(error)
        }
    }
}

/// Best-effort restoration of the terminal; safe to call repeatedly.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

/// Chain a hook that restores the terminal if the UI panics mid-session.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

// ---------------------------------------------------------------------------
// Internal event plumbing.
// ---------------------------------------------------------------------------

/// Events flowing from backend tasks back to the UI loop over the bounded
/// channel. Keyboard input is handled directly and never queued here.
enum Event {
    Snapshot(Result<DeviceSnapshot, SimtopError>),
    Logs(Result<DeviceLog, SimtopError>),
    Action(CompletedAction),
}

/// A user-triggered backend operation together with its outcome.
struct CompletedAction {
    action: Action,
    result: Result<(), SimtopError>,
    elapsed: Duration,
}

/// One typed operation the UI can dispatch against the backend.
enum Action {
    Boot { udid: String },
    Shutdown { udid: String },
    Open { udid: String },
    Delete { udid: String },
    Screenshot { udid: String, path: PathBuf },
    Install { udid: String, path: PathBuf },
    Launch { udid: String, bundle_id: String },
    Terminate { udid: String, bundle_id: String },
    Uninstall { udid: String, bundle_id: String },
    OpenUrl { udid: String, url: String },
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Action::Boot { .. } => "boot",
            Action::Shutdown { .. } => "shutdown",
            Action::Open { .. } => "open",
            Action::Delete { .. } => "delete",
            Action::Screenshot { .. } => "screenshot",
            Action::Install { .. } => "install",
            Action::Launch { .. } => "launch",
            Action::Terminate { .. } => "terminate",
            Action::Uninstall { .. } => "uninstall",
            Action::OpenUrl { .. } => "open url",
        }
    }

    fn udid(&self) -> &str {
        match self {
            Action::Boot { udid }
            | Action::Shutdown { udid }
            | Action::Open { udid }
            | Action::Delete { udid }
            | Action::Screenshot { udid, .. }
            | Action::Install { udid, .. }
            | Action::Launch { udid, .. }
            | Action::Terminate { udid, .. }
            | Action::Uninstall { udid, .. }
            | Action::OpenUrl { udid, .. } => udid,
        }
    }

    fn describe(&self) -> String {
        format!("{} {}", self.label(), short_udid(self.udid()))
    }

    async fn run(&self, backend: &dyn SimulatorBackend) -> Result<(), SimtopError> {
        match self {
            Action::Boot { udid } => backend.boot(udid).await,
            Action::Shutdown { udid } => backend.shutdown(udid).await,
            Action::Open { udid } => backend.open(udid).await,
            Action::Delete { udid } => backend.delete(udid).await,
            Action::Screenshot { udid, path } => backend.screenshot(udid, path).await,
            Action::Install { udid, path } => backend.install(udid, path).await,
            Action::Launch { udid, bundle_id } => backend.launch(udid, bundle_id).await.map(|_| ()),
            Action::Terminate { udid, bundle_id } => backend.terminate(udid, bundle_id).await,
            Action::Uninstall { udid, bundle_id } => backend.uninstall(udid, bundle_id).await,
            Action::OpenUrl { udid, url } => backend.open_url(udid, url).await,
        }
    }
}

/// The capability-aware action categories; which ones are enabled for a
/// device depends on its state and availability.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Boot,
    Shutdown,
    Open,
    Screenshot,
    Logs,
    Delete,
    Install,
    Launch,
    Terminate,
    Uninstall,
    OpenUrl,
}

impl ActionKind {
    fn label(self) -> &'static str {
        match self {
            ActionKind::Boot => "boot",
            ActionKind::Shutdown => "shutdown",
            ActionKind::Open => "open Simulator.app",
            ActionKind::Screenshot => "screenshot",
            ActionKind::Logs => "follow logs",
            ActionKind::Delete => "delete",
            ActionKind::Install => "install app",
            ActionKind::Launch => "launch app",
            ActionKind::Terminate => "terminate app",
            ActionKind::Uninstall => "uninstall app",
            ActionKind::OpenUrl => "open url",
        }
    }

    fn key(self) -> char {
        match self {
            ActionKind::Boot => 'b',
            ActionKind::Shutdown => 's',
            ActionKind::Open => 'o',
            ActionKind::Screenshot => 'p',
            ActionKind::Logs => 'l',
            ActionKind::Delete => 'd',
            ActionKind::Install => 'i',
            ActionKind::Launch => 'a',
            ActionKind::Terminate => 't',
            ActionKind::Uninstall => 'u',
            ActionKind::OpenUrl => 'w',
        }
    }
}

/// Order in which actions are listed in the details pane.
const ACTION_ORDER: [ActionKind; 11] = [
    ActionKind::Boot,
    ActionKind::Open,
    ActionKind::Shutdown,
    ActionKind::Screenshot,
    ActionKind::Logs,
    ActionKind::Install,
    ActionKind::Launch,
    ActionKind::Terminate,
    ActionKind::Uninstall,
    ActionKind::OpenUrl,
    ActionKind::Delete,
];

/// Actions that make sense for a device in its current state. Unavailable
/// devices can only be deleted; transitional states accept nothing.
fn enabled_actions(device: &SimDevice) -> Vec<ActionKind> {
    let mut actions = Vec::new();
    if device.is_available {
        match device.state {
            DeviceState::Shutdown | DeviceState::Unknown(_) => {
                actions.push(ActionKind::Boot);
                actions.push(ActionKind::Open);
            }
            DeviceState::Booted => {
                actions.extend([
                    ActionKind::Shutdown,
                    ActionKind::Open,
                    ActionKind::Screenshot,
                    ActionKind::Logs,
                    ActionKind::Install,
                    ActionKind::Launch,
                    ActionKind::Terminate,
                    ActionKind::Uninstall,
                    ActionKind::OpenUrl,
                ]);
            }
            DeviceState::Booting | DeviceState::ShuttingDown | DeviceState::Creating => {}
        }
    }
    actions.push(ActionKind::Delete);
    actions
}

/// Input modes: normal navigation, or a single-line edit for search/prompts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Search,
    InstallPath,
    LaunchBundle,
    TerminateBundle,
    UninstallBundle,
    OpenUrl,
    ScreenshotPath,
}

impl EditKind {
    fn title(self) -> &'static str {
        match self {
            EditKind::Search => "search",
            EditKind::InstallPath => "install path",
            EditKind::LaunchBundle => "launch bundle id",
            EditKind::TerminateBundle => "terminate bundle id",
            EditKind::UninstallBundle => "uninstall bundle id",
            EditKind::OpenUrl => "url",
            EditKind::ScreenshotPath => "screenshot path",
        }
    }
}

enum InputMode {
    Normal,
    Edit {
        kind: EditKind,
        text: String,
        cursor: usize,
    },
}

/// Which pane receives navigation/scrolling keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Logs,
}

/// State filter cycled with `f`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    All,
    Booted,
    Shutdown,
}

impl StateFilter {
    fn label(self) -> &'static str {
        match self {
            StateFilter::All => "all",
            StateFilter::Booted => "booted",
            StateFilter::Shutdown => "shutdown",
        }
    }

    fn matches(self, device: &SimDevice) -> bool {
        match self {
            StateFilter::All => true,
            StateFilter::Booted => device.state.is_booted(),
            StateFilter::Shutdown => matches!(device.state, DeviceState::Shutdown),
        }
    }

    fn next(self) -> StateFilter {
        match self {
            StateFilter::All => StateFilter::Booted,
            StateFilter::Booted => StateFilter::Shutdown,
            StateFilter::Shutdown => StateFilter::All,
        }
    }
}

/// A line in the bounded activity history.
enum ActivityKind {
    Info,
    Ok,
    Warn,
    Err,
}

struct ActivityLine {
    time: String,
    kind: ActivityKind,
    text: String,
}

impl ActivityLine {
    fn new(kind: ActivityKind, text: String) -> Self {
        ActivityLine {
            time: utc_time(),
            kind,
            text,
        }
    }

    fn info(text: String) -> Self {
        ActivityLine::new(ActivityKind::Info, text)
    }

    fn ok(text: String) -> Self {
        ActivityLine::new(ActivityKind::Ok, text)
    }

    fn warn(text: impl Into<String>) -> Self {
        ActivityLine::new(ActivityKind::Warn, text.into())
    }

    fn err(text: String) -> Self {
        ActivityLine::new(ActivityKind::Err, text)
    }

    fn style(&self) -> Style {
        match self.kind {
            ActivityKind::Info => INFO,
            ActivityKind::Ok => OK,
            ActivityKind::Warn => WARN,
            ActivityKind::Err => ERR,
        }
    }
}

/// A line in the bounded device-log buffer.
struct LogLine {
    time: String,
    process: String,
    message: String,
}

// ---------------------------------------------------------------------------
// The application state machine.
// ---------------------------------------------------------------------------

struct App {
    backend: Arc<dyn SimulatorBackend>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    config: TuiConfig,

    devices: Vec<SimDevice>,
    generation: u64,
    snapshot_time: String,
    filtered: Vec<usize>,
    selected: Option<usize>,
    selected_udid: Option<String>,
    list_scroll: usize,
    filter: String,
    state_filter: StateFilter,

    refresh_in_flight: bool,
    logs_in_flight: bool,
    last_refresh: Instant,
    last_snapshot_error: Option<String>,

    follow_udid: Option<String>,
    logs: VecDeque<LogLine>,
    log_scroll: usize,
    logs_visible: bool,

    activity: VecDeque<ActivityLine>,
    mode: InputMode,
    focus: Focus,
    confirm_delete: Option<String>,
    show_help: bool,
    dirty: bool,
    quit: bool,
}

impl App {
    fn new(backend: Box<dyn SimulatorBackend>, config: TuiConfig) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let config = TuiConfig {
            activity_capacity: config.activity_capacity.max(1),
            log_capacity: config.log_capacity.max(1),
            ..config
        };
        App {
            backend: Arc::from(backend),
            tx,
            rx,
            config,
            devices: Vec::new(),
            generation: 0,
            snapshot_time: String::new(),
            filtered: Vec::new(),
            selected: None,
            selected_udid: None,
            list_scroll: 0,
            filter: String::new(),
            state_filter: StateFilter::All,
            refresh_in_flight: false,
            logs_in_flight: false,
            last_refresh: Instant::now(),
            last_snapshot_error: None,
            follow_udid: None,
            logs: VecDeque::new(),
            log_scroll: 0,
            logs_visible: false,
            activity: VecDeque::new(),
            mode: InputMode::Normal,
            focus: Focus::List,
            confirm_delete: None,
            show_help: false,
            dirty: true,
            quit: false,
        }
    }

    /// The main loop: drain channel events, poll keyboard, tick refresh,
    /// redraw only when something changed.
    async fn run(mut self, terminal: &mut Term) -> Result<(), SimtopError> {
        self.spawn_refresh();
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(event) => self.handle_event(event),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        return Err(SimtopError::new(
                            ErrorCode::Internal,
                            "ui event channel closed",
                        ));
                    }
                }
            }
            if self.quit {
                break;
            }
            if event::poll(self.poll_duration()).map_err(SimtopError::from)? {
                match event::read().map_err(SimtopError::from)? {
                    TermEvent::Key(key) => self.handle_key(key),
                    TermEvent::Resize(_, _) => self.dirty = true,
                    _ => {}
                }
            }
            self.maybe_tick();
            if self.dirty {
                self.draw(terminal).map_err(SimtopError::from)?;
                self.dirty = false;
            }
        }
        Ok(())
    }

    /// Input latency bound, clamped so a refresh can start the moment it is
    /// due. Returns zero while the refresh deadline is already past.
    fn poll_duration(&self) -> Duration {
        let until_refresh = self
            .config
            .refresh_interval
            .saturating_sub(self.last_refresh.elapsed());
        until_refresh.min(Duration::from_millis(100))
    }

    /// Fire refresh/log polls when due; both are coalesced by in-flight flags.
    fn maybe_tick(&mut self) {
        if self.quit {
            return;
        }
        let due = self.last_refresh.elapsed() >= self.config.refresh_interval;
        if due && !self.refresh_in_flight {
            self.spawn_refresh();
        }
        if due && !self.logs_in_flight {
            if let Some(udid) = self.follow_udid.clone() {
                self.spawn_logs(&udid);
            }
        }
    }

    // -- backend task spawning ---------------------------------------------

    /// Poll a fresh snapshot off the event loop. Coalesced: while one request
    /// is in flight, further calls are no-ops.
    fn spawn_refresh(&mut self) {
        if self.refresh_in_flight {
            return;
        }
        self.refresh_in_flight = true;
        let backend = Arc::clone(&self.backend);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = backend.snapshot().await;
            let _ = tx.send(Event::Snapshot(result)).await;
        });
    }

    /// Fetch recent device logs off the event loop; same coalescing rule.
    fn spawn_logs(&mut self, udid: &str) {
        if self.logs_in_flight {
            return;
        }
        self.logs_in_flight = true;
        let backend = Arc::clone(&self.backend);
        let tx = self.tx.clone();
        let udid = udid.to_string();
        tokio::spawn(async move {
            let result = backend.logs(&udid).await;
            let _ = tx.send(Event::Logs(result)).await;
        });
    }

    /// Run a user-triggered operation on a worker task so the UI loop never
    /// blocks on backend work; the outcome comes back through the channel.
    fn spawn_action(&mut self, action: Action) {
        self.push_activity(ActivityLine::info(format!("{} ...", action.describe())));
        let backend = Arc::clone(&self.backend);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let result = action.run(backend.as_ref()).await;
            let _ = tx
                .send(Event::Action(CompletedAction {
                    action,
                    result,
                    elapsed: start.elapsed(),
                }))
                .await;
        });
    }

    // -- event handling -----------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Snapshot(result) => {
                self.refresh_in_flight = false;
                self.last_refresh = Instant::now();
                match result {
                    Ok(snapshot) => {
                        self.last_snapshot_error = None;
                        self.apply_snapshot(snapshot);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        self.last_snapshot_error = Some(message.clone());
                        self.push_activity(ActivityLine::err(format!(
                            "snapshot failed: {message}"
                        )));
                    }
                }
            }
            Event::Logs(result) => {
                self.logs_in_flight = false;
                match result {
                    Ok(log) => self.apply_logs(log),
                    Err(error) => {
                        self.follow_udid = None;
                        self.push_activity(ActivityLine::err(format!(
                            "log follow stopped: {error}"
                        )));
                    }
                }
            }
            Event::Action(done) => self.handle_action_done(done),
        }
    }

    fn apply_snapshot(&mut self, snapshot: DeviceSnapshot) {
        let devices_changed = fingerprint(&self.devices) != fingerprint(&snapshot.devices);
        let meta_changed =
            snapshot.generation != self.generation || snapshot.timestamp != self.snapshot_time;
        self.generation = snapshot.generation;
        self.snapshot_time = snapshot.timestamp;
        self.devices = snapshot.devices;
        self.recompute_filter();
        if devices_changed || meta_changed {
            self.dirty = true;
        }
    }

    fn apply_logs(&mut self, log: DeviceLog) {
        let capacity = self.config.log_capacity;
        self.logs.clear();
        self.logs.extend(
            log.entries
                .iter()
                .skip(log.entries.len().saturating_sub(capacity))
                .map(|entry| LogLine {
                    time: entry.timestamp.clone(),
                    process: entry.process.clone(),
                    message: entry.message.clone(),
                }),
        );
        if self.log_scroll > self.logs.len() {
            self.log_scroll = self.logs.len();
        }
        self.dirty = true;
    }

    fn handle_action_done(&mut self, done: CompletedAction) {
        match &done.result {
            Ok(()) => {
                self.push_activity(ActivityLine::ok(format!(
                    "{} ok ({}ms)",
                    done.action.describe(),
                    done.elapsed.as_millis()
                )));
                if let Action::Delete { udid } = &done.action {
                    self.devices.retain(|device| &device.udid != udid);
                    self.recompute_filter();
                } else {
                    // Lifecycle changes show up immediately on the next poll.
                    self.spawn_refresh();
                }
            }
            Err(error) => {
                self.push_activity(ActivityLine::err(format!(
                    "{} failed: {error}",
                    done.action.describe()
                )));
            }
        }
        self.dirty = true;
    }

    // -- selection & filtering ---------------------------------------------

    fn selected_device(&self) -> Option<&SimDevice> {
        self.selected
            .and_then(|index| self.filtered.get(index))
            .map(|&device_index| &self.devices[device_index])
    }

    fn recompute_filter(&mut self) {
        let query = self.filter.clone().to_lowercase();
        self.filtered = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, device)| self.state_filter.matches(device))
            .filter(|(_, device)| {
                query.is_empty()
                    || device.name.to_lowercase().contains(&query)
                    || device.udid.to_lowercase().contains(&query)
                    || device.device_type.to_lowercase().contains(&query)
                    || device.runtime.to_lowercase().contains(&query)
                    || device.os_version.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        let previous_udid = self.selected_udid.clone();
        let previous_index = self.selected;
        self.selected = previous_udid
            .as_deref()
            .and_then(|udid| {
                self.filtered
                    .iter()
                    .position(|&index| self.devices[index].udid == udid)
            })
            .or_else(|| {
                if self.filtered.is_empty() {
                    None
                } else {
                    Some(previous_index.unwrap_or(0).min(self.filtered.len() - 1))
                }
            });
        if let Some(index) = self.selected {
            let device_index = self.filtered[index];
            self.selected_udid = Some(self.devices[device_index].udid.clone());
        }
        self.dirty = true;
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered.len();
        if count == 0 {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = current.saturating_add(delta).clamp(0, count as isize - 1) as usize;
        self.selected = Some(next);
        let device_index = self.filtered[next];
        self.selected_udid = Some(self.devices[device_index].udid.clone());
    }

    // -- key handling -------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        if matches!(self.mode, InputMode::Edit { .. }) {
            self.handle_edit_key(key);
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Esc => {
                    self.show_help = false;
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('c') {
                self.quit = true;
            }
            return;
        }
        // A pending delete is confirmed only by an explicit y/Y. Every other
        // key means no; Enter therefore keeps the safe default.
        if let Some(udid) = self.confirm_delete.take() {
            if delete_confirmation_accepts(key.code) {
                self.spawn_action(Action::Delete { udid });
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('/') => self.start_edit(EditKind::Search, self.filter.clone()),
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.state_filter = self.state_filter.next();
                self.recompute_filter();
            }
            KeyCode::Char('r') | KeyCode::Char('R') => self.spawn_refresh(),
            KeyCode::Char('b') => self.dispatch(ActionKind::Boot),
            KeyCode::Char('s') => self.dispatch(ActionKind::Shutdown),
            KeyCode::Char('o') => self.dispatch(ActionKind::Open),
            KeyCode::Char('p') => self.dispatch(ActionKind::Screenshot),
            KeyCode::Char('l') => self.toggle_logs(),
            KeyCode::Char('i') => self.dispatch(ActionKind::Install),
            KeyCode::Char('a') => self.dispatch(ActionKind::Launch),
            KeyCode::Char('t') => self.dispatch(ActionKind::Terminate),
            KeyCode::Char('u') => self.dispatch(ActionKind::Uninstall),
            KeyCode::Char('w') => self.dispatch(ActionKind::OpenUrl),
            KeyCode::Char('d') => self.request_delete(),
            KeyCode::Char('g') => self.move_selection(isize::MIN),
            KeyCode::Char('G') => self.move_selection(isize::MAX),
            KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust_interval(true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.adjust_interval(false),
            KeyCode::Enter => self.primary_action(),
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                if self.focus == Focus::List && self.logs_visible {
                    self.focus = Focus::Logs;
                } else {
                    self.focus = Focus::List;
                }
            }
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down => self.move_focus(1),
            KeyCode::PageUp => self.page_focus(-10),
            KeyCode::PageDown => self.page_focus(10),
            KeyCode::Home => self.home_focus(),
            KeyCode::End => self.end_focus(),
            _ => {}
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('c') {
                self.quit = true;
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel_edit(),
            KeyCode::Enter => {
                if let InputMode::Edit { kind, text, .. } = &self.mode {
                    let kind = *kind;
                    let text = text.clone();
                    self.mode = InputMode::Normal;
                    self.submit_edit(kind, text);
                }
            }
            KeyCode::Left => self.edit_move_cursor(-1),
            KeyCode::Right => self.edit_move_cursor(1),
            KeyCode::Home => self.edit_set_cursor(0),
            KeyCode::End => self.edit_set_cursor(usize::MAX),
            KeyCode::Backspace => self.edit_backspace(),
            KeyCode::Delete => self.edit_delete(),
            KeyCode::Char(c) => self.edit_insert(c),
            _ => {}
        }
    }

    fn start_edit(&mut self, kind: EditKind, prefill: String) {
        let cursor = prefill.chars().count();
        self.mode = InputMode::Edit {
            kind,
            text: prefill,
            cursor,
        };
    }

    fn cancel_edit(&mut self) {
        if matches!(
            self.mode,
            InputMode::Edit {
                kind: EditKind::Search,
                ..
            }
        ) {
            self.filter.clear();
            self.recompute_filter();
        }
        self.mode = InputMode::Normal;
    }

    fn submit_edit(&mut self, kind: EditKind, text: String) {
        match kind {
            EditKind::Search => {
                self.filter = text;
                self.recompute_filter();
            }
            EditKind::InstallPath => self.spawn_typed(ActionKind::Install, text, |udid, value| {
                Some(Action::Install {
                    udid,
                    path: PathBuf::from(value),
                })
            }),
            EditKind::ScreenshotPath => {
                self.spawn_typed(ActionKind::Screenshot, text, |udid, value| {
                    Some(Action::Screenshot {
                        udid,
                        path: PathBuf::from(value),
                    })
                })
            }
            EditKind::LaunchBundle => {
                self.spawn_typed(ActionKind::Launch, text, |udid, bundle_id| {
                    Some(Action::Launch { udid, bundle_id })
                })
            }
            EditKind::TerminateBundle => {
                self.spawn_typed(ActionKind::Terminate, text, |udid, bundle_id| {
                    Some(Action::Terminate { udid, bundle_id })
                })
            }
            EditKind::UninstallBundle => {
                self.spawn_typed(ActionKind::Uninstall, text, |udid, bundle_id| {
                    Some(Action::Uninstall { udid, bundle_id })
                })
            }
            EditKind::OpenUrl => self.spawn_typed(ActionKind::OpenUrl, text, |udid, url| {
                Some(Action::OpenUrl { udid, url })
            }),
        }
    }

    /// Validate a prompt-supplied value against the selected device, then
    /// dispatch. Value validation errors surface in the activity history.
    fn spawn_typed(
        &mut self,
        kind: ActionKind,
        value: String,
        make: impl FnOnce(String, String) -> Option<Action>,
    ) {
        let value = value.trim().to_string();
        if value.is_empty() {
            self.push_activity(ActivityLine::warn(format!(
                "{} needs a value",
                kind.label()
            )));
            return;
        }
        let Some(device) = self.selected_device() else {
            self.push_activity(ActivityLine::warn("no device selected"));
            return;
        };
        if !enabled_actions(device).contains(&kind) {
            self.push_activity(ActivityLine::warn(format!(
                "{} not available while device is {}",
                kind.label(),
                state_short(&device.state)
            )));
            return;
        }
        let udid = device.udid.clone();
        if let Some(action) = make(udid, value) {
            self.spawn_action(action);
        }
    }

    fn edit_move_cursor(&mut self, delta: isize) {
        if let InputMode::Edit { cursor, text, .. } = &mut self.mode {
            let len = text.chars().count() as isize;
            *cursor = ((*cursor as isize).saturating_add(delta).clamp(0, len)) as usize;
        }
    }

    fn edit_set_cursor(&mut self, position: usize) {
        if let InputMode::Edit { cursor, text, .. } = &mut self.mode {
            *cursor = position.min(text.chars().count());
        }
    }

    fn edit_insert(&mut self, c: char) {
        let mut is_search = false;
        if let InputMode::Edit { kind, text, cursor } = &mut self.mode {
            is_search = *kind == EditKind::Search;
            let byte = char_byte_index(text, *cursor);
            text.insert(byte, c);
            *cursor += 1;
        }
        if is_search {
            self.sync_search_filter();
        }
    }

    fn edit_backspace(&mut self) {
        let mut is_search = false;
        if let InputMode::Edit { kind, text, cursor } = &mut self.mode {
            is_search = *kind == EditKind::Search;
            if *cursor > 0 {
                let byte = char_byte_index(text, *cursor);
                let start = text[..byte]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                text.replace_range(start..byte, "");
                *cursor -= 1;
            }
        }
        if is_search {
            self.sync_search_filter();
        }
    }

    fn edit_delete(&mut self) {
        let mut is_search = false;
        if let InputMode::Edit { kind, text, cursor } = &mut self.mode {
            is_search = *kind == EditKind::Search;
            let byte = char_byte_index(text, *cursor);
            if byte < text.len() {
                let end = text[byte..]
                    .char_indices()
                    .nth(1)
                    .map(|(index, _)| byte + index)
                    .unwrap_or(text.len());
                text.replace_range(byte..end, "");
            }
        }
        if is_search {
            self.sync_search_filter();
        }
    }

    /// Search edits filter the list live; keep the filter in sync with the
    /// edit buffer.
    fn sync_search_filter(&mut self) {
        if let InputMode::Edit { text, .. } = &self.mode {
            self.filter = text.clone();
        }
        self.recompute_filter();
    }

    /// Enter: boot a shutdown device, open a booted one.
    fn primary_action(&mut self) {
        let Some(device) = self.selected_device() else {
            self.push_activity(ActivityLine::warn("no device selected"));
            return;
        };
        if !device.is_available {
            self.push_activity(ActivityLine::warn(format!(
                "{} is not available",
                device.name
            )));
            return;
        }
        match &device.state {
            DeviceState::Shutdown | DeviceState::Unknown(_) => {
                self.spawn_action(Action::Boot {
                    udid: device.udid.clone(),
                });
            }
            DeviceState::Booted => {
                self.spawn_action(Action::Open {
                    udid: device.udid.clone(),
                });
            }
            other => self.push_activity(ActivityLine::warn(format!(
                "no primary action while device is {}",
                state_short(other)
            ))),
        }
    }

    /// Dispatch a key-bound action after checking the device's capabilities.
    fn dispatch(&mut self, kind: ActionKind) {
        let Some(device) = self.selected_device() else {
            self.push_activity(ActivityLine::warn("no device selected"));
            return;
        };
        if !enabled_actions(device).contains(&kind) {
            self.push_activity(ActivityLine::warn(format!(
                "{} not available while device is {}",
                kind.label(),
                state_short(&device.state)
            )));
            return;
        }
        let udid = device.udid.clone();
        match kind {
            ActionKind::Boot => self.spawn_action(Action::Boot { udid }),
            ActionKind::Shutdown => self.spawn_action(Action::Shutdown { udid }),
            ActionKind::Open => self.spawn_action(Action::Open { udid }),
            ActionKind::Screenshot => {
                self.start_edit(EditKind::ScreenshotPath, screenshot_default_path(&udid));
            }
            ActionKind::Logs => self.toggle_logs(),
            ActionKind::Delete => self.request_delete(),
            ActionKind::Install => self.start_edit(EditKind::InstallPath, String::new()),
            ActionKind::Launch => self.start_edit(EditKind::LaunchBundle, String::new()),
            ActionKind::Terminate => self.start_edit(EditKind::TerminateBundle, String::new()),
            ActionKind::Uninstall => self.start_edit(EditKind::UninstallBundle, String::new()),
            ActionKind::OpenUrl => self.start_edit(EditKind::OpenUrl, String::new()),
        }
    }

    /// Toggle log following for the selected device. Follow survives
    /// selection changes until toggled off or the backend errors out.
    fn toggle_logs(&mut self) {
        let Some(device) = self.selected_device() else {
            self.push_activity(ActivityLine::warn("no device selected"));
            return;
        };
        if !enabled_actions(device).contains(&ActionKind::Logs) {
            self.push_activity(ActivityLine::warn(format!(
                "logs not available while device is {}",
                state_short(&device.state)
            )));
            return;
        }
        let udid = device.udid.clone();
        let name = device.name.clone();
        if self.follow_udid.as_deref() == Some(udid.as_str()) {
            self.follow_udid = None;
            self.push_activity(ActivityLine::info(format!(
                "log follow stopped for {}",
                short_udid(&udid)
            )));
        } else {
            self.follow_udid = Some(udid.clone());
            self.push_activity(ActivityLine::info(format!(
                "following logs for {} ({})",
                name,
                short_udid(&udid)
            )));
            self.spawn_logs(&udid);
        }
    }

    /// Destructive delete requires an explicit yes; every other response
    /// cancels, including Enter.
    fn request_delete(&mut self) {
        let Some(device) = self.selected_device() else {
            self.push_activity(ActivityLine::warn("no device selected"));
            return;
        };
        let udid = device.udid.clone();
        let name = device.name.clone();
        self.confirm_delete = Some(udid.clone());
        self.push_activity(ActivityLine::warn(format!(
            "delete {} ({}): y/N",
            name,
            short_udid(&udid)
        )));
    }

    fn adjust_interval(&mut self, faster: bool) {
        let current = self.config.refresh_interval.as_secs();
        let index = REFRESH_LADDER_SECS
            .iter()
            .position(|&s| s == current)
            .unwrap_or(1);
        let next = if faster {
            index.saturating_sub(1)
        } else {
            (index + 1).min(REFRESH_LADDER_SECS.len() - 1)
        };
        if next != index {
            self.config.refresh_interval = Duration::from_secs(REFRESH_LADDER_SECS[next]);
            self.push_activity(ActivityLine::info(format!(
                "refresh interval {}s",
                REFRESH_LADDER_SECS[next]
            )));
        }
    }

    fn move_focus(&mut self, delta: isize) {
        if self.focus == Focus::List {
            self.move_selection(delta);
        } else {
            self.scroll_logs(delta);
        }
    }

    fn page_focus(&mut self, delta: isize) {
        if self.focus == Focus::List {
            self.move_selection(delta * 10);
        } else {
            self.scroll_logs(delta * 10);
        }
    }

    fn home_focus(&mut self) {
        if self.focus == Focus::List {
            self.move_selection(isize::MIN);
        } else {
            self.log_scroll = usize::MAX;
        }
    }

    fn end_focus(&mut self) {
        if self.focus == Focus::List {
            self.move_selection(isize::MAX);
        } else {
            self.log_scroll = 0;
        }
    }

    fn scroll_logs(&mut self, delta: isize) {
        let current = self.log_scroll as isize;
        self.log_scroll = current.saturating_add(delta).max(0) as usize;
    }

    fn push_activity(&mut self, line: ActivityLine) {
        if self.activity.len() >= self.config.activity_capacity {
            self.activity.pop_front();
        }
        self.activity.push_back(line);
        self.dirty = true;
    }

    // -- rendering ----------------------------------------------------------

    fn draw(&mut self, terminal: &mut Term) -> io::Result<()> {
        terminal.draw(|frame| {
            let area = frame.area();
            if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
                render_too_small(frame, area);
                return;
            }
            let rects = compute_layout(area);
            self.logs_visible = rects.logs.is_some();
            self.render_top(frame, rects.top);
            self.render_list(frame, rects.list);
            if let Some(rect) = rects.details {
                self.render_details(frame, rect);
            }
            if let Some(rect) = rects.logs {
                self.render_logs(frame, rect);
            }
            self.render_activity(frame, rects.activity);
            self.render_status(frame, rects.status);
            if self.show_help {
                render_help(frame, area);
            }
            if let InputMode::Edit {
                kind,
                text: _,
                cursor,
            } = &self.mode
            {
                let prefix = kind.title().len() + 2; // "title: "
                let x = rects.status.x + prefix as u16 + *cursor as u16;
                let x = x.min(rects.status.x + rects.status.width.saturating_sub(1));
                frame.set_cursor_position(Position::new(x, rects.status.y));
            }
        })?;
        Ok(())
    }

    fn render_top(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Block::new().borders(Borders::BOTTOM).border_style(BORDER),
            area,
        );
        let left = Line::from(vec![
            Span::styled("simtop", TITLE),
            Span::styled("  -  iOS simulator monitor", MUTED),
        ]);
        let follow = self
            .follow_udid
            .as_deref()
            .map(short_udid)
            .unwrap_or_else(|| "off".to_string());
        let meta = format!(
            "gen {} | {} devices | refresh {}s | follow {} | as of {}",
            self.generation,
            self.devices.len(),
            self.config.refresh_interval.as_secs(),
            follow,
            self.snapshot_time,
        );
        let meta_width = meta.chars().count() as u16;
        let left_width = if meta_width + 4 < area.width {
            area.width - meta_width - 2
        } else {
            area.width
        };
        frame.render_widget(
            Paragraph::new(left),
            Rect::new(area.x, area.y, left_width, 1),
        );
        if left_width < area.width {
            put(
                frame.buffer_mut(),
                area.x + left_width + 2,
                area.y,
                area.width - left_width - 2,
                &meta,
                MUTED,
            );
        }
    }

    fn list_title(&self) -> String {
        let marker = if self.focus == Focus::List { ">" } else { " " };
        let mut title = format!(
            "{marker} devices: {}/{}",
            self.filtered.len(),
            self.devices.len()
        );
        if !self.filter.is_empty() {
            title.push_str(&format!("  filter: {}", self.filter));
        }
        if self.state_filter != StateFilter::All {
            title.push_str(&format!("  state: {}", self.state_filter.label()));
        }
        if self.last_snapshot_error.is_some() {
            title.push_str("  [!] snapshot error");
        }
        title
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let body = pane_head(frame, area, &self.list_title(), self.focus == Focus::List);
        let width = body.width;
        let show_udid = width >= 58;
        let show_rt = width >= 74;
        let show_os = width >= 90;
        let show_avail = width >= 104;

        let fixed = MARKER_W
            + STATE_W
            + if show_udid { UDID_W } else { 0 }
            + if show_rt { RT_W } else { 0 }
            + if show_os { OS_W } else { 0 }
            + if show_avail { AVAIL_W } else { 0 };
        let name_width = width.saturating_sub(fixed).max(4);

        let mut col = body.x + MARKER_W;
        let state_x = col;
        col += STATE_W;
        let name_x = col;
        col += name_width;
        let udid_x = col;
        let rt_x = udid_x + if show_udid { UDID_W } else { 0 };
        let os_x = rt_x + if show_rt { RT_W } else { 0 };
        let avail_x = os_x + if show_os { OS_W } else { 0 };

        let count = self.filtered.len();
        if count == 0 {
            let message = if self.devices.is_empty() {
                match &self.last_snapshot_error {
                    Some(error) => format!("snapshot failed: {error}"),
                    None => "no simulators found".to_string(),
                }
            } else {
                "no devices match the current filter".to_string()
            };
            let message_width = message.chars().count() as u16;
            let x = (body.x + body.width.saturating_sub(message_width) / 2).max(body.x);
            let y = body.y + body.height / 2;
            put(frame.buffer_mut(), x, y, body.width, &message, MUTED);
            return;
        }

        // Keep the selected row visible.
        let mut first = self.list_scroll.min(count - 1);
        if let Some(selected) = self.selected {
            if selected < first {
                first = selected;
            }
            if selected >= first + body.height as usize {
                first = selected - body.height as usize + 1;
            }
        }
        self.list_scroll = first;

        let buf = frame.buffer_mut();
        // Header row.
        put(buf, state_x, body.y, STATE_W - 1, "STATE", HEADER);
        put(buf, name_x, body.y, name_width - 1, "NAME", HEADER);
        if show_udid {
            put(buf, udid_x, body.y, UDID_W - 1, "UDID", HEADER);
        }
        if show_rt {
            put(buf, rt_x, body.y, RT_W - 1, "RUNTIME", HEADER);
        }
        if show_os {
            put(buf, os_x, body.y, OS_W - 1, "OS", HEADER);
        }
        if show_avail {
            put(buf, avail_x, body.y, AVAIL_W - 1, "AVAIL", HEADER);
        }

        for row in 0..body.height {
            let index = first + row as usize;
            if index >= count {
                break;
            }
            let device = &self.devices[self.filtered[index]];
            let y = body.y + row;
            let selected = self.selected == Some(index);
            let row_style = if selected { SELECTED } else { Style::default() };
            if selected {
                buf.set_style(Rect::new(body.x, y, body.width, 1), SELECTED);
            }
            put(
                buf,
                body.x,
                y,
                MARKER_W,
                if selected { ">" } else { " " },
                row_style,
            );
            put(
                buf,
                state_x,
                y,
                STATE_W - 1,
                state_short(&device.state),
                state_style(&device.state).patch(row_style),
            );
            let name_style = if device.is_available { INFO } else { MUTED };
            put(
                buf,
                name_x,
                y,
                name_width - 1,
                &device.name,
                name_style.patch(row_style),
            );
            if show_udid {
                put(
                    buf,
                    udid_x,
                    y,
                    UDID_W - 1,
                    &device.udid,
                    MUTED.patch(row_style),
                );
            }
            if show_rt {
                let runtime = device.runtime.rsplit('.').next().unwrap_or(&device.runtime);
                put(buf, rt_x, y, RT_W - 1, runtime, MUTED.patch(row_style));
            }
            if show_os {
                put(
                    buf,
                    os_x,
                    y,
                    OS_W - 1,
                    &device.os_version,
                    INFO.patch(row_style),
                );
            }
            if show_avail {
                let (text, style) = if device.is_available {
                    ("yes", OK)
                } else {
                    ("no", ERR)
                };
                put(buf, avail_x, y, AVAIL_W - 1, text, style.patch(row_style));
            }
        }
    }

    fn details_title(&self) -> String {
        match self.selected_device() {
            Some(device) => format!(" details: {}", device.name),
            None => " details".to_string(),
        }
    }

    fn render_details(&self, frame: &mut Frame, area: Rect) {
        let body = pane_head(frame, area, &self.details_title(), false);
        let mut lines: Vec<Line> = Vec::new();
        match self.selected_device() {
            Some(device) => {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("udid", LABEL),
                    Span::raw("  "),
                    Span::raw(device.udid.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("name", LABEL),
                    Span::raw("  "),
                    Span::raw(device.name.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("state", LABEL),
                    Span::raw("  "),
                    Span::styled(device.state.to_string(), state_style(&device.state)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("type", LABEL),
                    Span::raw("  "),
                    Span::raw(device.device_type.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("runtime", LABEL),
                    Span::raw("  "),
                    Span::raw(device.runtime.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("os", LABEL),
                    Span::raw("  "),
                    Span::raw(device.os_version.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("available", LABEL),
                    Span::raw("  "),
                    Span::styled(
                        if device.is_available { "yes" } else { "no" },
                        if device.is_available { OK } else { ERR },
                    ),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::styled("actions (enabled for current state)", LABEL));
                for kind in ACTION_ORDER {
                    let enabled = enabled_actions(device).contains(&kind);
                    lines.push(Line::from(vec![
                        Span::styled(
                            if enabled {
                                format!("[{}]", kind.key())
                            } else {
                                "   ".to_string()
                            },
                            if enabled { ACCENT } else { DISABLED },
                        ),
                        Span::raw(" "),
                        Span::styled(kind.label(), if enabled { INFO } else { DISABLED }),
                    ]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Enter = boot when shutdown, open when booted",
                    LABEL,
                ));
            }
            None => {
                lines.push(Line::styled("no device selected", MUTED));
            }
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
    }

    fn logs_title(&self) -> String {
        let marker = if self.focus == Focus::Logs { ">" } else { " " };
        match &self.follow_udid {
            Some(udid) => format!(
                "{marker} logs {} [follow] - {} entries",
                short_udid(udid),
                self.logs.len()
            ),
            None => format!("{marker} logs [off] - press l to follow"),
        }
    }

    fn render_logs(&self, frame: &mut Frame, area: Rect) {
        let body = pane_head(frame, area, &self.logs_title(), self.focus == Focus::Logs);
        let total = self.logs.len();
        let height = body.height as usize;
        let scroll = self.log_scroll.min(total);
        let from = total.saturating_sub(scroll + height);
        let to = total.saturating_sub(scroll);
        let mut lines: Vec<Line> = Vec::new();
        if total == 0 {
            let message = if self.follow_udid.is_some() {
                "waiting for log entries..."
            } else {
                "press l to follow logs for the selected device"
            };
            lines.push(Line::styled(message, MUTED));
        } else {
            for entry in self.logs.range(from..to) {
                lines.push(Line::from(vec![
                    Span::styled(compact_time(&entry.time), MUTED),
                    Span::raw(" "),
                    Span::styled(entry.process.clone(), ACCENT),
                    Span::raw(" "),
                    Span::raw(entry.message.clone()),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), body);
    }

    fn render_activity(&self, frame: &mut Frame, area: Rect) {
        let title = format!(" activity - last {} ops", self.config.activity_capacity);
        let body = pane_head(frame, area, &title, false);
        let mut lines: Vec<Line> = Vec::new();
        for line in self.activity.iter().take(body.height as usize) {
            lines.push(Line::from(vec![
                Span::styled(line.time.clone(), MUTED),
                Span::raw("  "),
                Span::styled(line.text.clone(), line.style()),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), body);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let text: Line = match &self.mode {
            InputMode::Normal => {
                let message = if self.show_help {
                    "help open - press ? or q to close".to_string()
                } else if let Some(udid) = &self.confirm_delete {
                    format!("delete {}: y/N (Enter = no)", short_udid(udid))
                } else if self.focus == Focus::Logs {
                    "[PgUp/PgDn/j/k] scroll logs  [Tab] focus devices  [q] quit".to_string()
                } else {
                    "[up/down/j/k] select  [Enter] boot/open  [b] boot  [s] shutdown  [o] open  [/] search  [f] state filter  [l] logs  [d] delete  [r] refresh  [?] help  [q] quit".to_string()
                };
                let style = if self.confirm_delete.is_some() {
                    WARN
                } else {
                    INFO
                };
                Line::from(Span::styled(message, style))
            }
            InputMode::Edit { kind, text, .. } => Line::from(vec![
                Span::styled(format!("{}: ", kind.title()), ACCENT),
                Span::raw(text.clone()),
            ]),
        };
        frame.render_widget(Paragraph::new(text), area);
    }
}

// ---------------------------------------------------------------------------
// Layout helpers.
// ---------------------------------------------------------------------------

struct Rects {
    top: Rect,
    list: Rect,
    details: Option<Rect>,
    logs: Option<Rect>,
    activity: Rect,
    status: Rect,
}

/// Dense monitoring layout. Wide terminals get list + details + logs;
/// narrow ones drop panes rather than squeezing them into unusable slivers.
fn compute_layout(area: Rect) -> Rects {
    let activity_height = if area.height >= 30 {
        6
    } else if area.height >= 20 {
        4
    } else if area.height >= 14 {
        2
    } else {
        1
    };
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(activity_height),
        Constraint::Length(1),
    ])
    .split(area);
    let main = rows[1];
    let width = main.width;
    let (list, details, logs) = if width >= 110 {
        let cols = Layout::horizontal([
            Constraint::Min(44),
            Constraint::Length(42),
            Constraint::Min(20),
        ])
        .split(main);
        (cols[0], Some(cols[1]), Some(cols[2]))
    } else if width >= 78 {
        let cols = Layout::horizontal([Constraint::Min(30), Constraint::Length(42)]).split(main);
        (cols[0], Some(cols[1]), None)
    } else {
        (main, None, None)
    };
    Rects {
        top: rows[0],
        list,
        details,
        logs,
        activity: rows[2],
        status: rows[3],
    }
}

/// Pane chrome: 2-row title bar with a bottom border, content below. Panes
/// too short for a title just get the content.
fn pane_head(frame: &mut Frame, area: Rect, title: &str, focused: bool) -> Rect {
    if area.height < 3 {
        return area;
    }
    let inner = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(area);
    let head = inner[0];
    frame.render_widget(
        Block::new().borders(Borders::BOTTOM).border_style(BORDER),
        head,
    );
    let style = if focused { ACCENT } else { MUTED };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(title.to_string(), style))),
        Rect::new(head.x, head.y, head.width, 1),
    );
    inner[1]
}

/// Single-cell text writer that truncates instead of wrapping.
fn put(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 {
        return;
    }
    buf.set_stringn(x, y, text, width as usize, style);
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let message = format!(
        "terminal too small: need at least {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
        area.width, area.height
    );
    let center_x = area.x + area.width / 2;
    let center_y = area.y + area.height / 2;
    let buf = frame.buffer_mut();
    let message_width = message.chars().count() as u16;
    if message_width <= area.width {
        put(
            buf,
            center_x - message_width / 2,
            center_y.saturating_sub(1),
            message_width,
            &message,
            WARN,
        );
    }
    if area.width >= 10 {
        put(buf, center_x - 5, center_y, 10, "press q to quit", MUTED);
    }
}

const HELP: &[(&str, &str)] = &[
    ("q / Ctrl+C / Esc", "quit"),
    ("up/down or j/k", "select device"),
    ("g / G", "first / last device"),
    ("Enter", "boot when shutdown, open when booted"),
    ("b / s", "boot / shutdown selected"),
    ("o", "open Simulator.app"),
    ("p", "screenshot (path prompt)"),
    ("l", "follow / unfollow device logs"),
    ("i", "install app (path prompt)"),
    ("a / t / u", "launch / terminate / uninstall app"),
    ("w", "open URL in device"),
    ("d", "delete device (y/N confirmation)"),
    ("r", "refresh now"),
    ("/", "search and filter devices"),
    ("f", "cycle state filter: all/booted/shutdown"),
    ("+ / -", "faster / slower refresh"),
    ("Tab", "cycle focus: devices / logs"),
    ("PgUp / PgDn", "scroll focused pane"),
    ("Esc", "cancel prompt / clear search"),
    ("?", "toggle this help"),
];

fn render_help(frame: &mut Frame, area: Rect) {
    let width = 64u16.min(area.width.saturating_sub(4)).max(20);
    let height = (HELP.len() as u16 + 4)
        .min(area.height.saturating_sub(4))
        .max(8);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::bordered()
            .border_style(BORDER)
            .title(Span::styled(" simtop help ", ACCENT)),
        popup,
    );
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(format!("{key:<28}"), ACCENT),
                Span::raw(*description),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Small formatting helpers (no extra dependencies).
// ---------------------------------------------------------------------------

fn state_short(state: &DeviceState) -> &'static str {
    match state {
        DeviceState::Booted => "booted",
        DeviceState::Booting => "booting",
        DeviceState::ShuttingDown => "shutting down",
        DeviceState::Shutdown => "shutdown",
        DeviceState::Creating => "creating",
        DeviceState::Unknown(_) => "unknown",
    }
}

fn state_style(state: &DeviceState) -> Style {
    match state {
        DeviceState::Booted => OK,
        DeviceState::Booting | DeviceState::ShuttingDown => WARN,
        DeviceState::Shutdown => MUTED,
        DeviceState::Creating => ACCENT,
        DeviceState::Unknown(_) => ERR,
    }
}

/// First 8 characters of a UDID: enough to disambiguate visually.
fn short_udid(udid: &str) -> String {
    udid.chars().take(8).collect()
}

/// RFC 3339 timestamp -> HH:MM:SS for the log pane.
fn compact_time(timestamp: &str) -> String {
    let time = timestamp.rsplit('T').next().unwrap_or(timestamp);
    time.chars().take(8).collect()
}

/// Current UTC time as HH:MM:SS.
fn utc_time() -> String {
    utc_parts().1
}

/// Compact UTC stamp usable in file names: YYYYMMDD-HHMMSS.
fn utc_stamp_compact() -> String {
    let (date, time) = utc_parts();
    format!("{}-{}", date.replace('-', ""), time.replace(':', ""))
}

fn utc_parts() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let hour = secs % 86_400 / 3_600;
    let minute = secs % 3_600 / 60;
    let second = secs % 60;
    (
        format!("{year:04}-{month:02}-{day:02}"),
        format!("{hour:02}:{minute:02}:{second:02}"),
    )
}

/// Days-since-epoch to (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// FNV-1a over the identity-relevant fields of every device: catches state,
/// name, availability, and membership changes even if a backend's generation
/// counter is unreliable.
fn fingerprint(devices: &[SimDevice]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for device in devices {
        mix(&mut hash, device.udid.as_bytes());
        mix(&mut hash, device.state.to_string().as_bytes());
        mix(&mut hash, device.name.as_bytes());
        hash = (hash ^ u64::from(device.is_available)).wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn mix(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash = (*hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3);
    }
}

/// Char-index to byte-index conversion for edit-buffer cursor math.
fn char_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Default screenshot destination: ~/Desktop/simtop-<udid8>-<stamp>.png.
fn screenshot_default_path(udid: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!(
        "{home}/Desktop/simtop-{}-{}.png",
        short_udid(udid),
        utc_stamp_compact()
    )
}

fn delete_confirmation_accepts(key: KeyCode) -> bool {
    matches!(key, KeyCode::Char('y') | KeyCode::Char('Y'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_confirmation_defaults_to_no() {
        assert!(delete_confirmation_accepts(KeyCode::Char('y')));
        assert!(delete_confirmation_accepts(KeyCode::Char('Y')));
        assert!(!delete_confirmation_accepts(KeyCode::Char('n')));
        assert!(!delete_confirmation_accepts(KeyCode::Char('N')));
        assert!(!delete_confirmation_accepts(KeyCode::Enter));
    }
}
