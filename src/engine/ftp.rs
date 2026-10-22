use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;
use percent_encoding::percent_decode_str;
use suppaftp::types::{FileType, Mode};
use suppaftp::{FtpError, FtpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::cli::SourceSpec;
use crate::engine::Options;
use crate::engine::output::OutputRegistry;
use crate::engine::reserve_output;
use crate::engine::segments::{self, Segment};
use crate::engine::state::{self, DownloadState, StateMode};
use crate::ui::{self, Ui};
use crate::util;

#[derive(Clone)]
struct Credentials {
    user: String,
    pass: String,
}

#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    remote_path: String,
}

struct Probe {
    total: Option<u64>,
    modified: Option<String>,
    rest: bool,
}

#[derive(Clone)]
struct SegmentContext {
    target: Target,
    creds: Credentials,
    timeout: Duration,
    retries: usize,
    file: Arc<File>,
    bar: ProgressBar,
    cancel: CancellationToken,
}

enum SegmentError {
    Cancelled,
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

enum StreamError {
    Cancelled,
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

pub async fn download(
    opts: &Options,
    registry: &OutputRegistry,
    spec: &SourceSpec,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let url = Url::parse(&spec.source).context("无效的 FTP 地址")?;
    if url.scheme() != "ftp" {
        bail!("不是 FTP 地址");
    }
    let target = parse_target(&url)?;
    let creds = credentials(&url, opts);
    let suggested = remote_filename(&target.remote_path).unwrap_or_else(|| "download".into());
    let output = reserve_output(registry, opts, spec.output.clone(), &suggested)?.path;
    let state_path = state::state_path(&output);

    if opts.force {
        state::remove_if_exists(&state_path).ok();
    }
    let resume_state = if opts.force {
        None
    } else {
        state::load_if_exists(&state_path).with_context(|| {
            format!(
                "读取断点信息失败；如要放弃旧断点，请加 --force（{}）",
                output.display()
            )
        })?
    };

    let probe_target = target.clone();
    let probe_creds = creds.clone();
    let timeout = opts.timeout;
    let probe =
        tokio::task::spawn_blocking(move || probe_blocking(&probe_target, &probe_creds, timeout))
            .await
            .context("FTP 探测任务异常")??;

    if resume_state.is_some() && !probe.rest {
        bail!(
            "发现未完成的下载，但服务器当前不支持 REST 续传。\
             请使用 --force 重新下载，或更换支持断点续传的服务器"
        );
    }

    match probe.total {
        Some(0) => {
            tokio::fs::File::create(&output)
                .await
                .with_context(|| format!("无法创建文件 {}", output.display()))?;
            state::remove_if_exists(&state_path)?;
            bar.set_message("空文件");
            Ok(output)
        }
        Some(total) if probe.rest => {
            download_segmented(
                opts,
                ui,
                bar,
                cancel,
                target,
                creds,
                probe.modified,
                total,
                spec,
                output,
                state_path,
                resume_state,
            )
            .await
        }
        total => {
            if resume_state.is_some() {
                bail!("断点信息与当前下载模式不兼容，请使用 --force 重新下载");
            }
            download_stream(
                opts, ui, bar, cancel, target, creds, total, output, state_path,
            )
            .await
        }
    }
}

fn parse_target(url: &Url) -> Result<Target> {
    let host = url.host_str().context("FTP 地址缺少主机名")?.to_string();
    let port = url.port().unwrap_or(21);
    let raw_path = url.path();
    let raw_path = raw_path
        .rsplit_once(";type=")
        .map(|(path, _)| path)
        .unwrap_or(raw_path);
    let remote_path = percent_decode_str(raw_path).decode_utf8_lossy().to_string();
    if remote_path.is_empty() || remote_path == "/" {
        bail!("FTP 地址缺少远程文件路径");
    }
    Ok(Target {
        host,
        port,
        remote_path,
    })
}

fn remote_filename(remote_path: &str) -> Option<String> {
    let name = remote_path
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())?;
    util::sanitize_filename(name)
}

fn credentials(url: &Url, opts: &Options) -> Credentials {
    let url_user = (!url.username().is_empty()).then(|| {
        percent_decode_str(url.username())
            .decode_utf8_lossy()
            .to_string()
    });
    let url_pass = url
        .password()
        .map(|value| percent_decode_str(value).decode_utf8_lossy().to_string());

    let user = url_user
        .or_else(|| opts.ftp_user.clone())
        .unwrap_or_else(|| "anonymous".to_string());
    let pass = url_pass
        .or_else(|| opts.ftp_pass.clone())
        .unwrap_or_else(|| {
            if opts.ftp_user.is_none() && url.username().is_empty() {
                "anonymous@".to_string()
            } else {
                String::new()
            }
        });
    Credentials { user, pass }
}

fn connect(target: &Target, creds: &Credentials, timeout: Duration) -> Result<FtpStream> {
    let addrs: Vec<_> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .with_context(|| format!("无法解析 FTP 主机 {}", target.host))?
        .collect();
    if addrs.is_empty() {
        bail!("FTP 主机 {} 没有可用地址", target.host);
    }

    let mut last_error = None;
    let mut control = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                if let Err(error) = stream.set_read_timeout(Some(timeout)) {
                    last_error = Some(anyhow!(error));
                    continue;
                }
                let _ = stream.set_write_timeout(Some(timeout));
                control = Some(stream);
                break;
            }
            Err(error) => last_error = Some(anyhow!(error)),
        }
    }
    let control = control.ok_or_else(|| {
        anyhow!(
            "无法连接 FTP 服务器 {}:{}: {}",
            target.host,
            target.port,
            last_error
                .map(|error| error.to_string())
                .unwrap_or_default()
        )
    })?;

    let mut ftp = FtpStream::connect_with_stream(control).context("FTP 服务器握手失败")?;
    ftp = ftp.passive_stream_builder(move |addr| {
        let stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(FtpError::ConnectionError)?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(FtpError::ConnectionError)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(FtpError::ConnectionError)?;
        Ok(stream)
    });
    ftp.login(creds.user.as_str(), creds.pass.as_str())
        .context("FTP 登录失败（请检查用户名、密码或是否允许匿名访问）")?;
    ftp.transfer_type(FileType::Binary)
        .context("无法将 FTP 传输模式设置为二进制")?;

    let control_is_ipv6 = ftp
        .get_ref()
        .peer_addr()
        .map(|address| address.is_ipv6())
        .unwrap_or(false);
    if control_is_ipv6 {
        // PASV only carries IPv4 addresses; IPv6 data connections require EPSV.
        ftp.set_mode(Mode::ExtendedPassive);
    } else {
        ftp.set_passive_nat_workaround(true);
    }

    Ok(ftp)
}

