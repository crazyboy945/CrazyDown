use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Id, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    pos2, vec2,
};
use egui_extras::{Column, TableBuilder};
use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::cli::SourceSpec;
use crate::engine::{self, FileTypeRule, Options, SourceKind, output::OutputRegistry};
use crate::ui::Ui;
use crate::util::{self, format_bytes, format_duration};

const BG: Color32 = Color32::from_rgb(8, 12, 22);
const HEADER_BG: Color32 = Color32::from_rgb(10, 15, 27);
const CARD: Color32 = Color32::from_rgb(17, 23, 38);
const SIDEBAR_BG: Color32 = Color32::from_rgb(11, 17, 29);
const STRIPE: Color32 = Color32::from_rgb(13, 19, 32);
const TABLE_HEADER: Color32 = Color32::from_rgb(20, 28, 45);
const SELECTED_BG: Color32 = Color32::from_rgb(13, 53, 70);
const BORDER: Color32 = Color32::from_rgb(33, 44, 67);
const MUTED: Color32 = Color32::from_rgb(139, 154, 180);
const SUBTLE: Color32 = Color32::from_rgb(95, 110, 135);
const ACCENT: Color32 = Color32::from_rgb(56, 189, 248);
const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
const RED: Color32 = Color32::from_rgb(248, 113, 113);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
const ORANGE: Color32 = Color32::from_rgb(251, 146, 60);
const PURPLE: Color32 = Color32::from_rgb(167, 139, 250);

const CLOSE_GRACE: Duration = Duration::from_secs(3);

pub fn run(initial_sources: Vec<String>) -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("CrazyDown")
        .with_inner_size([1160.0, 640.0])
        .with_min_inner_size([960.0, 560.0])
        .with_position([40.0, 28.0]);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    };
    eframe::run_native(
        "CrazyDown",
        options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, initial_sources)))),
    )
}

fn default_save_dir() -> PathBuf {
    for variable in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(variable) {
            let downloads = PathBuf::from(home).join("Downloads");
            if downloads.is_dir() {
                return downloads;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

struct Settings {
    dir: String,
    jobs: usize,
    segments: usize,
    retries: usize,
    timeout_secs: u64,
    ftp_user: String,
    ftp_pass: String,
    force: bool,
    insecure: bool,
    by_file_type: bool,
    file_type_rules: Vec<FileTypeRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dir: default_save_dir().display().to_string(),
            jobs: 3,
            segments: 8,
            retries: 3,
            timeout_secs: 30,
            ftp_user: String::new(),
            ftp_pass: String::new(),
            force: false,
            insecure: false,
            by_file_type: false,
            file_type_rules: engine::default_file_type_rules(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedSettings {
    dir: String,
    jobs: usize,
    segments: usize,
    retries: usize,
    timeout_secs: u64,
    ftp_user: String,
    force: bool,
    insecure: bool,
    by_file_type: bool,
    file_type_rules: Vec<FileTypeRule>,
}

impl From<&Settings> for PersistedSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            dir: settings.dir.clone(),
            jobs: settings.jobs,
            segments: settings.segments,
            retries: settings.retries,
            timeout_secs: settings.timeout_secs,
            ftp_user: settings.ftp_user.clone(),
            force: settings.force,
            insecure: settings.insecure,
            by_file_type: settings.by_file_type,
            file_type_rules: settings.file_type_rules.clone(),
        }
    }
}

impl PersistedSettings {
    fn into_settings(self) -> Settings {
        let defaults = Settings::default();
        Settings {
            dir: if self.dir.trim().is_empty() {
                defaults.dir
            } else {
                self.dir
            },
            jobs: self.jobs.clamp(1, 64),
            segments: self.segments.clamp(1, 32),
            retries: self.retries.min(20),
            timeout_secs: self.timeout_secs.clamp(1, 3600),
            ftp_user: self.ftp_user,
            ftp_pass: String::new(),
            force: self.force,
            insecure: self.insecure,
            by_file_type: self.by_file_type,
            file_type_rules: if self.file_type_rules.is_empty() {
                defaults.file_type_rules
            } else {
                self.file_type_rules
            },
        }
    }
}

fn config_path() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata).join("crazydown").join("config.json"));
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config).join("crazydown").join("config.json"));
    }
    for variable in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(variable) {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("crazydown")
                    .join("config.json"),
            );
        }
    }
    None
}

