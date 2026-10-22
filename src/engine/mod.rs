pub mod ftp;
pub mod http;
pub(crate) mod output;
pub(crate) mod segments;
pub(crate) mod state;
pub mod torrent;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::cli::SourceSpec;
use crate::ui::Ui;
use crate::util;

use output::{OutputPlan, OutputRegistry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTypeRule {
    pub name: String,
    pub extensions: String,
    pub dir: String,
}

impl FileTypeRule {
    pub fn matches(&self, filename: &str) -> bool {
        let extension = file_extension(filename);
        self.extensions
            .split([',', ';', ' ', '\t', '\n', '\r'])
            .map(|token| token.trim().trim_start_matches('.').to_ascii_lowercase())
            .any(|token| token == "*" || (!token.is_empty() && token == extension))
    }
}

pub fn default_file_type_rules() -> Vec<FileTypeRule> {
    let rule = |name: &str, extensions: &str, dir: &str| FileTypeRule {
        name: name.into(),
        extensions: extensions.into(),
        dir: dir.into(),
    };
    vec![
        rule(
            "视频",
            "mp4 mkv avi mov flv wmv webm m4v mpg mpeg ts",
            "视频",
        ),
        rule("音频", "mp3 flac wav aac ogg m4a wma opus", "音频"),
        rule(
            "图片",
            "jpg jpeg png gif webp bmp svg tiff heic ico",
            "图片",
        ),
        rule(
            "文档",
            "pdf doc docx xls xlsx ppt pptx txt md epub mobi azw3",
            "文档",
        ),
        rule("压缩包", "zip rar 7z tar gz bz2 xz zst iso img", "压缩包"),
        rule("程序", "exe msi apk deb rpm dmg appimage", "程序"),
        rule("种子", "torrent", "种子"),
        rule("其他", "*", ""),
    ]
}

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub jobs: usize,
    pub segments: usize,
    pub retries: usize,
    pub timeout: Duration,
    pub ftp_user: Option<String>,
    pub ftp_pass: Option<String>,
    pub force: bool,
    pub insecure: bool,
    pub quiet: bool,
    pub no_progress: bool,
    pub file_type_rules: Vec<FileTypeRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Http,
    Ftp,
    Torrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub struct JobReport {
    pub id: usize,
    pub source: String,
    pub output: Option<PathBuf>,
    pub status: JobStatus,
    pub error: Option<String>,
    pub elapsed: Duration,
    pub bytes: Option<u64>,
}

#[derive(Debug)]
pub struct RunSummary {
    pub reports: Vec<JobReport>,
    pub unstarted: usize,
    pub elapsed: Duration,
}

impl RunSummary {
    pub fn successes(&self) -> usize {
        self.reports
            .iter()
            .filter(|report| report.status == JobStatus::Completed)
            .count()
    }

    pub fn failures(&self) -> usize {
        self.reports
            .iter()
            .filter(|report| report.status == JobStatus::Failed)
            .count()
    }

    pub fn cancelled(&self) -> usize {
        self.reports
            .iter()
            .filter(|report| report.status == JobStatus::Cancelled)
            .count()
    }
}

struct Engine {
    opts: Options,
    client: Client,
    registry: OutputRegistry,
    session: OnceCell<Arc<librqbit::Session>>,
    ui: Arc<Ui>,
}

pub async fn run(
    specs: Vec<SourceSpec>,
    mut opts: Options,
    cancel: CancellationToken,
) -> Result<RunSummary> {
    let started = Instant::now();
    opts.dir = util::absolutize(&opts.dir);
    std::fs::create_dir_all(&opts.dir)
        .with_context(|| format!("无法创建下载目录 {}", opts.dir.display()))?;

    let mut validated = Vec::with_capacity(specs.len());
    for (index, spec) in specs.into_iter().enumerate() {
        let kind = classify(&spec.source).with_context(|| format!("任务 {}", index + 1))?;
        if matches!(kind, SourceKind::Torrent) && spec.output.is_some() {
            bail!(
                "BT 任务不支持用 -o 或列表文件名指定输出文件，请用 -d 指定保存目录：{}",
                util::display_source(&spec.source)
            );
        }
        validated.push((spec, kind));
    }
    if validated.is_empty() {
        bail!("没有可执行的任务");
    }

    let ui = Arc::new(Ui::new(opts.quiet, opts.no_progress));
    let client = build_http_client(&opts)?;
    let engine = Arc::new(Engine {
        opts,
        client,
        registry: OutputRegistry::new(),
        session: OnceCell::new(),
        ui: ui.clone(),
    });

    if engine.opts.insecure {
        ui.warn("已禁用 HTTPS 证书校验，连接可能被中间人攻击");
    }

    let requested = validated.len();
    let permissions = Arc::new(Semaphore::new(engine.opts.jobs));
    let mut tasks = JoinSet::new();
    let mut scheduled = 0_usize;

    for (index, (spec, kind)) in validated.into_iter().enumerate() {
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            permit = permissions.clone().acquire_owned() => {
                permit.context("任务调度失败")?
            }
        };
        let engine = engine.clone();
        let job_cancel = cancel.child_token();
        let id = index + 1;
        tasks.spawn(async move {
            let _permit = permit;
            engine.run_job(id, requested, spec, kind, job_cancel).await
        });
        scheduled += 1;
    }

    let mut reports = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(report) => reports.push(report),
            Err(error) => {
                ui.log(format!("错误: 下载任务异常退出: {error}"));
            }
        }
    }

    if let Some(session) = engine.session.get() {
        let _ = tokio::time::timeout(Duration::from_secs(5), session.stop()).await;
    }

    reports.sort_by_key(|report| report.id);
    Ok(RunSummary {
        reports,
        unstarted: requested.saturating_sub(scheduled),
        elapsed: started.elapsed(),
    })
}