fn probe_blocking(target: &Target, creds: &Credentials, timeout: Duration) -> Result<Probe> {
    let mut ftp = connect(target, creds, timeout)?;
    let total = ftp.size(&target.remote_path).ok().map(|size| size as u64);
    let modified = ftp
        .mdtm(&target.remote_path)
        .ok()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string());
    let rest = match total {
        Some(total) if total > 1 => probe_rest(&mut ftp, &target.remote_path, timeout),
        _ => false,
    };
    let _ = ftp.quit();
    Ok(Probe {
        total,
        modified,
        rest,
    })
}

fn probe_rest(ftp: &mut FtpStream, remote_path: &str, timeout: Duration) -> bool {
    if ftp.resume_transfer(1).is_err() {
        return false;
    }
    let Ok(mut stream) = ftp.retr_as_stream(remote_path) else {
        return false;
    };
    let _ = stream
        .get_ref()
        .get_ref()
        .set_read_timeout(Some(timeout.min(Duration::from_secs(5))));
    let mut byte = [0_u8; 1];
    let supported = matches!(stream.read(&mut byte), Ok(1));
    let _ = stream.finish();
    supported
}

#[allow(clippy::too_many_arguments)]
async fn download_segmented(
    opts: &Options,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
    target: Target,
    creds: Credentials,
    modified: Option<String>,
    total: u64,
    spec: &SourceSpec,
    output: PathBuf,
    state_path: PathBuf,
    mut resume_state: Option<DownloadState>,
) -> Result<PathBuf> {
    if let Some(saved) = &resume_state {
        if saved.source_key != state::source_key(&spec.source) {
            bail!("断点文件属于另一个下载任务，请使用 --force 或指定其他输出文件名");
        }
        if saved.total != total {
            bail!(
                "服务器文件大小已变化（原 {} 字节，现 {total} 字节），无法续传；请使用 --force 重新下载",
                saved.total
            );
        }
        if saved.mode != StateMode::Segmented {
            bail!("断点信息与当前下载模式不兼容，请使用 --force 重新下载");
        }
        match (&saved.last_modified, &modified) {
            (Some(saved_time), Some(current)) if saved_time != current => {
                bail!(
                    "服务器文件已更新（MDTM 修改时间不一致），无法安全续传；请使用 --force 重新下载"
                );
            }
            (Some(_), None) => {
                ui.warn("服务器未返回 MDTM，续传只能按文件大小校验，存在内容变化风险");
            }
            (None, _) => {
                ui.warn("断点缺少修改时间校验信息，续传只能按文件大小校验，存在内容变化风险");
            }
            _ => {}
        }

        let valid_file = std::fs::metadata(&output)
            .ok()
            .is_some_and(|metadata| metadata.is_file() && metadata.len() == total);
        if !valid_file {
            ui.warn("断点数据不完整，将重新下载该文件");
            state::remove_if_exists(&state_path)?;
            resume_state = None;
        }
    }

    if resume_state.is_none() && output.exists() && !opts.force {
        bail!(
            "目标文件已存在且没有可用断点：{}。使用 --force 覆盖",
            output.display()
        );
    }

    let file = if resume_state.is_some() {
        OpenOptions::new().read(true).write(true).open(&output)
    } else {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output)
    }
    .with_context(|| format!("无法打开输出文件 {}", output.display()))?;
    file.set_len(total)
        .with_context(|| format!("无法预分配文件大小 {}", output.display()))?;
    let file = Arc::new(file);

    let segments = if let Some(saved) = &resume_state {
        ui.log(format!(
            "发现断点，继续下载: {}（已完成 {}/{}）",
            output.display(),
            util::format_bytes(segments::total_done(&segments::from_state(saved))),
            util::format_bytes(total)
        ));
        segments::from_state(saved)
    } else {
        segments::new_segments(total, opts.segments)
    };
    let initial_done = segments::total_done(&segments);

    let template = DownloadState::new(
        &spec.source,
        total,
        StateMode::Segmented,
        None,
        modified.clone(),
    );
    segments::save_snapshot(&state_path, &template, &segments)
        .with_context(|| "无法写入断点信息，下载未开始")?;

    ui::set_total(&bar, total);
    bar.set_position(initial_done);
    if initial_done > 0 {
        bar.set_message("继续下载");
    } else {
        bar.set_message(format!("{} 连接", segments.len()));
    }

    if initial_done >= total {
        state::remove_if_exists(&state_path)?;
        bar.set_position(total);
        return Ok(output);
    }

    let segment_cancel = cancel.child_token();
    let context = SegmentContext {
        target,
        creds,
        timeout: opts.timeout,
        retries: opts.retries,
        file: file.clone(),
        bar: bar.clone(),
        cancel: segment_cancel.clone(),
    };

    let saver_stop = CancellationToken::new();
    let saver = segments::spawn_saver(
        state_path.clone(),
        template.clone(),
        segments.clone(),
        saver_stop.clone(),
    );

    let mut tasks = JoinSet::new();
    for segment in segments
        .iter()
        .filter(|segment| !segment.completed())
        .cloned()
    {
        let context = context.clone();
        let start = segment.start;
        let end = segment.end;
        tasks.spawn_blocking(move || {
            run_segment(context, segment).with_context(|| format!("分段 {start}-{end} 下载失败"))
        });
    }

    let mut first_error: Option<anyhow::Error> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                segment_cancel.cancel();
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => {
                segment_cancel.cancel();
                if first_error.is_none() {
                    first_error = Some(anyhow!("下载任务异常退出: {error}"));
                }
            }
        }
    }

    saver_stop.cancel();
    let _ = saver.await;

    let all_complete = segments.iter().all(|segment| segment.completed());
    if !all_complete {
        let saved = segments::save_snapshot(&state_path, &template, &segments).is_ok();
        if cancel.is_cancelled() {
            if saved {
                bail!("已中断，断点已保存；重新运行相同命令即可续传");
            }
            bail!("已中断，但断点保存失败；请检查磁盘权限或空间");
        }
        let suffix = if saved {
            "断点已保存，重新运行相同命令即可续传"
        } else {
            "断点保存失败，请检查磁盘权限或空间"
        };
        return Err(match first_error {
            Some(error) => error.context(suffix),
            None => anyhow!("下载未完成：{suffix}"),
        });
    }

    if let Err(error) = tokio::task::spawn_blocking({
        let file = file.clone();
        move || file.sync_all()
    })
    .await
    .context("同步文件失败")?
    {
        ui.warn(format!("文件同步失败: {error}"));
    }

    let actual_size = std::fs::metadata(&output)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if actual_size != total {
        bail!("下载完成但文件大小异常（期望 {total}，实际 {actual_size}）");
    }

    if let Err(error) = state::remove_if_exists(&state_path) {
        ui.warn(format!("下载完成，但无法删除断点文件: {error:#}"));
    }
    bar.set_position(total);
    Ok(output)
}

