use super::super::backend::SimulatorBackend;
use super::super::build::{BuildEvent, BuildRequest, BuildStage, XcodeBuildRunner};
use super::super::config::{Config, SavedProjectSelection};
use super::super::error::{ErrorCode, SimtopError};
use super::super::model::{DeviceSnapshot, SimDevice};
use super::super::project::{self, ProjectId, ProjectMetadata, XcodeContainer, XcodeProject};
use super::super::run::{ProjectRunCoordinator, ProjectRunEvent, ProjectRunStage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 128;
const MAX_OUTPUT_LINES: usize = 240;
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const DISCOVERY_DEPTH: usize = 3;
const TITLE: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const MUTED: Style = Style::new().fg(Color::DarkGray);
const OK: Style = Style::new().fg(Color::Green);
const ERR: Style = Style::new().fg(Color::Red);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

#[derive(Debug)]
pub(super) enum ProjectUiEvent {
    Config(Result<Config, SimtopError>),
    ConfigSaved(Result<(), SimtopError>),
    Discovery(Result<Vec<XcodeProject>, SimtopError>),
    Metadata {
        project_id: ProjectId,
        result: Result<ProjectMetadata, SimtopError>,
    },
    Build(BuildEvent),
    Run(ProjectRunEvent),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Projects,
    Container,
    Scheme,
    Configuration,
    Simulator,
}
impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Projects => Self::Container,
            Self::Container => Self::Scheme,
            Self::Scheme => Self::Configuration,
            Self::Configuration => Self::Simulator,
            Self::Simulator => Self::Projects,
        }
    }
    fn prev(self) -> Self {
        match self {
            Self::Projects => Self::Simulator,
            Self::Container => Self::Projects,
            Self::Scheme => Self::Container,
            Self::Configuration => Self::Scheme,
            Self::Simulator => Self::Configuration,
        }
    }
}
#[derive(Debug, Clone)]
struct Row {
    project: XcodeProject,
    metadata: Option<ProjectMetadata>,
    loading: bool,
    error: Option<String>,
}
#[derive(Debug)]
enum Input {
    Normal,
    Filter(String),
    Root(String),
}
#[derive(Debug)]
enum Active {
    Build(mpsc::Sender<()>),
    Run(mpsc::Sender<()>),
}
impl Active {
    fn sender(&self) -> &mpsc::Sender<()> {
        match self {
            Self::Build(sender) | Self::Run(sender) => sender,
        }
    }
}

pub(super) struct ProjectsView {
    backend: Arc<dyn SimulatorBackend>,
    developer_dir: PathBuf,
    config_path: PathBuf,
    cache_root: PathBuf,
    launch_dir: PathBuf,
    roots: Vec<PathBuf>,
    rows: Vec<Row>,
    selected: usize,
    focus: Focus,
    input: Input,
    dirty: bool,
    started: bool,
    config: Config,
    config_status: Option<String>,
    devices: Vec<SimDevice>,
    container: Option<XcodeContainer>,
    scheme: Option<String>,
    configuration: Option<String>,
    simulator: Option<String>,
    output: VecDeque<String>,
    output_bytes: usize,
    status: String,
    status_error: bool,
    operation: Option<String>,
    active: Option<Active>,
    tx: mpsc::Sender<ProjectUiEvent>,
    rx: mpsc::Receiver<ProjectUiEvent>,
}