fn load_settings() -> (Settings, Option<String>) {
    let Some(path) = config_path() else {
        return (Settings::default(), None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<PersistedSettings>(&text) {
            Ok(persisted) => (persisted.into_settings(), None),
            Err(error) => (
                Settings::default(),
                Some(format!("配置文件格式错误，已使用默认设置：{error}")),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
        Err(error) => (
            Settings::default(),
            Some(format!("无法读取配置文件，已使用默认设置：{error}")),
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskFilter {
    All,
    Running,
    Pending,
    Completed,
    Failed,
}

impl TaskFilter {
    const PRIMARY: [Self; 5] = [
        Self::All,
        Self::Running,
        Self::Pending,
        Self::Completed,
        Self::Failed,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "全部任务",
            Self::Running => "下载中",
            Self::Pending => "等待中",
            Self::Completed => "已完成",
            Self::Failed => "失败 / 取消",
        }
    }

    fn accent(self) -> Color32 {
        match self {
            Self::All => Color32::from_rgb(148, 163, 184),
            Self::Running => ACCENT,
            Self::Pending => MUTED,
            Self::Completed => GREEN,
            Self::Failed => RED,
        }
    }

    fn matches(self, task: &GuiTask) -> bool {
        match self {
            Self::All => true,
            Self::Running => task.status == TaskStatus::Running,
            Self::Pending => task.status == TaskStatus::Pending,
            Self::Completed => task.status == TaskStatus::Completed,
            Self::Failed => matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled),
        }
    }
}

struct GuiTask {
    id: u64,
    spec: SourceSpec,
    kind: SourceKind,
    dir: PathBuf,
    label: String,
    status: TaskStatus,
    bar: ProgressBar,
    cancel: CancellationToken,
    output: Option<PathBuf>,
    error: Option<String>,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
    last_sample: Instant,
    speed_samples: VecDeque<(Instant, u64)>,
    speed: f64,
}

impl GuiTask {
    fn new(id: u64, spec: SourceSpec, kind: SourceKind, dir: PathBuf) -> Self {
        let label = task_label(&spec, kind);
        let bar = ProgressBar::hidden();
        bar.set_message("等待开始");
        Self {
            id,
            spec,
            kind,
            dir,
            label,
            status: TaskStatus::Pending,
            bar,
            cancel: CancellationToken::new(),
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            last_sample: Instant::now(),
            speed_samples: VecDeque::new(),
            speed: 0.0,
        }
    }

    fn sample_speed(&mut self) {
        if self.status != TaskStatus::Running {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.last_sample) < Duration::from_millis(120) {
            return;
        }
        self.last_sample = now;
        let position = self.bar.position();
        self.speed_samples.push_back((now, position));
        while self.speed_samples.len() > 2
            && now.saturating_duration_since(self.speed_samples[0].0) > Duration::from_secs(2)
        {
            self.speed_samples.pop_front();
        }
        if let Some((start, start_pos)) = self.speed_samples.front().copied() {
            let elapsed = now.saturating_duration_since(start).as_secs_f64();
            if elapsed >= 0.2 {
                self.speed = position.saturating_sub(start_pos) as f64 / elapsed;
            }
        }
    }

    fn reset_speed(&mut self) {
        self.last_sample = Instant::now();
        self.speed_samples.clear();
        self.speed = 0.0;
    }

    fn total_bytes(&self) -> Option<u64> {
        self.bar.length().filter(|total| *total > 0)
    }

    fn fraction(&self) -> Option<f32> {
        let total = self.total_bytes()?;
        Some((self.bar.position() as f32 / total as f32).clamp(0.0, 1.0))
    }

    fn eta(&self) -> Option<String> {
        let total = self.total_bytes()?;
        if self.speed < 1024.0 {
            return None;
        }
        let remaining = total.saturating_sub(self.bar.position()) as f64;
        Some(format_eta(remaining / self.speed))
    }

    fn size_text(&self) -> String {
        self.total_bytes()
            .map(format_bytes)
            .unwrap_or_else(|| "—".to_string())
    }

    fn speed_text(&self) -> String {
        if self.status == TaskStatus::Running {
            format!("{}/s", format_bytes(self.speed.round() as u64))
        } else {
            "—".to_string()
        }
    }

    fn status_text(&self) -> &'static str {
        match self.status {
            TaskStatus::Pending => "等待中",
            TaskStatus::Running if self.cancel.is_cancelled() => "正在取消",
            TaskStatus::Running => "下载中",
            TaskStatus::Completed => "已完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Cancelled => "已取消",
        }
    }

    fn status_color(&self) -> Color32 {
        match self.status {
            TaskStatus::Pending => MUTED,
            TaskStatus::Running if self.cancel.is_cancelled() => AMBER,
            TaskStatus::Running => ACCENT,
            TaskStatus::Completed => GREEN,
            TaskStatus::Failed => RED,
            TaskStatus::Cancelled => AMBER,
        }
    }

    fn progress_color(&self) -> Color32 {
        match self.status {
            TaskStatus::Completed => GREEN,
            TaskStatus::Failed => RED,
            TaskStatus::Cancelled => AMBER,
            _ => ACCENT,
        }
    }
}

enum GuiEvent {
    Finished {
        id: u64,
        result: std::result::Result<PathBuf, String>,
    },
}

enum TaskAction {
    Cancel(u64),
    Retry(u64),
    Remove(u64),
    Open(u64),
    OpenDir(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolIcon {
    Add,
    Resume,
    Stop,
    StopAll,
    Delete,
    Clear,
    Folder,
    Settings,
    Logs,
}

#[derive(Clone, Copy)]
enum ToolbarAction {
    Add,
    Resume,
    Stop,
    StopAll,
    Delete,
    ClearCompleted,
    Open,
    Settings,
    ToggleLogs,
}

enum EmptyAction {
    None,
    Add,
}

struct GuiApp {
    runtime: Runtime,
    session: Arc<OnceCell<Arc<librqbit::Session>>>,
    registry: Arc<OutputRegistry>,
    event_tx: UnboundedSender<GuiEvent>,
    event_rx: UnboundedReceiver<GuiEvent>,
    log_tx: UnboundedSender<String>,
    log_rx: UnboundedReceiver<String>,
    tasks: Vec<GuiTask>,
    next_id: u64,
    settings: Settings,
    source_input: String,
    add_error: Option<String>,
    logs: Vec<String>,
    icon: Option<egui::TextureHandle>,
    bt_dir: Option<PathBuf>,
    filter: TaskFilter,
    search: String,
    selected: Option<u64>,
    show_add: bool,
    focus_add: bool,
    show_settings: bool,
    show_logs: bool,
    closing: bool,
    allow_close: bool,
    close_deadline: Option<Instant>,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>, initial_sources: Vec<String>) -> Self {
        install_fonts(&cc.egui_ctx);
        install_style(&cc.egui_ctx);
        let icon = load_icon_texture(&cc.egui_ctx);
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("crazydown-gui-io")
            .build()
            .expect("无法创建异步运行时");

        let (settings, settings_warning) = load_settings();
        let mut app = Self {
            runtime,
            session: Arc::new(OnceCell::new()),
            registry: Arc::new(OutputRegistry::new()),
            event_tx,
            event_rx,
            log_tx,
            log_rx,
            tasks: Vec::new(),
            next_id: 1,
            settings,
            source_input: String::new(),
            add_error: None,
            logs: Vec::new(),
            icon,
            bt_dir: None,
            filter: TaskFilter::All,
            search: String::new(),
            selected: None,
            show_add: false,
            focus_add: false,
            show_settings: false,
            show_logs: false,
            closing: false,
            allow_close: false,
            close_deadline: None,
        };

        if let Some(warning) = settings_warning {
            app.push_log(format!("警告: {warning}"));
        }
        if initial_sources.is_empty() {
            app.push_log("就绪：新建下载，或导入批量列表 / 种子文件。");
        } else {
            let specs = initial_sources
                .into_iter()
                .map(|source| SourceSpec {
                    source,
                    output: None,
                })
                .collect();
            app.add_specs(specs);
        }
        app
    }

    fn push_log(&mut self, message: impl Into<String>) {
        if self.logs.len() >= 800 {
            self.logs.remove(0);
        }
        self.logs.push(message.into());
    }

    fn current_dir(&self) -> PathBuf {
        util::absolutize(Path::new(self.settings.dir.trim()))
    }

    fn options(&self) -> Options {
        Options {
            dir: self.current_dir(),
            jobs: self.settings.jobs,
            segments: self.settings.segments,
            retries: self.settings.retries,
            timeout: Duration::from_secs(self.settings.timeout_secs),
            ftp_user: non_empty(&self.settings.ftp_user),
            ftp_pass: non_empty(&self.settings.ftp_pass),
            force: self.settings.force,
            insecure: self.settings.insecure,
            quiet: true,
            no_progress: true,
            file_type_rules: if self.settings.by_file_type {
                self.settings.file_type_rules.clone()
            } else {
                Vec::new()
            },
        }
    }

    fn active_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status.is_active())
            .count()
    }

    fn pending_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Pending)
            .count()
    }

    fn completed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count()
    }

    fn failed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled))
            .count()
    }

    fn total_speed(&self) -> f64 {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
            .map(|task| task.speed)
            .sum()
    }

    fn selected_task(&self) -> Option<&GuiTask> {
        self.selected
            .and_then(|id| self.tasks.iter().find(|task| task.id == id))
    }

    fn filter_count(&self, filter: TaskFilter) -> usize {
        self.tasks
            .iter()
            .filter(|task| filter.matches(task))
            .count()
    }

    fn add_specs(&mut self, specs: Vec<SourceSpec>) -> bool {
        let mut added = 0;
        let mut errors = Vec::new();
        for spec in specs {
            match self.add_spec(spec) {
                Ok(()) => added += 1,
                Err(error) => {
                    let message = format!("{error:#}");
                    self.push_log(format!("添加失败：{message}"));
                    errors.push(message);
                }
            }
        }
        self.add_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("\n"))
        };
        if added > 0 {
            self.filter = TaskFilter::All;
            self.search.clear();
            self.selected = self.tasks.last().map(|task| task.id);
        }
        self.pump();
        added > 0
    }

    fn add_spec(&mut self, spec: SourceSpec) -> Result<()> {
        let kind = engine::classify(&spec.source)?;
        let dir = self.current_dir();
        if kind == SourceKind::Torrent {
            if spec.output.is_some() {
                bail!("BT 任务不能单独指定文件名，请使用保存目录");
            }
            if let Some(bound) = &self.bt_dir
                && bound != &dir
            {
                bail!(
                    "BT 会话已绑定保存目录 {}，请重启程序后再更换目录",
                    bound.display()
                );
            }
            self.bt_dir.get_or_insert_with(|| dir.clone());
        }

        let id = self.next_id;
        self.next_id += 1;
        self.tasks.push(GuiTask::new(id, spec.clone(), kind, dir));
        self.push_log(format!(
            "已添加 [{}] {}",
            id,
            util::display_source(&spec.source)
        ));
        Ok(())
    }

    fn submit_input(&mut self) -> bool {
        if self.source_input.trim().is_empty() {
            self.add_error = Some("请输入下载地址".to_string());
            return false;
        }
        match crate::cli::parse_batch_content(&self.source_input) {
            Ok(specs) if specs.is_empty() => {
                self.add_error = Some("没有可添加的地址".to_string());
                false
            }
            Ok(specs) => {
                let added = self.add_specs(specs);
                if added {
                    self.source_input.clear();
                }
                added
            }
            Err(error) => {
                self.add_error = Some(format!("{error:#}"));
                false
            }
        }
    }

    fn pick_directory(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("选择保存目录");
        let dir = self.current_dir();
        if dir.is_dir() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_folder() {
            self.settings.dir = path.display().to_string();
            self.save_settings();
        }
    }

    fn pick_rule_directory(&mut self, index: usize) {
        let Some(rule) = self.settings.file_type_rules.get(index) else {
            return;
        };
        let name = rule.name.trim().to_string();
        let configured = rule.dir.trim().to_string();
        let base = self.current_dir();
        let path = Path::new(&configured);
        let start = if path.is_absolute() {
            util::absolutize(path)
        } else {
            util::absolutize(&base.join(path))
        };

        let title = if name.is_empty() {
            "选择类型保存目录".to_string()
        } else {
            format!("选择 {name} 保存目录")
        };
        let mut dialog = rfd::FileDialog::new().set_title(&title);
        if start.is_dir() {
            dialog = dialog.set_directory(start);
        } else if base.is_dir() {
            dialog = dialog.set_directory(base);
        }
        if let Some(path) = dialog.pick_folder() {
            self.settings.file_type_rules[index].dir = path.display().to_string();
            self.save_settings();
        }
    }

    fn save_settings(&mut self) {
        let Some(path) = config_path() else {
            return;
        };
        let result = (|| -> Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("无法创建配置目录 {}", parent.display()))?;
            }
            let text = serde_json::to_string_pretty(&PersistedSettings::from(&self.settings))
                .context("无法序列化设置")?;
            std::fs::write(&path, text).with_context(|| format!("无法写入 {}", path.display()))
        })();
        if let Err(error) = result {
            self.push_log(format!("警告: {error:#}"));
        }
    }

    fn pick_batch_file(&mut self) -> bool {
        let Some(path) = rfd::FileDialog::new()
            .set_title("选择批量下载列表")
            .add_filter("文本列表", &["txt", "list"])
            .pick_file()
        else {
            return false;
        };
        match crate::cli::parse_batch_file(&path) {
            Ok(specs) => self.add_specs(specs),
            Err(error) => {
                self.add_error = Some(format!("{error:#}"));
                false
            }
        }
    }

    fn pick_torrent_file(&mut self) -> bool {
        let Some(path) = rfd::FileDialog::new()
            .set_title("选择种子文件")
            .add_filter("BitTorrent 种子", &["torrent"])
            .pick_file()
        else {
            return false;
        };
        self.add_specs(vec![SourceSpec {
            source: path.display().to_string(),
            output: None,
        }])
    }

    fn open_add_modal(&mut self) {
        self.show_add = true;
        self.focus_add = true;
        self.add_error = None;
    }

    fn pump(&mut self) {
        if self.closing {
            return;
        }
        let limit = self.settings.jobs.max(1);
        loop {
            let running = self.active_count();
            if running >= limit {
                break;
            }
            let Some(index) = self
                .tasks
                .iter()
                .position(|task| task.status == TaskStatus::Pending)
            else {
                break;
            };
            self.start_task(index);
        }
    }

    fn start_task(&mut self, index: usize) {
        let mut opts = self.options();
        opts.dir = self.tasks[index].dir.clone();
        let kind = self.tasks[index].kind;
        let id = self.tasks[index].id;
        let label = self.tasks[index].label.clone();
        let mut spec = self.tasks[index].spec.clone();
        if let Some(output) = &spec.output
            && !output.is_absolute()
        {
            spec.output = Some(opts.dir.join(output));
        }

        let (bar, cancel) = {
            let task = &mut self.tasks[index];
            task.status = TaskStatus::Running;
            task.error = None;
            task.output = None;
            task.started_at = Some(Instant::now());
            task.finished_at = None;
            task.reset_speed();
            task.cancel = CancellationToken::new();
            task.bar.reset();
            task.bar.set_message("准备中");
            (task.bar.clone(), task.cancel.clone())
        };

        self.push_log(format!("开始 [{id}] {label}"));

        let event_tx = self.event_tx.clone();
        let log_tx = self.log_tx.clone();
        let registry = self.registry.clone();
        let session = self.session.clone();
        self.runtime.spawn(async move {
            let ui = Ui::with_sink(move |line| {
                let _ = log_tx.send(line);
            });
            let result = run_task(TaskJob {
                kind,
                spec,
                opts,
                ui: &ui,
                bar: &bar,
                cancel: &cancel,
                registry: &registry,
                session: &session,
            })
            .await;
            let _ = event_tx.send(GuiEvent::Finished {
                id,
                result: result.map_err(|error| format!("{error:#}")),
            });
        });
    }

    fn cancel_task(&mut self, id: u64) {
        let mut log_line = None;
        {
            let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
                return;
            };
            match task.status {
                TaskStatus::Pending => {
                    task.status = TaskStatus::Cancelled;
                    task.bar.set_message("已取消");
                    log_line = Some(format!("已取消等待中的任务 [{}] {}", id, task.label));
                }
                TaskStatus::Running => {
                    task.cancel.cancel();
                    task.bar.set_message("正在取消");
                    log_line = Some(format!("正在取消 [{}] {}", id, task.label));
                }
                _ => {}
            }
        }
        if let Some(line) = log_line {
            self.push_log(line);
        }
    }

    fn retry_task(&mut self, id: u64) {
        let label;
        {
            let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
                return;
            };
            if !matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled) {
                return;
            }
            task.status = TaskStatus::Pending;
            task.error = None;
            task.output = None;
            task.finished_at = None;
            task.reset_speed();
            task.bar.reset();
            task.bar.set_message("等待开始");
            label = task.label.clone();
        }
        self.selected = Some(id);
        self.filter = TaskFilter::All;
        self.search.clear();
        self.push_log(format!("重新排队 [{id}] {label}"));
        self.pump();
    }

    fn remove_task(&mut self, id: u64) {
        if let Some(index) = self.tasks.iter().position(|task| task.id == id)
            && self.tasks[index].status != TaskStatus::Running
        {
            let task = self.tasks.remove(index);
            if self.selected == Some(id) {
                self.selected = None;
            }
            self.push_log(format!("已从列表移除 [{}] {}", task.id, task.label));
        }
    }

    fn clear_completed(&mut self) {
        let before = self.tasks.len();
        self.tasks
            .retain(|task| task.status != TaskStatus::Completed);
        if self
            .selected
            .is_some_and(|id| !self.tasks.iter().any(|task| task.id == id))
        {
            self.selected = None;
        }
        let removed = before - self.tasks.len();
        if removed > 0 {
            self.push_log(format!("已清除 {removed} 个完成的任务记录"));
        }
    }

    fn cancel_all(&mut self) {
        for task in &mut self.tasks {
            match task.status {
                TaskStatus::Pending => {
                    task.status = TaskStatus::Cancelled;
                    task.bar.set_message("已取消");
                }
                TaskStatus::Running => {
                    task.cancel.cancel();
                    task.bar.set_message("正在取消");
                }
                _ => {}
            }
        }
        self.push_log("已请求取消所有任务，正在保存断点…");
    }

    fn open_location(&mut self) {
        let path = if let Some(task) = self.selected_task() {
            task.output.clone().unwrap_or_else(|| task.dir.clone())
        } else {
            self.current_dir()
        };
        if let Err(error) = open_path(&path) {
            self.push_log(format!("无法打开 {}: {error}", path.display()));
        }
    }

    fn finish_task(&mut self, event: GuiEvent) {
        let GuiEvent::Finished { id, result } = event;
        let Some(index) = self.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let label = self.tasks[index].label.clone();
        let source = util::display_source(&self.tasks[index].spec.source);
        let log_line: String;
        {
            let task = &mut self.tasks[index];
            task.finished_at = Some(Instant::now());
            task.reset_speed();
            match result {
                Ok(path) => {
                    if let Some(total) = task.bar.length() {
                        task.bar.set_position(total);
                    }
                    task.bar.set_message("完成");
                    task.status = TaskStatus::Completed;
                    task.output = Some(path.clone());
                    let size = std::fs::metadata(&path)
                        .ok()
                        .filter(|metadata| metadata.is_file())
                        .map(|metadata| metadata.len())
                        .or(task.bar.length())
                        .map(|bytes| format!("，{}", format_bytes(bytes)))
                        .unwrap_or_default();
                    let duration = task
                        .started_at
                        .map(|start| Instant::now().saturating_duration_since(start))
                        .unwrap_or_default();
                    log_line = format!(
                        "完成 [{}] {} -> {}{}，用时 {}",
                        id,
                        label,
                        path.display(),
                        size,
                        format_duration(duration)
                    );
                }
                Err(message) => {
                    let cancelled = task.cancel.is_cancelled();
                    task.status = if cancelled {
                        TaskStatus::Cancelled
                    } else {
                        TaskStatus::Failed
                    };
                    task.bar
                        .set_message(if cancelled { "已取消" } else { "失败" });
                    if cancelled {
                        log_line = format!("已取消 [{}] {}", id, label);
                    } else {
                        task.error = Some(message.clone());
                        log_line = format!("失败 [{}] {}: {}", id, source, message);
                    }
                }
            }
        }
        self.push_log(log_line);
        self.pump();
    }

    fn apply_action(&mut self, action: TaskAction) {
        match action {
            TaskAction::Cancel(id) => self.cancel_task(id),
            TaskAction::Retry(id) => self.retry_task(id),
            TaskAction::Remove(id) => self.remove_task(id),
            TaskAction::Open(id) => {
                let Some(task) = self.tasks.iter().find(|task| task.id == id) else {
                    return;
                };
                let Some(path) = &task.output else {
                    return;
                };
                if let Err(error) = open_path(path) {
                    self.push_log(format!("无法打开 {}: {error}", path.display()));
                }
            }
            TaskAction::OpenDir(path) => {
                if let Err(error) = open_path(&path) {
                    self.push_log(format!("无法打开 {}: {error}", path.display()));
                }
            }
        }
    }

    fn handle_toolbar(&mut self, action: ToolbarAction) {
        match action {
            ToolbarAction::Add => self.open_add_modal(),
            ToolbarAction::Resume => {
                if let Some(id) = self.selected {
                    self.retry_task(id);
                }
            }
            ToolbarAction::Stop => {
                if let Some(id) = self.selected {
                    self.cancel_task(id);
                }
            }
            ToolbarAction::StopAll => self.cancel_all(),
            ToolbarAction::Delete => {
                if let Some(id) = self.selected {
                    self.remove_task(id);
                }
            }
            ToolbarAction::ClearCompleted => self.clear_completed(),
            ToolbarAction::Open => self.open_location(),
            ToolbarAction::Settings => self.show_settings = true,
            ToolbarAction::ToggleLogs => self.show_logs = !self.show_logs,
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.show_add || self.show_settings {
            return;
        }
        if ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::N)) {
            self.open_add_modal();
        }
        if ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Comma)) {
            self.show_settings = true;
        }
        if !ctx.wants_keyboard_input()
            && ctx.input(|input| input.key_pressed(egui::Key::Delete))
            && let Some(id) = self.selected
        {
            self.remove_task(id);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(line) = self.log_rx.try_recv() {
            self.push_log(line);
        }
        while let Ok(event) = self.event_rx.try_recv() {
            self.finish_task(event);
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing {
                self.closing = true;
                self.show_add = false;
                self.show_settings = false;
                self.close_deadline = Some(Instant::now() + CLOSE_GRACE);
                if self.active_count() > 0 {
                    self.cancel_all();
                }
            }
        }

        if self.closing && !self.allow_close {
            let expired = self
                .close_deadline
                .is_some_and(|deadline| Instant::now() >= deadline);
            if self.active_count() == 0 || expired {
                self.save_settings();
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    fn paint(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("chrome")
            .frame(egui::Frame::default().fill(HEADER_BG))
            .show(ctx, |ui| {
                self.paint_header(ui);
                self.paint_toolbar(ui);
            });

        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(26.0)
            .frame(
                egui::Frame::default()
                    .fill(HEADER_BG)
                    .inner_margin(Margin::symmetric(12, 4)),
            )
            .show(ctx, |ui| self.paint_status(ui));

        egui::SidePanel::left("categories")
            .exact_width(190.0)
            .resizable(false)
            .frame(
                egui::Frame::default()
                    .fill(SIDEBAR_BG)
                    .inner_margin(Margin {
                        left: 8,
                        right: 8,
                        top: 10,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| self.paint_sidebar(ui));

        if self.show_logs {
            egui::TopBottomPanel::bottom("log_panel")
                .exact_height(150.0)
                .frame(
                    egui::Frame::default()
                        .fill(CARD)
                        .inner_margin(Margin::symmetric(12, 8)),
                )
                .show(ctx, |ui| self.paint_logs(ui));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(BG).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 9,
                bottom: 6,
            }))
            .show(ctx, |ui| self.paint_tasks(ui));
    }

    fn paint_header(&self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .inner_margin(Margin::symmetric(14, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(icon) = &self.icon {
                        ui.image((icon.id(), vec2(31.0, 31.0)));
                    }
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(
                            RichText::new("CrazyDown")
                                .size(19.0)
                                .strong()
                                .color(Color32::from_rgb(235, 241, 250)),
                        );
                    });
                });
            });
    }

    fn paint_toolbar(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        let selected_status = self.selected_task().map(|task| task.status);
        let can_resume = matches!(
            selected_status,
            Some(TaskStatus::Failed | TaskStatus::Cancelled)
        );
        let can_stop = matches!(
            selected_status,
            Some(TaskStatus::Running | TaskStatus::Pending)
        );
        let can_delete = matches!(
            selected_status,
            Some(
                TaskStatus::Pending
                    | TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            )
        );
        let has_busy = self.active_count() > 0 || self.pending_count() > 0;
        let has_completed = self.completed_count() > 0;
        let show_logs = self.show_logs;

        egui::Frame::default()
            .fill(CARD)
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(1.0, 0.0);

                    if tool_button(
                        ui,
                        ToolIcon::Add,
                        "新建下载",
                        "新建下载任务 (Ctrl+N)",
                        true,
                        false,
                        ACCENT,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::Add);
                    }
                    toolbar_separator(ui);
                    if tool_button(
                        ui,
                        ToolIcon::Resume,
                        "继续",
                        "继续选中的失败或取消任务",
                        can_resume,
                        false,
                        GREEN,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::Resume);
                    }
                    if tool_button(
                        ui,
                        ToolIcon::Stop,
                        "停止",
                        "停止选中的任务",
                        can_stop,
                        false,
                        RED,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::Stop);
                    }
                    if tool_button(
                        ui,
                        ToolIcon::StopAll,
                        "全部停止",
                        "停止所有下载并保存断点",
                        has_busy,
                        false,
                        RED,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::StopAll);
                    }
                    toolbar_separator(ui);
                    if tool_button(
                        ui,
                        ToolIcon::Delete,
                        "删除",
                        "从列表删除选中的任务 (Delete)",
                        can_delete,
                        false,
                        RED,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::Delete);
                    }
                    if tool_button(
                        ui,
                        ToolIcon::Clear,
                        "清除完成",
                        "清除所有已完成的任务记录",
                        has_completed,
                        false,
                        GREEN,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::ClearCompleted);
                    }
                    if tool_button(
                        ui,
                        ToolIcon::Folder,
                        "打开目录",
                        "打开所选任务的保存位置",
                        true,
                        false,
                        ORANGE,
                    )
                    .clicked()
                    {
                        action = Some(ToolbarAction::Open);
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if tool_button(
                            ui,
                            ToolIcon::Settings,
                            "设置",
                            "下载设置 (Ctrl+,)",
                            true,
                            false,
                            MUTED,
                        )
                        .clicked()
                        {
                            action = Some(ToolbarAction::Settings);
                        }
                        if tool_button(
                            ui,
                            ToolIcon::Logs,
                            "日志",
                            "显示或隐藏运行日志",
                            true,
                            show_logs,
                            ACCENT,
                        )
                        .clicked()
                        {
                            action = Some(ToolbarAction::ToggleLogs);
                        }
                    });
                });
            });
        if let Some(action) = action {
            self.handle_toolbar(action);
        }
    }

    fn paint_status(&self, ui: &mut egui::Ui) {
        ui.painter().hline(
            ui.max_rect().x_range(),
            ui.max_rect().top(),
            Stroke::new(1.0_f32, BORDER),
        );
        ui.horizontal_centered(|ui| {
            let (text, color) = if self.closing {
                ("正在保存断点并退出…", AMBER)
            } else {
                ("就绪", MUTED)
            };
            ui.label(RichText::new(text).size(10.5).color(color));
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "全部 {}  ·  下载中 {}  ·  等待 {}  ·  完成 {}  ·  失败/取消 {}",
                    self.tasks.len(),
                    self.active_count(),
                    self.pending_count(),
                    self.completed_count(),
                    self.failed_count()
                ))
                .size(10.5)
                .color(SUBTLE),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new("Ctrl+N 新建 · Ctrl+, 设置 · 双击打开 / 重试")
                        .size(10.5)
                        .color(SUBTLE),
                );
                if self.active_count() > 0 {
                    let speed = self.total_speed();
                    ui.label(
                        RichText::new(format!("总速度 {}/s", format_bytes(speed.round() as u64)))
                            .size(10.5)
                            .color(ACCENT),
                    );
                }
            });
        });
    }

    fn paint_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("下载分类").size(10.0).strong().color(SUBTLE));
        ui.add_space(3.0);

        let mut chosen = self.filter;
        for filter in TaskFilter::PRIMARY {
            if category_row(ui, filter, self.filter_count(filter), self.filter == filter) {
                chosen = filter;
            }
        }
        if chosen != self.filter {
            self.filter = chosen;
            self.selected = None;
        }

        if ui.available_height() > 96.0 {
            ui.add_space(ui.available_height() - 96.0);
        }
        let path = self.settings.dir.clone();
        egui::Frame::default()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(8.0)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let color = ORANGE;
                    let (rect, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                    draw_tool_icon(
                        ui.painter(),
                        ToolIcon::Folder,
                        Rect::from_center_size(rect.center(), vec2(13.0, 13.0)),
                        color,
                    );
                    ui.label(RichText::new("保存目录").size(10.0).color(SUBTLE));
                });
                let label = ui.add(
                    egui::Label::new(
                        RichText::new(&path)
                            .size(10.5)
                            .color(Color32::from_rgb(188, 201, 220)),
                    )
                    .truncate(),
                );
                label.on_hover_text(&path);
                ui.add_space(1.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("更改目录…").size(10.5).color(ACCENT))
                            .frame(false),
                    )
                    .clicked()
                {
                    self.pick_directory();
                }
            });
    }

    fn paint_tasks(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.filter.label())
                    .size(16.0)
                    .strong()
                    .color(Color32::from_rgb(232, 240, 250)),
            );
            ui.label(
                RichText::new(format!("{} 项", self.visible_count()))
                    .size(10.5)
                    .color(SUBTLE),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("搜索任务…")
                        .desired_width(190.0)
                        .font(egui::TextStyle::Small),
                );
                if search.changed() {
                    self.selected = None;
                }
                if !self.search.is_empty()
                    && ui
                        .add(egui::Button::new("×").frame(false))
                        .on_hover_text("清除搜索")
                        .clicked()
                {
                    self.search.clear();
                }
            });
        });
        ui.add_space(6.0);

        let query = self.search.trim().to_lowercase();
        let visible: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, task)| {
                self.filter.matches(task)
                    && (query.is_empty()
                        || task.label.to_lowercase().contains(&query)
                        || task.spec.source.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();

        if visible.is_empty() && self.filter == TaskFilter::All && query.is_empty() {
            if let EmptyAction::Add = empty_state(ui) {
                self.open_add_modal();
            }
            return;
        }

        let selected_id = self.selected;
        let tasks = &self.tasks;
        let mut clicked = None;
        let mut double_clicked = None;
        let mut actions = Vec::new();

        ui.spacing_mut().item_spacing.y = 0.0;
        TableBuilder::new(ui)
            .id_salt("downloads_table")
            .striped(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(Align::Center))
            .auto_shrink([false, false])
            .min_scrolled_height(0.0)
            .column(Column::remainder().at_least(260.0).clip(true))
            .column(Column::initial(88.0).at_least(78.0).clip(true))
            .column(Column::initial(86.0).at_least(78.0).clip(true))
            .column(Column::initial(112.0).at_least(100.0).clip(true))
            .column(Column::initial(92.0).at_least(82.0).clip(true))
            .column(Column::initial(86.0).at_least(78.0).clip(true))
            .header(28.0, |mut header| {
                header.col(|ui| table_header_cell(ui, "文件名", false));
                header.col(|ui| table_header_cell(ui, "大小", true));
                header.col(|ui| table_header_cell(ui, "状态", false));
                header.col(|ui| table_header_cell(ui, "进度", false));
                header.col(|ui| table_header_cell(ui, "速度", true));
                header.col(|ui| table_header_cell(ui, "剩余时间", true));
            })
            .body(|body| {
                body.rows(30.0, visible.len(), |mut row| {
                    let Some(&task_index) = visible.get(row.index()) else {
                        return;
                    };
                    let task = &tasks[task_index];
                    let selected = selected_id == Some(task.id);
                    let source = util::display_source(&task.spec.source);
                    let tooltip = format!("{}\n保存到：{}", source, task.dir.display());

                    row.col(|ui| {
                        paint_row_background(ui, selected, true);
                        ui.spacing_mut().item_spacing.x = 7.0;
                        protocol_chip(ui, task.kind);
                        let label = ui.add(
                            egui::Label::new(RichText::new(&task.label).size(12.0).color(
                                if selected {
                                    Color32::from_rgb(238, 247, 255)
                                } else {
                                    Color32::from_rgb(214, 224, 238)
                                },
                            ))
                            .truncate(),
                        );
                        label.on_hover_text(&tooltip);
                    });
                    row.col(|ui| {
                        paint_row_background(ui, selected, false);
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(task.size_text()).size(11.5).color(MUTED));
                        });
                    });
                    row.col(|ui| {
                        paint_row_background(ui, selected, false);
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let (rect, _) = ui.allocate_exact_size(vec2(7.0, 7.0), Sense::hover());
                        ui.painter()
                            .circle_filled(rect.center(), 2.5, task.status_color());
                        let label = ui.label(
                            RichText::new(task.status_text())
                                .size(11.5)
                                .color(task.status_color()),
                        );
                        if let Some(error) = &task.error {
                            label.on_hover_text(error);
                        }
                    });
                    row.col(|ui| {
                        paint_row_background(ui, selected, false);
                        let fraction = task.fraction();
                        let color = task.progress_color();
                        let (bar, _) = ui.allocate_exact_size(vec2(54.0, 6.0), Sense::hover());
                        ui.painter()
                            .rect_filled(bar, 3.0, Color32::from_rgb(30, 42, 62));
                        if let Some(fraction) = fraction {
                            let width = bar.width() * fraction;
                            if width > 0.5 {
                                ui.painter().rect_filled(
                                    Rect::from_min_size(bar.min, vec2(width, bar.height())),
                                    3.0,
                                    color,
                                );
                            }
                        }
                        ui.add_space(5.0);
                        let text = match fraction {
                            Some(fraction) if fraction >= 0.9999 => "100%".to_string(),
                            Some(fraction) => format!("{:.1}%", fraction * 100.0),
                            None => "—".to_string(),
                        };
                        ui.label(RichText::new(text).size(10.5).color(if fraction.is_some() {
                            color
                        } else {
                            SUBTLE
                        }));
                    });
                    row.col(|ui| {
                        paint_row_background(ui, selected, false);
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let color = if task.status == TaskStatus::Running {
                                Color32::from_rgb(125, 211, 252)
                            } else {
                                SUBTLE
                            };
                            ui.label(RichText::new(task.speed_text()).size(11.0).color(color));
                        });
                    });
                    row.col(|ui| {
                        paint_row_background(ui, selected, false);
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let (text, color) = match task.status {
                                TaskStatus::Completed => ("完成".to_string(), GREEN),
                                TaskStatus::Running => {
                                    (task.eta().unwrap_or_else(|| "—".to_string()), MUTED)
                                }
                                _ => ("—".to_string(), SUBTLE),
                            };
                            ui.label(RichText::new(text).size(10.5).color(color));
                        });
                    });

                    let response = row.response();
                    if response.clicked() {
                        clicked = Some(task.id);
                    }
                    if response.double_clicked() {
                        double_clicked = Some(task.id);
                    }
                    response.context_menu(|ui| {
                        ui.set_min_width(152.0);
                        if ui
                            .add_enabled(task.output.is_some(), egui::Button::new("打开位置"))
                            .clicked()
                        {
                            actions.push(TaskAction::Open(task.id));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(
                                matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled),
                                egui::Button::new("重试下载"),
                            )
                            .clicked()
                        {
                            actions.push(TaskAction::Retry(task.id));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(
                                matches!(task.status, TaskStatus::Running | TaskStatus::Pending),
                                egui::Button::new("停止下载"),
                            )
                            .clicked()
                        {
                            actions.push(TaskAction::Cancel(task.id));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(
                                task.status != TaskStatus::Running,
                                egui::Button::new("从列表删除"),
                            )
                            .clicked()
                        {
                            actions.push(TaskAction::Remove(task.id));
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("复制下载地址").clicked() {
                            ui.ctx().copy_text(task.spec.source.clone());
                            ui.close_menu();
                        }
                        if ui.button("打开保存目录").clicked() {
                            actions.push(TaskAction::OpenDir(task.dir.clone()));
                            ui.close_menu();
                        }
                    });
                });
            });

        if let Some(id) = clicked {
            self.selected = Some(id);
        }
        if let Some(id) = double_clicked {
            self.selected = Some(id);
            if let Some(task) = self.tasks.iter().find(|task| task.id == id) {
                match task.status {
                    TaskStatus::Completed => actions.push(TaskAction::Open(id)),
                    TaskStatus::Failed | TaskStatus::Cancelled => {
                        actions.push(TaskAction::Retry(id));
                    }
                    _ => {}
                }
            }
        }
        for action in actions {
            self.apply_action(action);
        }
    }

    fn visible_count(&self) -> usize {
        let query = self.search.trim().to_lowercase();
        self.tasks
            .iter()
            .filter(|task| {
                self.filter.matches(task)
                    && (query.is_empty()
                        || task.label.to_lowercase().contains(&query)
                        || task.spec.source.to_lowercase().contains(&query))
            })
            .count()
    }

    fn paint_logs(&mut self, ui: &mut egui::Ui) {
        let mut close = false;
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("运行日志")
                    .size(12.5)
                    .strong()
                    .color(Color32::from_rgb(214, 225, 240)),
            );
            ui.label(
                RichText::new(format!("{} 条", self.logs.len()))
                    .size(10.0)
                    .color(SUBTLE),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("×").frame(false))
                    .on_hover_text("隐藏日志")
                    .clicked()
                {
                    close = true;
                }
                if ui
                    .add_enabled(
                        !self.logs.is_empty(),
                        egui::Button::new(RichText::new("清空").size(10.5)),
                    )
                    .clicked()
                {
                    clear = true;
                }
            });
        });
        ui.add_space(2.0);
        ui.painter().hline(
            ui.max_rect().x_range(),
            ui.cursor().top(),
            Stroke::new(1.0_f32, BORDER),
        );
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.logs.is_empty() {
                    ui.label(RichText::new("暂无日志").size(11.0).color(SUBTLE));
                } else {
                    for line in &self.logs {
                        ui.add(
                            egui::Label::new(
                                RichText::new(line)
                                    .monospace()
                                    .size(10.5)
                                    .color(log_color(line)),
                            )
                            .wrap(),
                        );
                    }
                }
            });
        if clear {
            self.logs.clear();
        }
        if close {
            self.show_logs = false;
        }
    }

    fn paint_modals(&mut self, ctx: &egui::Context) {
        if self.show_add {
            let mut close = false;
            let response = egui::Modal::new(Id::new("add_download_modal"))
                .frame(modal_frame())
                .backdrop_color(Color32::from_black_alpha(150))
                .show(ctx, |ui| {
                    close = self.add_modal_contents(ui);
                });
            if close || response.should_close() {
                self.show_add = false;
                self.focus_add = false;
            }
        }

        if self.show_settings {
            let mut close = false;
            let response = egui::Modal::new(Id::new("settings_modal"))
                .frame(modal_frame())
                .backdrop_color(Color32::from_black_alpha(150))
                .show(ctx, |ui| {
                    close = self.settings_modal_contents(ui);
                });
            if close || response.should_close() {
                self.show_settings = false;
            }
        }
    }

    fn add_modal_contents(&mut self, ui: &mut egui::Ui) -> bool {
        let mut close = false;
        ui.set_width(560.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("新建下载")
                    .size(20.0)
                    .strong()
                    .color(Color32::from_rgb(236, 244, 252)),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if close_button(ui) {
                    close = true;
                }
            });
        });
        ui.label(
            RichText::new(
                "每行一个地址，可在地址后用空格指定文件名；支持磁力链接与本地 .torrent 文件",
            )
            .size(10.5)
            .color(MUTED),
        );
        ui.add_space(8.0);

        ui.label(RichText::new("下载地址").size(11.5).strong().color(MUTED));
        let editor = ui.add(
            egui::TextEdit::multiline(&mut self.source_input)
                .hint_text(
                    "https://example.com/file.zip\n\
                     ftp://user:pass@example.com/file.iso\n\
                     magnet:?xt=urn:btih:...",
                )
                .desired_rows(5)
                .desired_width(f32::INFINITY),
        );
        if editor.changed() {
            self.add_error = None;
        }
        if self.focus_add {
            editor.request_focus();
            self.focus_add = false;
        }
        ui.label(
            RichText::new("提示：Ctrl+Enter 快速开始；BT 任务会使用保存目录。")
                .size(10.0)
                .color(SUBTLE),
        );

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("导入批量列表…").clicked() && self.pick_batch_file() {
                close = true;
            }
            if ui.button("选择种子…").clicked() && self.pick_torrent_file() {
                close = true;
            }
        });

        ui.add_space(10.0);
        ui.label(RichText::new("保存目录").size(11.5).strong().color(MUTED));
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 82.0).max(220.0);
            ui.add(egui::TextEdit::singleline(&mut self.settings.dir).desired_width(width));
            if ui.button("浏览…").clicked() {
                self.pick_directory();
            }
        });

        if let Some(error) = self.add_error.clone() {
            ui.add_space(6.0);
            egui::Frame::default()
                .fill(RED.gamma_multiply(0.1))
                .stroke(Stroke::new(1.0_f32, RED.gamma_multiply(0.4)))
                .corner_radius(6.0)
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.label(RichText::new(error).size(10.5).color(RED));
                });
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("添加后立即开始下载，可在列表中右键管理任务。")
                    .size(10.0)
                    .color(SUBTLE),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, "开始下载").clicked() && self.submit_input() {
                    close = true;
                }
                if secondary_button(ui, "取消").clicked() {
                    close = true;
                }
            });
        });

        if ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter))
            && self.submit_input()
        {
            close = true;
        }

        close
    }

    fn settings_modal_contents(&mut self, ui: &mut egui::Ui) -> bool {
        let mut close = false;
        ui.set_width(600.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("设置")
                    .size(20.0)
                    .strong()
                    .color(Color32::from_rgb(236, 244, 252)),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if close_button(ui) {
                    close = true;
                }
            });
        });
        ui.label(
            RichText::new("设置对之后开始的任务生效，正在下载的任务保持原有参数。")
                .size(10.5)
                .color(MUTED),
        );

        egui::ScrollArea::vertical()
            .max_height(410.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section_title(ui, "下载");
        ui.horizontal(|ui| {
            ui.label("保存目录");
            let width = (ui.available_width() - 82.0).max(220.0);
            ui.add(egui::TextEdit::singleline(&mut self.settings.dir).desired_width(width));
            if ui.button("浏览…").clicked() {
                self.pick_directory();
            }
        });

        ui.add_space(2.0);
        egui::Grid::new("settings_numbers")
            .num_columns(4)
            .spacing([10.0, 7.0])
            .show(ui, |ui| {
                ui.label("并发任务");
                ui.add(
                    egui::DragValue::new(&mut self.settings.jobs)
                        .range(1..=16)
                        .suffix(" 个"),
                );
                ui.label("分段连接");
                ui.add(
                    egui::DragValue::new(&mut self.settings.segments)
                        .range(1..=32)
                        .suffix(" 个"),
                );
                ui.end_row();

                ui.label("失败重试");
                ui.add(
                    egui::DragValue::new(&mut self.settings.retries)
                        .range(0..=20)
                        .suffix(" 次"),
                );
                ui.label("读写超时");
                ui.add(
                    egui::DragValue::new(&mut self.settings.timeout_secs)
                        .range(1..=3600)
                        .suffix(" 秒"),
                );
                ui.end_row();
            });

        section_title(ui, "按类型保存");
        let _ = ui
            .checkbox(&mut self.settings.by_file_type, "按文件类型保存到不同目录")
            .on_hover_text("HTTP / FTP 根据最终文件名匹配类型；BT 仍使用会话目录");
        if self.settings.by_file_type {
            ui.label(
                RichText::new("扩展名用空格或逗号分隔，* 表示其他类型。目录可填绝对路径或相对于默认保存目录的子目录。")
                    .size(10.0)
                    .color(SUBTLE),
            );
            let mut add_rule = false;
            let mut reset_rules = false;
            let mut delete_rule = None;
            let mut browse_rule = None;
            egui::CollapsingHeader::new("类型目录规则")
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("添加类型").clicked() {
                            add_rule = true;
                        }
                        if ui.button("恢复默认").clicked() {
                            reset_rules = true;
                        }
                    });
                    egui::ScrollArea::vertical()
                        .max_height(210.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (index, rule) in
                                self.settings.file_type_rules.iter_mut().enumerate()
                            {
                                ui.push_id(index, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut rule.name)
                                                .desired_width(72.0),
                                        );
                                        ui.add(
                                            egui::TextEdit::singleline(&mut rule.extensions)
                                                .desired_width(310.0)
                                                .hint_text("扩展名，例如 mp4 mkv 或 *"),
                                        );
                                        if ui.button("删除").clicked() {
                                            delete_rule = Some(index);
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("目录");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut rule.dir)
                                                .desired_width(395.0)
                                                .hint_text("留空使用默认保存目录"),
                                        );
                                        if ui.button("浏览…").clicked() {
                                            browse_rule = Some(index);
                                        }
                                    });
                                    ui.separator();
                                });
                            }
                        });
                });
            if add_rule {
                self.settings.file_type_rules.push(FileTypeRule {
                    name: "新类型".into(),
                    extensions: String::new(),
                    dir: String::new(),
                });
                self.save_settings();
            }
            if reset_rules {
                self.settings.file_type_rules = engine::default_file_type_rules();
                self.save_settings();
            }
            if let Some(index) = delete_rule
                && index < self.settings.file_type_rules.len()
            {
                self.settings.file_type_rules.remove(index);
                self.save_settings();
            }
            if let Some(index) = browse_rule {
                self.pick_rule_directory(index);
            }
        }

        section_title(ui, "FTP 认证");
        ui.horizontal(|ui| {
            ui.label("用户名");
            ui.add(egui::TextEdit::singleline(&mut self.settings.ftp_user).desired_width(150.0));
            ui.add_space(6.0);
            ui.label("密码");
            ui.add(
                egui::TextEdit::singleline(&mut self.settings.ftp_pass)
                    .password(true)
                    .desired_width(150.0),
            );
        });

        section_title(ui, "安全与文件");
        ui.horizontal(|ui| {
            let _ = ui
                .checkbox(&mut self.settings.force, "覆盖已有文件")
                .on_hover_text("忽略或清除旧断点，覆盖已有文件");
            ui.add_space(8.0);
            let _ = ui
                .checkbox(&mut self.settings.insecure, "跳过 HTTPS 证书校验")
                .on_hover_text("仅用于自签名或测试环境，存在中间人攻击风险");
        });
        if self.settings.insecure {
            ui.label(
                RichText::new("警告：HTTPS 证书校验已关闭，连接可能被中间人攻击")
                    .size(10.5)
                    .color(AMBER),
            );
        }
        ui.label(
            RichText::new("分段连接仅对 HTTP / FTP 生效；BT 速度由种子与节点决定。")
                .size(10.0)
                .color(SUBTLE),
        );
        if let Some(dir) = &self.bt_dir {
            ui.label(
                RichText::new(format!(
                    "BT 会话目录已固定为 {}，重启程序后可更换。",
                    dir.display()
                ))
                .size(10.0)
                .color(SUBTLE),
            );
        }
            });

        ui.add_space(12.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("HTTP / FTP 支持多线程与断点续传")
                    .size(10.0)
                    .color(SUBTLE),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, "完成").clicked() {
                    close = true;
                }
            });
        });

        if close {
            self.save_settings();
        }
        close
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_close(ctx);
        self.drain_events();
        self.pump();
        for task in &mut self.tasks {
            task.sample_speed();
        }
        self.handle_shortcuts(ctx);
        self.paint(ctx);
        self.paint_modals(ctx);
        self.pump();

        if self.active_count() > 0 || self.pending_count() > 0 || self.closing {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }
}