fn run_segment(context: SegmentContext, segment: Arc<Segment>) -> Result<()> {
    let mut consecutive_failures = 0_usize;
    loop {
        if context.cancel.is_cancelled() {
            bail!("已取消");
        }
        if segment.completed() {
            return Ok(());
        }
        let before = segment.done.load(Ordering::Relaxed);
        match fetch_segment(&context, &segment) {
            Ok(()) => return Ok(()),
            Err(SegmentError::Cancelled) => bail!("已取消"),
            Err(SegmentError::Fatal(error)) => return Err(error),
            Err(SegmentError::Retry(error)) => {
                if segment.done.load(Ordering::Relaxed) > before {
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                }
                if consecutive_failures > context.retries {
                    bail!(
                        "连续失败 {} 次（重试上限 {}）: {error:#}",
                        consecutive_failures,
                        context.retries
                    );
                }
                context
                    .bar
                    .set_message(format!("重试 {consecutive_failures}/{}", context.retries));
                let factor = 1_u64 << consecutive_failures.saturating_sub(1).min(4);
                let deadline = Instant::now() + Duration::from_millis(500 * factor);
                while Instant::now() < deadline {
                    if context.cancel.is_cancelled() {
                        bail!("已取消");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn fetch_segment(context: &SegmentContext, segment: &Arc<Segment>) -> Result<(), SegmentError> {
    let offset = segment.offset();
    if offset > segment.end {
        return Ok(());
    }

    let mut ftp =
        connect(&context.target, &context.creds, context.timeout).map_err(SegmentError::Retry)?;
    let offset_usize = usize::try_from(offset)
        .map_err(|_| SegmentError::Fatal(anyhow!("FTP 续传偏移超出平台限制")))?;
    ftp.resume_transfer(offset_usize)
        .map_err(|error| SegmentError::Retry(error.into()))?;
    let mut stream = ftp
        .retr_as_stream(&context.target.remote_path)
        .map_err(|error| SegmentError::Retry(error.into()))?;

    let poll_timeout = context.timeout.min(Duration::from_secs(1));
    stream
        .get_ref()
        .get_ref()
        .set_read_timeout(Some(poll_timeout))
        .map_err(|error| SegmentError::Retry(error.into()))?;

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut last_data = Instant::now();
    loop {
        if context.cancel.is_cancelled() {
            let _ = stream.finish();
            return Err(SegmentError::Cancelled);
        }
        let done = segment.done.load(Ordering::Relaxed);
        let remaining = segment.length().saturating_sub(done);
        if remaining == 0 {
            break;
        }
        let want = remaining.min(buffer.len() as u64) as usize;
        match stream.read(&mut buffer[..want]) {
            Ok(0) => {
                let _ = stream.finish();
                return Err(SegmentError::Retry(anyhow!("FTP 数据连接提前关闭")));
            }
            Ok(read) => {
                let position = segment.start + done;
                if let Err(error) = segments::write_all_at(&context.file, &buffer[..read], position)
                {
                    let _ = stream.finish();
                    return Err(SegmentError::Fatal(error.into()));
                }
                segment.done.fetch_add(read as u64, Ordering::Relaxed);
                context.bar.inc(read as u64);
                last_data = Instant::now();
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if last_data.elapsed() > context.timeout {
                    let _ = stream.finish();
                    return Err(SegmentError::Retry(anyhow!("读取数据超时")));
                }
            }
            Err(error) => {
                let _ = stream.finish();
                return Err(SegmentError::Retry(error.into()));
            }
        }
    }

    let _ = stream.finish();
    if segment.completed() {
        Ok(())
    } else {
        Err(SegmentError::Retry(anyhow!("数据流提前结束")))
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_stream(
    opts: &Options,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
    target: Target,
    creds: Credentials,
    total: Option<u64>,
    output: PathBuf,
    state_path: PathBuf,
) -> Result<PathBuf> {
    state::remove_if_exists(&state_path)?;
    if output.exists() && !opts.force {
        bail!(
            "目标文件已存在且没有可用断点：{}。使用 --force 覆盖",
            output.display()
        );
    }
    ui.warn("服务器不支持 REST 续传或无法获取文件大小，将使用单连接下载（无法断点续传）");
    bar.set_message("单连接");

    let temp_path = part_path(&output);
    let mut failures = 0_usize;
    loop {
        if cancel.is_cancelled() {
            remove_quiet(&temp_path);
            bail!("已取消");
        }
        match stream_attempt(opts, &target, &creds, total, &temp_path, &bar, cancel).await {
            Ok(()) => break,
            Err(StreamError::Cancelled) => {
                remove_quiet(&temp_path);
                bail!("已取消");
            }
            Err(StreamError::Fatal(error)) => {
                remove_quiet(&temp_path);
                return Err(error);
            }
            Err(StreamError::Retry(error)) => {
                failures += 1;
                if failures > opts.retries {
                    remove_quiet(&temp_path);
                    bail!("下载失败（已重试 {} 次）: {error:#}", opts.retries);
                }
                ui.warn(format!(
                    "FTP 连接中断，将重新下载（第 {failures}/{} 次重试）: {error:#}",
                    opts.retries
                ));
                bar.set_position(0);
                tokio::select! {
                    _ = cancel.cancelled() => {
                        remove_quiet(&temp_path);
                        bail!("已取消");
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }

    if output.exists() {
        if opts.force {
            std::fs::remove_file(&output)
                .with_context(|| format!("无法覆盖文件 {}", output.display()))?;
        } else {
            bail!("目标文件已在下载期间被其他程序创建：{}", output.display());
        }
    }
    std::fs::rename(&temp_path, &output)
        .with_context(|| format!("无法将临时文件重命名为 {}", output.display()))?;
    Ok(output)
}

async fn stream_attempt(
    opts: &Options,
    target: &Target,
    creds: &Credentials,
    total: Option<u64>,
    temp_path: &Path,
    bar: &ProgressBar,
    cancel: &CancellationToken,
) -> Result<(), StreamError> {
    if let Some(total) = total {
        ui::set_total(bar, total);
    } else {
        ui::set_unknown(bar);
    }
    bar.set_position(0);

    let target = target.clone();
    let creds = creds.clone();
    let timeout = opts.timeout;
    let temp_path = temp_path.to_path_buf();
    let bar = bar.clone();
    let cancel = cancel.clone();

    tokio::task::spawn_blocking(move || {
        stream_attempt_blocking(&target, &creds, total, &temp_path, &bar, &cancel, timeout)
    })
    .await
    .context("FTP 下载任务异常")
    .map_err(StreamError::Fatal)?
}

fn stream_attempt_blocking(
    target: &Target,
    creds: &Credentials,
    total: Option<u64>,
    temp_path: &Path,
    bar: &ProgressBar,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<(), StreamError> {
    let mut file = File::create(temp_path).map_err(|error| StreamError::Fatal(error.into()))?;
    let mut ftp = connect(target, creds, timeout).map_err(StreamError::Retry)?;
    let mut stream = ftp
        .retr_as_stream(&target.remote_path)
        .map_err(|error| StreamError::Retry(error.into()))?;

    let poll_timeout = timeout.min(Duration::from_secs(1));
    stream
        .get_ref()
        .get_ref()
        .set_read_timeout(Some(poll_timeout))
        .map_err(|error| StreamError::Retry(error.into()))?;

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut written = 0_u64;
    let mut last_data = Instant::now();
    loop {
        if cancel.is_cancelled() {
            drop(stream);
            return Err(StreamError::Cancelled);
        }
        if total.is_some_and(|total| written >= total) {
            break;
        }
        let want = total
            .map(|total| (total - written).min(buffer.len() as u64) as usize)
            .unwrap_or(buffer.len());
        match stream.read(&mut buffer[..want]) {
            Ok(0) => break,
            Ok(read) => {
                file.write_all(&buffer[..read])
                    .map_err(|error| StreamError::Fatal(error.into()))?;
                written += read as u64;
                bar.inc(read as u64);
                last_data = Instant::now();
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if last_data.elapsed() > timeout {
                    return Err(StreamError::Retry(anyhow!("读取数据超时")));
                }
            }
            Err(error) => return Err(StreamError::Retry(error.into())),
        }
    }
    let _ = stream.finish();

    file.flush()
        .map_err(|error| StreamError::Fatal(error.into()))?;
    drop(file);

    if let Some(total) = total
        && written != total
    {
        return Err(StreamError::Retry(anyhow!(
            "收到的字节数不完整（{written}/{total}）"
        )));
    }
    Ok(())
}

fn part_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(".crazydown.part");
    PathBuf::from(name)
}

fn remove_quiet(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        eprintln!("警告: 无法删除临时文件 {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ftp_target_and_name() {
        let url =
            Url::parse("ftp://alice:secret@example.com:2121/pub/My%20File.iso;type=i").unwrap();
        let target = parse_target(&url).unwrap();
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 2121);
        assert_eq!(target.remote_path, "/pub/My File.iso");
        assert_eq!(
            remote_filename(&target.remote_path).as_deref(),
            Some("My File.iso")
        );
        assert_eq!(credentials(&url, &test_options()).user, "alice");
        assert_eq!(credentials(&url, &test_options()).pass, "secret");
    }

    #[test]
    fn defaults_to_anonymous_login() {
        let url = Url::parse("ftp://example.com/file.zip").unwrap();
        let creds = credentials(&url, &test_options());
        assert_eq!(creds.user, "anonymous");
        assert_eq!(creds.pass, "anonymous@");
    }

    fn test_options() -> Options {
        Options {
            dir: PathBuf::from("."),
            jobs: 1,
            segments: 1,
            retries: 0,
            timeout: Duration::from_secs(1),
            ftp_user: None,
            ftp_pass: None,
            force: false,
            insecure: false,
            quiet: true,
            no_progress: true,
            file_type_rules: Vec::new(),
        }
    }
}