impl Engine {
    async fn run_job(
        &self,
        id: usize,
        total: usize,
        spec: SourceSpec,
        kind: SourceKind,
        cancel: CancellationToken,
    ) -> JobReport {
        let started = Instant::now();
        let label = format!(
            "[{id}/{total}] {}",
            util::truncate_chars(&util::source_label(&spec.source), 30)
        );
        let bar = self.ui.add_bar(&label);
        self.ui.log(format!(
            "开始 [{id}] {}",
            util::display_source(&spec.source)
        ));

        let result = match kind {
            SourceKind::Http => {
                http::download(
                    &self.client,
                    &self.opts,
                    &self.registry,
                    &spec,
                    &self.ui,
                    bar.clone(),
                    &cancel,
                )
                .await
            }
            SourceKind::Ftp => {
                ftp::download(
                    &self.opts,
                    &self.registry,
                    &spec,
                    &self.ui,
                    bar.clone(),
                    &cancel,
                )
                .await
            }
            SourceKind::Torrent => {
                torrent::download(
                    &spec.source,
                    &self.opts.dir,
                    &self.session,
                    &self.ui,
                    &bar,
                    &cancel,
                    self.opts.force,
                )
                .await
            }
        };

        match result {
            Ok(path) => {
                let bytes = std::fs::metadata(&path)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .map(|metadata| metadata.len());
                let elapsed = started.elapsed();
                bar.finish_with_message("完成");
                let size_text = bytes
                    .map(|value| format!("，{}", util::format_bytes(value)))
                    .unwrap_or_default();
                self.ui.log(format!(
                    "完成 [{id}] {} -> {}{size_text}，用时 {}",
                    util::source_label(&spec.source),
                    path.display(),
                    util::format_duration(elapsed)
                ));
                JobReport {
                    id,
                    source: util::display_source(&spec.source),
                    output: Some(path),
                    status: JobStatus::Completed,
                    error: None,
                    elapsed,
                    bytes,
                }
            }
            Err(error) => {
                let elapsed = started.elapsed();
                let message = format!("{error:#}");
                let cancelled = cancel.is_cancelled();
                let (status, bar_message, log_prefix) = if cancelled {
                    (JobStatus::Cancelled, "已中断", "已中断")
                } else {
                    (JobStatus::Failed, "失败", "失败")
                };
                self.ui.fail_bar(&bar, bar_message);
                self.ui.log(format!(
                    "{log_prefix} [{id}] {}: {}",
                    util::display_source(&spec.source),
                    message
                ));
                JobReport {
                    id,
                    source: util::display_source(&spec.source),
                    output: None,
                    status,
                    error: Some(message),
                    elapsed,
                    bytes: None,
                }
            }
        }
    }
}