struct TaskJob<'a> {
    kind: SourceKind,
    spec: SourceSpec,
    opts: Options,
    ui: &'a Ui,
    bar: &'a ProgressBar,
    cancel: &'a CancellationToken,
    registry: &'a OutputRegistry,
    session: &'a OnceCell<Arc<librqbit::Session>>,
}

async fn run_task(job: TaskJob<'_>) -> Result<PathBuf> {
    let TaskJob {
        kind,
        spec,
        opts,
        ui,
        bar,
        cancel,
        registry,
        session,
    } = job;
    std::fs::create_dir_all(&opts.dir)
        .with_context(|| format!("无法创建保存目录 {}", opts.dir.display()))?;
    match kind {
        SourceKind::Http => {
            let client = engine::build_http_client(&opts)?;
            engine::http::download(&client, &opts, registry, &spec, ui, bar.clone(), cancel).await
        }
        SourceKind::Ftp => {
            engine::ftp::download(&opts, registry, &spec, ui, bar.clone(), cancel).await
        }
        SourceKind::Torrent => {
            engine::torrent::download(
                &spec.source,
                &opts.dir,
                session,
                ui,
                bar,
                cancel,
                opts.force,
            )
            .await
        }
    }
}

fn empty_state(ui: &mut egui::Ui) -> EmptyAction {
    let mut action = EmptyAction::None;
    ui.add_space((ui.available_height() * 0.18).min(84.0));
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        let center = rect.center();
        let painter = ui.painter();
        painter.circle_filled(center, 28.0, ACCENT.gamma_multiply(0.1));
        painter.circle_stroke(
            center,
            28.0,
            Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.4)),
        );
        painter.line_segment(
            [center + vec2(0.0, -11.0), center + vec2(0.0, 7.0)],
            Stroke::new(2.0_f32, ACCENT),
        );
        painter.line_segment(
            [center + vec2(0.0, 7.0), center + vec2(-6.0, 1.0)],
            Stroke::new(2.0_f32, ACCENT),
        );
        painter.line_segment(
            [center + vec2(0.0, 7.0), center + vec2(6.0, 1.0)],
            Stroke::new(2.0_f32, ACCENT),
        );
        painter.line_segment(
            [center + vec2(-10.0, 12.0), center + vec2(10.0, 12.0)],
            Stroke::new(1.6_f32, ACCENT),
        );

        ui.add_space(10.0);
        ui.label(
            RichText::new("还没有下载任务")
                .size(16.0)
                .strong()
                .color(Color32::from_rgb(222, 232, 245)),
        );
        ui.label(
            RichText::new("支持 HTTP / HTTPS / FTP 链接、磁力链接和 .torrent 文件")
                .size(10.5)
                .color(MUTED),
        );
        ui.add_space(12.0);
        if primary_button(ui, "新建下载").clicked() {
            action = EmptyAction::Add;
        }
    });
    action
}