impl ProjectsView {
    pub(super) fn new(
        backend: Arc<dyn SimulatorBackend>,
        developer_dir: PathBuf,
        config_path: PathBuf,
        cache_root: PathBuf,
        launch_dir: PathBuf,
    ) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        Self {
            backend,
            developer_dir,
            config_path,
            cache_root,
            launch_dir,
            roots: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            focus: Focus::Projects,
            input: Input::Normal,
            dirty: true,
            started: false,
            config: Config::default(),
            config_status: None,
            devices: Vec::new(),
            container: None,
            scheme: None,
            configuration: None,
            simulator: None,
            output: VecDeque::new(),
            output_bytes: 0,
            status: "Press r to rescan projects".into(),
            status_error: false,
            operation: None,
            active: None,
            tx,
            rx,
        }
    }
    pub(super) fn kickoff(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let tx = self.tx.clone();
        let path = self.config_path.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || Config::load(path)).await;
            let result = match result {
                Ok(result) => result,
                Err(error) => Err(SimtopError::new(
                    ErrorCode::Internal,
                    format!("config task: {error}"),
                )),
            };
            let _ = tx.send(ProjectUiEvent::Config(result)).await;
        });
        self.rescan();
    }
    pub(super) fn tick(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.handle_event(event);
        }
    }
    pub(super) fn handle_event(&mut self, event: ProjectUiEvent) {
        match event {
            ProjectUiEvent::Config(result) => match result {
                Ok(config) => {
                    self.roots = config.project_roots.clone();
                    self.config = config;
                    self.config_status = Some("configuration loaded".into());
                    self.rescan();
                }
                Err(error) => {
                    self.status = format!("config unavailable: {error}");
                    self.status_error = true;
                    self.rescan();
                }
            },
            ProjectUiEvent::ConfigSaved(result) => {
                if let Err(error) = result {
                    self.status = format!("could not save selection: {error}");
                    self.status_error = true;
                } else {
                    self.config_status = Some("selection saved".into());
                }
            }
            ProjectUiEvent::Discovery(result) => match result {
                Ok(projects) => {
                    let old = self.selected_project().map(|row| row.project.id.clone());
                    self.rows = projects
                        .into_iter()
                        .map(|project| Row {
                            project,
                            metadata: None,
                            loading: false,
                            error: None,
                        })
                        .collect();
                    self.selected = old
                        .and_then(|id| self.rows.iter().position(|row| row.project.id == id))
                        .unwrap_or(0)
                        .min(self.rows.len().saturating_sub(1));
                    self.status = if self.rows.is_empty() {
                        "no Xcode projects found; press a to add a root".into()
                    } else {
                        format!("{} projects discovered", self.rows.len())
                    };
                    self.status_error = false;
                    self.restore();
                    self.load_metadata();
                }
                Err(error) => {
                    self.rows.clear();
                    self.status = format!("discovery failed: {error}");
                    self.status_error = true;
                    self.container = None;
                    self.scheme = None;
                    self.configuration = None;
                }
            },
            ProjectUiEvent::Metadata { project_id, result } => {
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|row| row.project.id == project_id)
                {
                    row.loading = false;
                    match result {
                        Ok(metadata) => {
                            row.metadata = Some(metadata);
                            row.error = None;
                        }
                        Err(error) => {
                            row.error = Some(error.to_string());
                            self.status = format!("metadata failed: {error}");
                            self.status_error = true;
                        }
                    }
                    self.restore_metadata();
                }
            }
            ProjectUiEvent::Build(event) => self.build_event(event),
            ProjectUiEvent::Run(event) => self.run_event(event),
        }
        self.dirty = true;
    }
    pub(super) fn device_snapshot(&mut self, snapshot: DeviceSnapshot) {
        self.devices = snapshot.devices;
        let valid = self.simulator.as_ref().is_some_and(|udid| {
            self.devices
                .iter()
                .any(|device| device.is_available && &device.udid == udid)
        });
        if !valid {
            self.simulator = self
                .devices
                .iter()
                .find(|device| device.is_available)
                .map(|device| device.udid.clone());
            self.persist();
        }
        self.dirty = true;
    }
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.dirty = true;
        if !matches!(self.input, Input::Normal) {
            return self.input_key(key);
        }
        match key.code {
            KeyCode::Tab => {
                self.focus = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.focus.prev()
                } else {
                    self.focus.next()
                }
            }
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Char('/') => self.input = Input::Filter(String::new()),
            KeyCode::Char('a') => self.input = Input::Root(String::new()),
            KeyCode::Char('r') if self.active.is_none() => self.rescan(),
            KeyCode::Char('R') if self.active.is_none() => self.start(true),
            KeyCode::Char('b') | KeyCode::Char('B') => self.start(false),
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => self.cancel(),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Left | KeyCode::Char('h') => self.adjust(-1),
            KeyCode::Right | KeyCode::Char('l') => self.adjust(1),
            KeyCode::Enter => self.load_metadata(),
            _ => return false,
        }
        true
    }
    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 40 || area.height < 5 {
            frame.render_widget(Paragraph::new("terminal too small for Projects"), area);
            self.dirty = false;
            return;
        }
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(29),
                Constraint::Percentage(39),
                Constraint::Percentage(32),
            ])
            .split(vertical[0]);
        self.project_list(frame, body[0]);
        self.setup(frame, body[1]);
        self.output(frame, body[2]);
        self.status(frame, vertical[1]);
        self.dirty = false;
    }
    pub(super) fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty;
        self.dirty = false;
        dirty
    }
    pub(super) fn selected_request(&self) -> Option<BuildRequest> {
        let row = self.selected_project()?;
        Some(BuildRequest {
            project_id: row.project.id.clone(),
            container: self
                .container
                .clone()
                .or_else(|| Some(row.project.preferred_container.clone()))?,
            scheme: self.scheme.clone()?,
            configuration: self.configuration.clone()?,
            simulator_udid: self.simulator.clone()?,
            developer_dir: self.developer_dir.clone(),
            cache_root: self.cache_root.clone(),
        })
    }
    fn input_key(&mut self, key: KeyEvent) -> bool {
        let mut submit = false;
        let mut cancel = false;
        match (&mut self.input, key.code) {
            (Input::Filter(value), KeyCode::Char(character))
            | (Input::Root(value), KeyCode::Char(character))
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                value.push(character)
            }
            (Input::Filter(value), KeyCode::Backspace)
            | (Input::Root(value), KeyCode::Backspace) => {
                value.pop();
            }
            (Input::Filter(_), KeyCode::Enter) | (Input::Root(_), KeyCode::Enter) => submit = true,
            (_, KeyCode::Esc) => cancel = true,
            _ => return true,
        }
        if cancel {
            self.input = Input::Normal;
            return true;
        }
        if submit {
            match std::mem::replace(&mut self.input, Input::Normal) {
                Input::Filter(value) => {
                    self.status = if value.trim().is_empty() {
                        "filter cleared".into()
                    } else {
                        format!("filter: {value}")
                    }
                }
                Input::Root(value) => {
                    let path = PathBuf::from(value.trim());
                    if path.as_os_str().is_empty() {
                        self.status = "root path is empty".into();
                        self.status_error = true;
                    } else {
                        self.add_root(path);
                    }
                }
                Input::Normal => {}
            }
        }
        true
    }
    fn indices(&self) -> Vec<usize> {
        let query = match &self.input {
            Input::Filter(value) => value.to_ascii_lowercase(),
            Input::Normal | Input::Root(_) => String::new(),
        };
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if query.is_empty()
                    || row.project.name.to_ascii_lowercase().contains(&query)
                    || row
                        .project
                        .directory
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&query)
                {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }
    fn selected_project(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }
    fn move_sel(&mut self, delta: i32) {
        if self.focus != Focus::Projects {
            self.adjust(delta);
            return;
        }
        let indices = self.indices();
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0) as i32;
        self.selected = indices[(current + delta).rem_euclid(indices.len() as i32) as usize];
        self.restore();
        self.load_metadata();
    }
    fn choices(&self) -> Vec<String> {
        match self.focus {
            Focus::Container => self
                .selected_project()
                .map(|row| {
                    row.project
                        .containers
                        .iter()
                        .map(|container| container.path().display().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            Focus::Scheme => self
                .selected_project()
                .and_then(|row| row.metadata.as_ref())
                .map(|metadata| metadata.schemes.clone())
                .unwrap_or_default(),
            Focus::Configuration => self
                .selected_project()
                .and_then(|row| row.metadata.as_ref())
                .map(|metadata| metadata.configurations.clone())
                .unwrap_or_default(),
            Focus::Simulator => self
                .devices
                .iter()
                .filter(|device| device.is_available)
                .map(|device| device.udid.clone())
                .collect(),
            Focus::Projects => Vec::new(),
        }
    }
    fn adjust(&mut self, delta: i32) {
        let choices = self.choices();
        if choices.is_empty() {
            return;
        }
        let current = match self.focus {
            Focus::Container => self
                .container
                .as_ref()
                .map(|container| container.path().display().to_string()),
            Focus::Scheme => self.scheme.clone(),
            Focus::Configuration => self.configuration.clone(),
            Focus::Simulator => self.simulator.clone(),
            Focus::Projects => None,
        };
        let current = current
            .and_then(|value| choices.iter().position(|choice| choice == &value))
            .unwrap_or(0) as i32;
        let next = choices[(current + delta).rem_euclid(choices.len() as i32) as usize].clone();
        match self.focus {
            Focus::Container => {
                self.container = self.selected_project().and_then(|row| {
                    row.project
                        .containers
                        .iter()
                        .find(|container| container.path().display().to_string() == next)
                        .cloned()
                });
                self.scheme = None;
                self.configuration = None;
                self.load_metadata();
            }
            Focus::Scheme => self.scheme = Some(next),
            Focus::Configuration => self.configuration = Some(next),
            Focus::Simulator => self.simulator = Some(next),
            Focus::Projects => {}
        }
        self.persist();
    }
    fn restore(&mut self) {
        let Some(row) = self.selected_project() else {
            self.container = None;
            self.scheme = None;
            self.configuration = None;
            self.simulator = None;
            return;
        };
        let saved = self.config.project_selections.get(row.project.id.as_str());
        self.container = saved
            .and_then(|selection| {
                row.project
                    .containers
                    .iter()
                    .find(|container| container.path() == selection.container)
                    .cloned()
            })
            .or_else(|| Some(row.project.preferred_container.clone()));
        self.scheme = saved.map(|selection| selection.scheme.clone());
        self.configuration = saved.map(|selection| selection.configuration.clone());
        self.simulator = saved
            .and_then(|selection| {
                self.devices
                    .iter()
                    .find(|device| device.is_available && device.udid == selection.simulator_udid)
                    .map(|device| device.udid.clone())
            })
            .or_else(|| {
                self.devices
                    .iter()
                    .find(|device| device.is_available)
                    .map(|device| device.udid.clone())
            });
        self.restore_metadata();
    }
    fn restore_metadata(&mut self) {
        let schemes = self
            .selected_project()
            .and_then(|row| row.metadata.as_ref())
            .map(|metadata| metadata.schemes.clone())
            .unwrap_or_default();
        if !schemes.is_empty()
            && self
                .scheme
                .as_ref()
                .map_or(true, |value| !schemes.contains(value))
        {
            self.scheme = schemes.first().cloned();
        }
        let configurations = self
            .selected_project()
            .and_then(|row| row.metadata.as_ref())
            .map(|metadata| metadata.configurations.clone())
            .unwrap_or_default();
        if !configurations.is_empty()
            && self
                .configuration
                .as_ref()
                .map_or(true, |value| !configurations.contains(value))
        {
            self.configuration = configurations
                .iter()
                .find(|value| value.eq_ignore_ascii_case("Debug"))
                .cloned()
                .or_else(|| configurations.first().cloned());
        }
    }
    fn load_metadata(&mut self) {
        let index = self.selected;
        if index >= self.rows.len() || self.rows[index].loading {
            return;
        }
        let Some(container) = self
            .container
            .clone()
            .or_else(|| Some(self.rows[index].project.preferred_container.clone()))
        else {
            return;
        };
        self.rows[index].loading = true;
        let project_id = self.rows[index].project.id.clone();
        let developer_dir = self.developer_dir.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = project::load_metadata(&developer_dir, &container).await;
            let _ = tx
                .send(ProjectUiEvent::Metadata { project_id, result })
                .await;
        });
    }
    fn rescan(&mut self) {
        let roots = self.roots.clone();
        let launch_dir = self.launch_dir.clone();
        let tx = self.tx.clone();
        self.status = "scanning for Xcode projects…".into();
        self.status_error = false;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                project::discover_projects(&roots, &launch_dir, DISCOVERY_DEPTH)
            })
            .await;
            let result = match result {
                Ok(result) => result,
                Err(error) => Err(SimtopError::new(
                    ErrorCode::Internal,
                    format!("discovery task: {error}"),
                )),
            };
            let _ = tx.send(ProjectUiEvent::Discovery(result)).await;
        });
    }
    fn add_root(&mut self, path: PathBuf) {
        let path = if path.is_absolute() {
            path
        } else {
            self.launch_dir.join(path)
        };
        if !self.roots.iter().any(|root| root == &path) {
            self.roots.push(path);
            self.config.project_roots = self.roots.clone();
            self.save_config();
        }
        self.rescan();
    }
    fn persist(&mut self) {
        let Some(row) = self.selected_project() else {
            return;
        };
        let (Some(container), Some(scheme), Some(configuration), Some(simulator_udid)) = (
            self.container.clone(),
            self.scheme.clone(),
            self.configuration.clone(),
            self.simulator.clone(),
        ) else {
            return;
        };
        self.config.project_selections.insert(
            row.project.id.to_string(),
            SavedProjectSelection {
                container: container.path().to_path_buf(),
                scheme,
                configuration,
                simulator_udid,
            },
        );
        self.save_config();
    }
    fn save_config(&self) {
        let config = self.config.clone();
        let path = self.config_path.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || config.save(path)).await;
            let result = match result {
                Ok(result) => result,
                Err(error) => Err(SimtopError::new(
                    ErrorCode::Internal,
                    format!("config save: {error}"),
                )),
            };
            let _ = tx.send(ProjectUiEvent::ConfigSaved(result)).await;
        });
    }
    fn start(&mut self, run: bool) {
        if self.active.is_some() {
            return;
        }
        let Some(request) = self.selected_request() else {
            self.status = "select project, scheme, configuration, and simulator".into();
            self.status_error = true;
            return;
        };
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let tx = self.tx.clone();
        self.output.clear();
        self.output_bytes = 0;
        self.operation = Some(if run { "run" } else { "build" }.into());
        if run {
            let coordinator =
                ProjectRunCoordinator::new(Arc::clone(&self.backend), XcodeBuildRunner::new());
            match coordinator.start(request) {
                Ok(mut handle) => {
                    self.active = Some(Active::Run(cancel_tx));
                    tokio::spawn(async move {
                        loop {
                            tokio::select! { _ = cancel_rx.recv() => handle.cancel(), event = handle.recv() => { let Some(event) = event else { break; }; let done = matches!(event, ProjectRunEvent::Finished(_)); if tx.send(ProjectUiEvent::Run(event)).await.is_err() || done { break; } } }
                        }
                    });
                }
                Err(error) => {
                    self.operation = None;
                    self.status = format!("run start: {error}");
                    self.status_error = true;
                }
            }
        } else {
            match XcodeBuildRunner::new().start(request) {
                Ok(mut handle) => {
                    self.active = Some(Active::Build(cancel_tx));
                    tokio::spawn(async move {
                        loop {
                            tokio::select! { _ = cancel_rx.recv() => handle.cancel(), event = handle.recv() => { let Some(event) = event else { break; }; let done = matches!(event, BuildEvent::Finished(_)); if tx.send(ProjectUiEvent::Build(event)).await.is_err() || done { break; } } }
                        }
                    });
                }
                Err(error) => {
                    self.operation = None;
                    self.status = format!("build start: {error}");
                    self.status_error = true;
                }
            }
        }
    }
    fn cancel(&mut self) {
        if let Some(active) = self.active.as_ref() {
            let _ = active.sender().try_send(());
            self.status = "cancelling operation…".into();
        }
    }
    fn build_event(&mut self, event: BuildEvent) {
        match event {
            BuildEvent::Stage(stage) => {
                self.status = match stage {
                    BuildStage::Building => "building…",
                    BuildStage::ResolvingProduct => "resolving product…",
                }
                .into()
            }
            BuildEvent::Output(line) => self.push(line),
            BuildEvent::Finished(result) => {
                self.active = None;
                self.operation = None;
                match result {
                    Ok(product) => {
                        self.status = format!("build complete: {}", product.app_path.display());
                        self.status_error = false;
                    }
                    Err(error) => {
                        self.status = format!("build failed: {error}");
                        self.status_error = true;
                    }
                }
            }
        }
        self.dirty = true;
    }
    fn run_event(&mut self, event: ProjectRunEvent) {
        match event {
            ProjectRunEvent::Stage(stage) => {
                self.status = match stage {
                    ProjectRunStage::Booting => "booting simulator…",
                    ProjectRunStage::Building => "building…",
                    ProjectRunStage::ResolvingProduct => "resolving product…",
                    ProjectRunStage::Installing => "installing app…",
                    ProjectRunStage::Launching => "launching app…",
                }
                .into()
            }
            ProjectRunEvent::Output(line) => self.push(line),
            ProjectRunEvent::Finished(result) => {
                self.active = None;
                self.operation = None;
                match result {
                    Ok(result) => {
                        self.status = format!("run complete: {}", result.product.bundle_id);
                        self.status_error = false;
                    }
                    Err(error) => {
                        self.status = format!("run failed: {error}");
                        self.status_error = true;
                    }
                }
            }
        }
        self.dirty = true;
    }
    fn push(&mut self, line: String) {
        let line = line.trim_end_matches(['\r', '\n']).to_owned();
        if line.is_empty() {
            return;
        }
        self.output_bytes += line.len();
        self.output.push_back(line);
        while self.output.len() > MAX_OUTPUT_LINES || self.output_bytes > MAX_OUTPUT_BYTES {
            if let Some(oldest) = self.output.pop_front() {
                self.output_bytes = self.output_bytes.saturating_sub(oldest.len());
            }
        }
    }
    fn project_list(&self, frame: &mut Frame, area: Rect) {
        let indices = self.indices();
        let mut state = ListState::default();
        state.select(indices.iter().position(|index| *index == self.selected));
        let items = indices
            .iter()
            .map(|index| {
                let row = &self.rows[*index];
                let marker = if *index == self.selected { "›" } else { " " };
                let detail = if row.loading {
                    "  loading metadata…".to_string()
                } else if let Some(error) = &row.error {
                    format!("  metadata: {error}")
                } else {
                    format!("  {}", row.project.directory.display())
                };
                ListItem::new(vec![
                    Line::from(format!("{marker} {}", row.project.name)),
                    Line::from(Span::styled(detail, MUTED)),
                ])
            })
            .collect::<Vec<_>>();
        let title = match &self.input {
            Input::Normal => format!(" Projects ({}/{}) ", indices.len(), self.rows.len()),
            Input::Filter(value) => format!(" Filter: {value}_ "),
            Input::Root(value) => format!(" Add root: {value}_ "),
        };
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .title(Line::from(Span::styled(title, TITLE)))
                        .borders(Borders::ALL),
                )
                .highlight_style(if self.focus == Focus::Projects {
                    SELECTED
                } else {
                    Style::default()
                }),
            area,
            &mut state,
        );
    }
    fn setup(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().title(" Setup ").borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(1),
            ])
            .split(inner);
        let container = self
            .container
            .as_ref()
            .map(|container| container.path().display().to_string())
            .unwrap_or_else(|| "—".into());
        let scheme = self.scheme.clone().unwrap_or_else(|| "no scheme".into());
        let configuration = self
            .configuration
            .clone()
            .unwrap_or_else(|| "no configuration".into());
        let simulator = self
            .simulator
            .as_ref()
            .and_then(|udid| {
                self.devices
                    .iter()
                    .find(|device| &device.udid == udid)
                    .map(|device| format!("{} ({})", device.name, &udid[..udid.len().min(8)]))
            })
            .unwrap_or_else(|| "no simulator".into());
        self.field(frame, rows[0], "Container", &container, Focus::Container);
        self.field(frame, rows[1], "Scheme", &scheme, Focus::Scheme);
        self.field(
            frame,
            rows[2],
            "Configuration",
            &configuration,
            Focus::Configuration,
        );
        self.field(frame, rows[3], "Simulator", &simulator, Focus::Simulator);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "1/2 switch views · ←/→ select · Tab focus · Enter load metadata · b build · R run · c cancel · a add root · / filter · r rescan",
                MUTED,
            ))
            .wrap(Wrap { trim: true }),
            rows[4],
        );
    }
    fn field(&self, frame: &mut Frame, area: Rect, label: &str, value: &str, focus: Focus) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{label}: "),
                    if self.focus == focus { ACCENT } else { MUTED },
                ),
                Span::styled(
                    value,
                    if self.focus == focus {
                        SELECTED
                    } else {
                        Style::default()
                    },
                ),
            ]))
            .wrap(Wrap { trim: true }),
            area,
        );
    }
    fn output(&self, frame: &mut Frame, area: Rect) {
        let lines = self
            .output
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev()
            .map(|line| Line::from(line.as_str()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(self.operation.as_deref().unwrap_or("Output"))
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    fn status(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(
                if self.status_error { "! " } else { "· " },
                if self.status_error { ERR } else { OK },
            ),
            Span::raw(self.status.as_str()),
        ];
        if let Some(config_status) = &self.config_status {
            spans.push(Span::styled(format!(" [{config_status}]"), MUTED));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::TOP)),
            area,
        );
    }
}