pub(crate) fn build_http_client(opts: &Options) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(format!("CrazyDown/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(opts.timeout)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .pool_max_idle_per_host(opts.segments.max(4));
    if opts.insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("无法创建 HTTP 客户端")
}

pub(crate) fn classify(source: &str) -> Result<SourceKind> {
    let source = source.trim();
    if source.is_empty() {
        bail!("下载地址为空");
    }
    if source.starts_with("magnet:") {
        return Ok(SourceKind::Torrent);
    }

    let path = Path::new(source);
    if path.exists() {
        let is_torrent = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"));
        if is_torrent {
            return Ok(SourceKind::Torrent);
        }
        bail!(
            "本地文件只支持 .torrent 种子文件：{}",
            util::display_source(source)
        );
    }

    if let Ok(url) = Url::parse(source) {
        return match url.scheme() {
            "http" | "https" => Ok(SourceKind::Http),
            "ftp" => Ok(SourceKind::Ftp),
            "magnet" => Ok(SourceKind::Torrent),
            scheme => bail!("不支持的协议: {scheme}"),
        };
    }

    if source.to_ascii_lowercase().ends_with(".torrent") {
        bail!("种子文件不存在：{}", path.display());
    }
    bail!("无法识别的下载地址：{}", util::display_source(source))
}

pub(crate) fn reserve_output(
    registry: &OutputRegistry,
    opts: &Options,
    explicit: Option<PathBuf>,
    suggested: &str,
) -> Result<OutputPlan> {
    let dir = if explicit.is_none() && !opts.file_type_rules.is_empty() {
        Cow::Owned(type_dir(&opts.dir, &opts.file_type_rules, suggested))
    } else {
        Cow::Borrowed(&opts.dir)
    };
    registry.choose(&dir, suggested, explicit, opts.force)
}

fn type_dir(base: &Path, rules: &[FileTypeRule], filename: &str) -> PathBuf {
    for rule in rules {
        if !rule.matches(filename) {
            continue;
        }
        let dir = rule.dir.trim();
        if dir.is_empty() {
            return base.to_path_buf();
        }
        let path = Path::new(dir);
        return if path.is_absolute() {
            util::absolutize(path)
        } else {
            util::absolutize(&base.join(path))
        };
    }
    base.to_path_buf()
}

fn file_extension(filename: &str) -> String {
    let name = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(filename);
    match name.rfind('.') {
        Some(index) if index > 0 => name[index + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_supported_sources() {
        assert!(matches!(
            classify("https://example.com/file.zip").unwrap(),
            SourceKind::Http
        ));
        assert!(matches!(
            classify("ftp://example.com/file.zip").unwrap(),
            SourceKind::Ftp
        ));
        assert!(matches!(
            classify("magnet:?xt=urn:btih:abcdef").unwrap(),
            SourceKind::Torrent
        ));
        assert!(classify("gopher://example.com/file").is_err());
        assert!(classify("not a url").is_err());
    }

    #[test]
    fn routes_known_file_types_to_rule_directories() {
        let base = util::absolutize(Path::new("routes"));
        let rules = default_file_type_rules();
        assert_eq!(type_dir(&base, &rules, "movie.MP4"), base.join("视频"));
        assert_eq!(
            type_dir(&base, &rules, "archive.tar.gz"),
            base.join("压缩包")
        );
        assert_eq!(type_dir(&base, &rules, "notes"), base);
    }

    #[test]
    fn custom_file_type_rule_overrides_defaults() {
        let base = util::absolutize(Path::new("custom-routes"));
        let mut rules = default_file_type_rules();
        rules.insert(
            0,
            FileTypeRule {
                name: "安装包".into(),
                extensions: ".exe, msi".into(),
                dir: "Installers".into(),
            },
        );
        assert_eq!(
            type_dir(&base, &rules, "setup.exe"),
            base.join("Installers")
        );
    }
}