fn table_header_cell(ui: &mut egui::Ui, text: &str, align_right: bool) {
    let rect = ui.max_rect().expand2(0.5 * ui.spacing().item_spacing);
    ui.painter().rect_filled(rect, 0.0, TABLE_HEADER);
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0_f32, BORDER),
    );
    let color = Color32::from_rgb(143, 158, 182);
    if align_right {
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(text).size(10.5).strong().color(color));
        });
    } else {
        ui.label(RichText::new(text).size(10.5).strong().color(color));
    }
}

fn paint_row_background(ui: &egui::Ui, selected: bool, first_cell: bool) {
    if !selected {
        return;
    }
    let rect = ui.max_rect().expand2(0.5 * ui.spacing().item_spacing);
    ui.painter().rect_filled(rect, 0.0, SELECTED_BG);
    if first_cell {
        ui.painter().rect_filled(
            Rect::from_min_max(
                pos2(rect.left(), rect.top()),
                pos2(rect.left() + 3.0, rect.bottom()),
            ),
            1.0,
            ACCENT,
        );
    }
}

fn protocol_chip(ui: &mut egui::Ui, kind: SourceKind) -> egui::Response {
    let (text, color) = match kind {
        SourceKind::Http => ("HTTP", ACCENT),
        SourceKind::Ftp => ("FTP", ORANGE),
        SourceKind::Torrent => ("BT", PURPLE),
    };
    let (rect, response) = ui.allocate_exact_size(vec2(34.0, 15.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, color.gamma_multiply(0.13));
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0_f32, color.gamma_multiply(0.42)),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(8.5),
        color,
    );
    response
}

fn category_row(ui: &mut egui::Ui, filter: TaskFilter, count: usize, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 27.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 6.0, ACCENT.gamma_multiply(0.14));
        painter.rect_filled(
            Rect::from_min_max(
                pos2(rect.left(), rect.top() + 4.0),
                pos2(rect.left() + 3.0, rect.bottom() - 4.0),
            ),
            2.0,
            ACCENT,
        );
    } else if response.hovered() {
        painter.rect_filled(rect, 6.0, Color32::from_white_alpha(6));
    }

    let color = filter.accent();
    let icon_rect =
        Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(14.0, 14.0));
    draw_filter_icon(painter, filter, icon_rect, color);
    painter.text(
        pos2(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        filter.label(),
        FontId::proportional(12.0),
        if selected {
            Color32::from_rgb(236, 245, 252)
        } else {
            Color32::from_rgb(176, 189, 209)
        },
    );
    painter.text(
        pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        count.to_string(),
        FontId::proportional(10.0),
        if selected { color } else { SUBTLE },
    );
    response.clicked()
}

fn draw_filter_icon(painter: &egui::Painter, filter: TaskFilter, rect: Rect, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(1.3_f32, color);
    match filter {
        TaskFilter::All => {
            for (dx, dy) in [(-3.5, -3.5), (3.5, -3.5), (-3.5, 3.5), (3.5, 3.5)] {
                painter.rect_filled(
                    Rect::from_center_size(center + vec2(dx, dy), vec2(4.5, 4.5)),
                    1.0,
                    color,
                );
            }
        }
        TaskFilter::Running => {
            painter.line_segment(
                [center + vec2(0.0, -7.0), center + vec2(0.0, 5.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(0.0, 5.0), center + vec2(-3.5, 1.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(0.0, 5.0), center + vec2(3.5, 1.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(-6.0, 7.0), center + vec2(6.0, 7.0)],
                Stroke::new(1.4_f32, color),
            );
        }
        TaskFilter::Pending => {
            painter.circle_stroke(center, 6.5, stroke);
            painter.line_segment([center, center + vec2(0.0, -3.5)], stroke);
            painter.line_segment([center, center + vec2(3.0, 1.0)], stroke);
        }
        TaskFilter::Completed => {
            painter.circle_stroke(center, 6.5, stroke);
            painter.line_segment(
                [center + vec2(-3.0, 0.0), center + vec2(-0.5, 3.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(-0.5, 3.0), center + vec2(3.5, -3.0)],
                Stroke::new(1.6_f32, color),
            );
        }
        TaskFilter::Failed => {
            painter.circle_stroke(center, 6.5, stroke);
            painter.line_segment(
                [center + vec2(0.0, -3.5), center + vec2(0.0, 1.5)],
                Stroke::new(1.5_f32, color),
            );
            painter.circle_filled(center + vec2(0.0, 4.0), 1.0, color);
        }
    }
}

fn tool_button(
    ui: &mut egui::Ui,
    icon: ToolIcon,
    label: &str,
    tooltip: &str,
    enabled: bool,
    active: bool,
    tint: Color32,
) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(64.0, 48.0), sense);
    let hovered = enabled && response.hovered();
    let background = if active {
        tint.gamma_multiply(0.16)
    } else if hovered {
        Color32::from_rgb(27, 37, 56)
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    if background != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink2(vec2(1.5, 1.0)), 7.0, background);
    }

    let icon_color = if enabled {
        tint
    } else {
        SUBTLE.gamma_multiply(0.7)
    };
    draw_tool_icon(
        painter,
        icon,
        Rect::from_center_size(pos2(rect.center().x, rect.top() + 16.0), vec2(19.0, 19.0)),
        icon_color,
    );
    let text_color = if !enabled {
        SUBTLE.gamma_multiply(0.8)
    } else if active {
        tint
    } else {
        Color32::from_rgb(185, 198, 218)
    };
    painter.text(
        pos2(rect.center().x, rect.bottom() - 10.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(10.5),
        text_color,
    );
    if enabled {
        response.on_hover_text(tooltip)
    } else {
        response
    }
}

fn draw_tool_icon(painter: &egui::Painter, icon: ToolIcon, rect: Rect, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(1.5_f32, color);
    match icon {
        ToolIcon::Add => {
            painter.circle_filled(center, 9.0, color.gamma_multiply(0.18));
            painter.circle_stroke(center, 9.0, stroke);
            let plus = Stroke::new(1.8_f32, color);
            painter.line_segment([center + vec2(-4.5, 0.0), center + vec2(4.5, 0.0)], plus);
            painter.line_segment([center + vec2(0.0, -4.5), center + vec2(0.0, 4.5)], plus);
        }
        ToolIcon::Resume => {
            painter.circle_stroke(center, 9.0, stroke);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + vec2(-3.0, -5.0),
                    center + vec2(5.0, 0.0),
                    center + vec2(-3.0, 5.0),
                ],
                color,
                Stroke::NONE,
            ));
        }
        ToolIcon::Stop => {
            painter.rect_filled(Rect::from_center_size(center, vec2(15.0, 15.0)), 3.0, color);
        }
        ToolIcon::StopAll => {
            painter.rect_stroke(
                Rect::from_center_size(center - vec2(2.5, 2.5), vec2(11.0, 11.0)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            painter.rect_filled(
                Rect::from_center_size(center + vec2(3.0, 3.0), vec2(10.0, 10.0)),
                2.0,
                color,
            );
        }
        ToolIcon::Delete => {
            painter.rect_stroke(
                Rect::from_center_size(center + vec2(0.0, 2.0), vec2(11.0, 12.0)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment(
                [center + vec2(-8.0, -6.0), center + vec2(8.0, -6.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(-3.0, -9.0), center + vec2(3.0, -9.0)],
                Stroke::new(1.6_f32, color),
            );
            painter.line_segment(
                [center + vec2(-2.5, 0.0), center + vec2(-2.5, 6.0)],
                Stroke::new(1.2_f32, color),
            );
            painter.line_segment(
                [center + vec2(2.5, 0.0), center + vec2(2.5, 6.0)],
                Stroke::new(1.2_f32, color),
            );
        }
        ToolIcon::Clear => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + vec2(0.0, -9.0),
                    center + vec2(7.0, -5.0),
                    center + vec2(7.0, 4.0),
                    center + vec2(0.0, 9.0),
                    center + vec2(-7.0, 4.0),
                    center + vec2(-7.0, -5.0),
                ],
                color.gamma_multiply(0.12),
                stroke,
            ));
            let check = Stroke::new(1.8_f32, color);
            painter.line_segment([center + vec2(-3.5, 0.0), center + vec2(-1.0, 3.0)], check);
            painter.line_segment([center + vec2(-1.0, 3.0), center + vec2(4.0, -3.5)], check);
        }
        ToolIcon::Folder => {
            let folder = Rect::from_min_max(center + vec2(-9.0, -5.0), center + vec2(9.0, 7.0));
            painter.rect_filled(folder, 2.0, color.gamma_multiply(0.12));
            painter.rect_stroke(folder, 2.0, stroke, StrokeKind::Inside);
            painter.rect_stroke(
                Rect::from_min_max(center + vec2(-9.0, -8.0), center + vec2(-2.0, -4.0)),
                1.5,
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment(
                [center + vec2(-9.0, -4.0), center + vec2(9.0, -4.0)],
                Stroke::new(1.2_f32, color),
            );
        }
        ToolIcon::Settings => {
            painter.circle_stroke(center, 3.5, Stroke::new(1.5_f32, color));
            for index in 0..8 {
                let angle = index as f32 * std::f32::consts::TAU / 8.0;
                let direction = vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [center + direction * 5.5, center + direction * 8.5],
                    Stroke::new(1.7_f32, color),
                );
            }
            painter.circle_filled(center, 1.3, color);
        }
        ToolIcon::Logs => {
            painter.rect_stroke(
                Rect::from_center_size(center, vec2(14.0, 17.0)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            for (index, width) in [9.0_f32, 7.0, 9.0].into_iter().enumerate() {
                let y = center.y - 4.0 + index as f32 * 4.0;
                painter.line_segment(
                    [
                        pos2(center.x - 4.5, y),
                        pos2(center.x - 4.5 + width - 4.5, y),
                    ],
                    Stroke::new(1.3_f32, color),
                );
            }
        }
    }
}

fn toolbar_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 44.0), Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.top() + 7.0..=rect.bottom() - 7.0,
        Stroke::new(1.0_f32, BORDER),
    );
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(3.0, 12.0), Sense::hover());
        ui.painter().rect_filled(rect, 1.0, ACCENT);
        ui.label(
            RichText::new(text)
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(196, 211, 230)),
        );
    });
    ui.add_space(2.0);
}

fn modal_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(14.0)
        .inner_margin(Margin::same(18))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(120),
        })
}

fn close_button(ui: &mut egui::Ui) -> bool {
    ui.add(
        egui::Button::new(RichText::new("×").size(18.0).color(MUTED))
            .frame(false)
            .min_size(vec2(24.0, 24.0)),
    )
    .on_hover_text("关闭")
    .clicked()
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .size(12.0)
                .color(Color32::from_rgb(6, 18, 28)),
        )
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .corner_radius(6.0)
        .min_size(vec2(92.0, 30.0)),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0))
            .fill(Color32::from_rgb(27, 37, 56))
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(6.0)
            .min_size(vec2(84.0, 30.0)),
    )
}

fn format_eta(seconds: f64) -> String {
    let secs = seconds.ceil() as u64;
    if secs >= 86_400 * 99 {
        return ">99 天".to_string();
    }
    if secs >= 86_400 {
        return format!("{} 天 {} 时", secs / 86_400, (secs % 86_400) / 3_600);
    }
    if secs >= 3_600 {
        return format!("{} 时 {} 分", secs / 3_600, (secs % 3_600) / 60);
    }
    if secs >= 60 {
        return format!("{} 分 {} 秒", secs / 60, secs % 60);
    }
    format!("{secs} 秒")
}

fn log_color(line: &str) -> Color32 {
    if line.starts_with("失败") || line.contains("错误") {
        RED
    } else if line.starts_with("完成") || line.contains("已保存") {
        GREEN
    } else if line.starts_with("已取消") || line.starts_with("正在取消") || line.contains("警告")
    {
        AMBER
    } else if line.starts_with("开始") || line.starts_with("已添加") {
        ACCENT
    } else {
        MUTED
    }
}

fn task_label(spec: &SourceSpec, kind: SourceKind) -> String {
    if kind == SourceKind::Torrent
        && spec.source.starts_with("magnet:")
        && let Some(hash) = spec
            .source
            .split("btih:")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
    {
        let short: String = hash.chars().take(8).collect();
        if !short.is_empty() {
            return format!("磁力任务 {short}");
        }
    }
    util::source_label(&spec.source)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\NotoSansSC-VF.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_string(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_string());
    }
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD;
    style.visuals.extreme_bg_color = Color32::from_rgb(9, 14, 25);
    style.visuals.faint_bg_color = STRIPE;
    style.visuals.override_text_color = Some(Color32::from_rgb(216, 226, 240));
    style.visuals.widgets.noninteractive.bg_fill = CARD;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(27, 37, 56);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(24, 33, 50);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(22, 33, 52);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(33, 46, 69);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(42, 62, 92);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(38, 55, 82);
    style.visuals.selection.bg_fill = SELECTED_BG;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.hyperlink_color = ACCENT;
    style.spacing.item_spacing = vec2(8.0, 6.0);
    style.spacing.button_padding = vec2(10.0, 4.0);
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(12.5));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(11.5));
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
}

fn load_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .to_rgba8();
    let small = image::imageops::resize(&image, 128, 128, image::imageops::FilterType::Lanczos3);
    let color = egui::ColorImage::from_rgba_unmultiplied([128, 128], small.as_raw());
    Some(ctx.load_texture("crazydown-icon", color, egui::TextureOptions::LINEAR))
}

fn load_window_icon() -> Option<Arc<egui::IconData>> {
    let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .to_rgba8();
    let small = image::imageops::resize(&image, 128, 128, image::imageops::FilterType::Lanczos3);
    Some(Arc::new(egui::IconData {
        rgba: small.into_raw(),
        width: 128,
        height: 128,
    }))
}

fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("explorer");
        if path.is_file() {
            command.arg("/select,").arg(path);
        } else {
            command.arg(path);
        }
        command.spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_labels_include_magnet_prefix() {
        let spec = SourceSpec {
            source: "magnet:?xt=urn:btih:0123456789abcdef&dn=test".to_string(),
            output: None,
        };
        assert_eq!(task_label(&spec, SourceKind::Torrent), "磁力任务 01234567");
    }

    #[test]
    fn empty_ftp_credentials_are_omitted() {
        assert_eq!(non_empty("  "), None);
        assert_eq!(non_empty(" user "), Some("user".to_string()));
    }

    #[test]
    fn cancelled_tasks_belong_to_failed_group() {
        let spec = SourceSpec {
            source: "https://example.com/file.zip".to_string(),
            output: None,
        };
        let mut task = GuiTask::new(1, spec, SourceKind::Http, PathBuf::from("."));
        task.status = TaskStatus::Cancelled;
        assert!(TaskFilter::Failed.matches(&task));
        assert!(!TaskFilter::Completed.matches(&task));
        assert!(!TaskFilter::Pending.matches(&task));
    }

    #[test]
    fn eta_format_is_compact() {
        assert_eq!(format_eta(5.2), "6 秒");
        assert_eq!(format_eta(65.0), "1 分 5 秒");
        assert_eq!(format_eta(3_700.0), "1 时 1 分");
        assert_eq!(format_eta(90_000.0), "1 天 1 时");
    }
}
