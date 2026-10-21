use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use indicatif::ProgressBar;
use percent_encoding::percent_decode_str;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE,
    LAST_MODIFIED, RANGE,
};
use reqwest::{Client, RequestBuilder, StatusCode};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::cli::SourceSpec;
use crate::engine::output::OutputRegistry;
use crate::engine::segments::{self, Segment};
use crate::engine::state::{self, DownloadState, StateMode};
use crate::engine::{Options, reserve_output};
use crate::ui::{self, Ui};
use crate::util;

#[derive(Debug, Clone)]
struct BasicAuth {
    user: String,
    password: String,
}

#[derive(Debug, Clone)]
struct Probe {
    total: Option<u64>,
    ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    suggested_name: Option<String>,
}

#[derive(Clone)]
struct SegmentContext {
    client: Client,
    url: Url,
    auth: Option<BasicAuth>,
    total: u64,
    retries: usize,
    timeout: Duration,
    validator: Option<String>,
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
    client: &Client,
    opts: &Options,
    registry: &OutputRegistry,
    spec: &SourceSpec,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let mut url = Url::parse(&spec.source).context("无效的 HTTP 地址")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("不是 HTTP/HTTPS 地址");
    }
    let auth = extract_basic_auth(&mut url);

    let probe = probe(client, &url, auth.as_ref(), opts, cancel).await?;
    let suggested = probe
        .suggested_name
        .clone()
        .unwrap_or_else(|| "download".to_string());
    let output = reserve_output(registry, opts, spec.output.clone(), &suggested)?.path;
    let state_path = state::state_path(&output);

    if opts.force {
        state::remove_if_exists(&state_path).ok();
    }

    let mut resume_state = if opts.force {
        None
    } else {
        state::load_if_exists(&state_path).with_context(|| {
            format!(
                "读取断点信息失败；如要放弃旧断点，请加 --force（{}）",
                output.display()
            )
        })?
    };

    if resume_state.is_some() && (!probe.ranges || probe.total.is_none()) {
        bail!(
            "发现未完成的下载，但服务器当前不支持 Range 请求，无法安全续传。\
             请使用 --force 重新下载，或更换支持断点续传的镜像"
        );
    }

    if let Some(state) = &resume_state {
        if state.mode != StateMode::Segmented {
            bail!("断点信息与当前下载模式不兼容，请使用 --force 重新下载");
        }
        if !output.exists() {
            ui.warn("断点文件存在但部分下载文件已丢失，将重新下载");
            state::remove_if_exists(&state_path)?;
            resume_state = None;
        }
    }

    if probe.total == Some(0) {
        tokio::fs::File::create(&output)
            .await
            .with_context(|| format!("无法创建文件 {}", output.display()))?;
        state::remove_if_exists(&state_path)?;
        bar.set_message("空文件");
        return Ok(output);
    }

    if probe.ranges && probe.total.is_some_and(|total| total > 0) {
        download_segmented(
            client,
            opts,
            spec,
            ui,
            bar,
            cancel,
            probe,
            auth,
            url,
            output,
            state_path,
            resume_state,
        )
        .await
    } else {
        download_stream(
            client, opts, spec, ui, bar, cancel, probe, auth, url, output, state_path,
        )
        .await
    }
}

async fn probe(
    client: &Client,
    url: &Url,
    auth: Option<&BasicAuth>,
    opts: &Options,
    cancel: &CancellationToken,
) -> Result<Probe> {
    let request = with_auth(client.get(url.clone()), auth)
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .timeout(opts.timeout);
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("已取消"),
        result = request.send() => result.context("连接服务器失败")?,
    };

    let status = response.status();
    let final_url = response.url().clone();
    let headers = response.headers().clone();
    let etag = headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let last_modified = headers
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let suggested_name = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(util::filename_from_content_disposition)
        .or_else(|| util::filename_from_url(&final_url));

    let empty_range = status == StatusCode::RANGE_NOT_SATISFIABLE
        && headers
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total)
            == Some(0);
    let (total, ranges) = if status == StatusCode::PARTIAL_CONTENT {
        let value = headers
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok());
        let total = value.and_then(content_range_total);
        (total, total.is_some())
    } else if empty_range {
        (Some(0), false)
    } else if status.is_success() {
        (response.content_length(), false)
    } else {
        let snippet = error_snippet(response, opts.timeout).await;
        let reason = status.canonical_reason().unwrap_or("请求失败");
        if snippet.is_empty() {
            bail!("服务器返回 HTTP {}（{reason}）", status.as_u16());
        }
        bail!("服务器返回 HTTP {}（{reason}）：{snippet}", status.as_u16());
    };

    Ok(Probe {
        total,
        ranges,
        etag,
        last_modified,
        suggested_name,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_segmented(
    client: &Client,
    opts: &Options,
    spec: &SourceSpec,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
    probe: Probe,
    auth: Option<BasicAuth>,
    url: Url,
    output: PathBuf,
    state_path: PathBuf,
    mut resume_state: Option<DownloadState>,
) -> Result<PathBuf> {
    let total = probe.total.expect("segmented download needs total size");

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
        if saved.etag != probe.etag || saved.last_modified != probe.last_modified {
            bail!("服务器文件已更新（ETag/修改时间不一致），无法安全续传；请使用 --force 重新下载");
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
        probe.etag.clone(),
        probe.last_modified.clone(),
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

    if resume_state.is_some() && probe.etag.is_none() && probe.last_modified.is_none() {
        ui.warn("服务器未提供 ETag 或 Last-Modified，续传只能按文件大小校验，存在内容变化风险");
    }

    let validator = probe
        .etag
        .as_ref()
        .filter(|value| !value.trim_start().starts_with("W/"))
        .cloned()
        .or_else(|| probe.last_modified.clone());

    let context = SegmentContext {
        client: client.clone(),
        url,
        auth,
        total,
        retries: opts.retries,
        timeout: opts.timeout,
        validator,
        file: file.clone(),
        bar: bar.clone(),
        cancel: cancel.child_token(),
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
        tasks.spawn(async move {
            run_segment(context, segment)
                .await
                .with_context(|| format!("分段 {start}-{end} 下载失败"))
        });
    }

    let mut first_error: Option<anyhow::Error> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                context.cancel.cancel();
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => {
                context.cancel.cancel();
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

async fn run_segment(context: SegmentContext, segment: Arc<Segment>) -> Result<()> {
    let mut consecutive_failures = 0_usize;
    loop {
        if context.cancel.is_cancelled() {
            bail!("已取消");
        }
        if segment.completed() {
            return Ok(());
        }
        let before = segment.done.load(Ordering::Relaxed);
        match fetch_segment(&context, &segment).await {
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
                let delay = Duration::from_millis(500 * factor);
                tokio::select! {
                    _ = context.cancel.cancelled() => bail!("已取消"),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn fetch_segment(
    context: &SegmentContext,
    segment: &Arc<Segment>,
) -> Result<(), SegmentError> {
    let start = segment.offset();
    let remaining = segment.remaining();
    if remaining == 0 {
        return Ok(());
    }

    let mut request = with_auth(
        context.client.get(context.url.clone()),
        context.auth.as_ref(),
    )
    .header(RANGE, format!("bytes={start}-{}", segment.end))
    .header(ACCEPT_ENCODING, "identity");
    if let Some(validator) = &context.validator {
        request = request.header(IF_RANGE, validator);
    }

    let response = tokio::select! {
        _ = context.cancel.cancelled() => return Err(SegmentError::Cancelled),
        result = request.send() => match result {
            Ok(response) => response,
            Err(error) => return Err(SegmentError::Retry(error.into())),
        },
    };

    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                SegmentError::Fatal(anyhow!("服务器返回 206 但缺少 Content-Range 头"))
            })?;
        let Some(actual_start) = content_range_start(&content_range) else {
            return Err(SegmentError::Fatal(anyhow!(
                "无法解析 Content-Range: {content_range}"
            )));
        };
        if actual_start != start {
            return Err(SegmentError::Fatal(anyhow!(
                "服务器返回了错误的 Range 起点（请求 {start}，实际 {actual_start}）"
            )));
        }
        if let Some(total) = content_range_total(&content_range)
            && total != context.total
        {
            return Err(SegmentError::Fatal(anyhow!(
                "下载过程中服务器文件大小发生变化（{total} != {}）",
                context.total
            )));
        }
    } else if status.is_success() {
        return Err(SegmentError::Fatal(anyhow!(
            "服务器忽略了 Range 请求（HTTP {}），无法继续分段下载",
            status.as_u16()
        )));
    } else if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(SegmentError::Fatal(anyhow!(
            "服务器拒绝 Range 请求（HTTP 416），文件可能已被修改"
        )));
    } else if status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
    {
        return Err(SegmentError::Retry(anyhow!(
            "服务器返回 HTTP {}",
            status.as_u16()
        )));
    } else {
        return Err(SegmentError::Fatal(anyhow!(
            "服务器返回 HTTP {}",
            status.as_u16()
        )));
    }

    let mut stream = response.bytes_stream();
    loop {
        let done = segment.done.load(Ordering::Relaxed);
        let left = segment.length().saturating_sub(done);
        if left == 0 {
            break;
        }

        let next = tokio::select! {
            _ = context.cancel.cancelled() => return Err(SegmentError::Cancelled),
            result = tokio::time::timeout(context.timeout, stream.next()) => result,
        };

        match next {
            Err(_) => return Err(SegmentError::Retry(anyhow!("读取数据超时"))),
            Ok(None) => return Err(SegmentError::Retry(anyhow!("连接在分段完成前关闭"))),
            Ok(Some(Err(error))) => {
                return Err(SegmentError::Retry(error.into()));
            }
            Ok(Some(Ok(chunk))) => {
                let take = chunk.len().min(left as usize);
                let offset = segment.start + done;
                segments::write_all_at(&context.file, &chunk[..take], offset)
                    .map_err(|error| SegmentError::Fatal(error.into()))?;
                segment.done.fetch_add(take as u64, Ordering::Relaxed);
                context.bar.inc(take as u64);
                if take < chunk.len() {
                    break;
                }
            }
        }
    }

    if segment.completed() {
        Ok(())
    } else {
        Err(SegmentError::Retry(anyhow!("数据流提前结束")))
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_stream(
    client: &Client,
    opts: &Options,
    spec: &SourceSpec,
    ui: &Ui,
    bar: ProgressBar,
    cancel: &CancellationToken,
    probe: Probe,
    auth: Option<BasicAuth>,
    url: Url,
    output: PathBuf,
    state_path: PathBuf,
) -> Result<PathBuf> {
    if state_path.exists() && !opts.force {
        bail!("服务器不支持 Range 请求，无法继续已有断点。使用 --force 重新下载，或更换镜像");
    }
    if output.exists() && !opts.force {
        bail!(
            "目标文件已存在且没有可用断点：{}。使用 --force 覆盖",
            output.display()
        );
    }
    if opts.force {
        state::remove_if_exists(&state_path)?;
    }

    if probe.ranges {
        ui.warn("服务器未提供文件大小，将使用单连接下载");
    } else {
        ui.warn("服务器不支持 Range 请求，将使用单连接下载（无法断点续传）");
    }
    bar.set_message("单连接");

    let temp_path = part_path(&output);
    let mut failures = 0_usize;
    loop {
        if cancel.is_cancelled() {
            remove_quiet(&temp_path);
            bail!("已取消");
        }
        match stream_attempt(
            client,
            opts,
            &probe,
            auth.as_ref(),
            &url,
            &temp_path,
            &bar,
            cancel,
        )
        .await
        {
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
                    "连接中断，将重新下载（第 {failures}/{} 次重试）: {error:#}",
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
    state::remove_if_exists(&state_path)?;
    let _ = spec;
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
async fn stream_attempt(
    client: &Client,
    opts: &Options,
    probe: &Probe,
    auth: Option<&BasicAuth>,
    url: &Url,
    temp_path: &Path,
    bar: &ProgressBar,
    cancel: &CancellationToken,
) -> Result<(), StreamError> {
    let mut file = tokio::fs::File::create(temp_path)
        .await
        .map_err(|error| StreamError::Fatal(error.into()))?;

    let request = with_auth(client.get(url.clone()), auth).header(ACCEPT_ENCODING, "identity");
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(StreamError::Cancelled),
        result = request.send() => match result {
            Ok(response) => response,
            Err(error) => return Err(StreamError::Retry(error.into())),
        },
    };

    let status = response.status();
    if !status.is_success() {
        let retryable = status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT;
        let snippet = error_snippet(response, opts.timeout).await;
        let error = anyhow!(
            "服务器返回 HTTP {}{}",
            status.as_u16(),
            if snippet.is_empty() {
                String::new()
            } else {
                format!("：{snippet}")
            }
        );
        return Err(if retryable {
            StreamError::Retry(error)
        } else {
            StreamError::Fatal(error)
        });
    }

    if let Some(encoding) = response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        && !encoding.eq_ignore_ascii_case("identity")
    {
        return Err(StreamError::Fatal(anyhow!(
            "服务器返回了压缩内容（Content-Encoding: {encoding}），无法按原文件保存"
        )));
    }

    let expected = response.content_length();
    if let Some(total) = expected.or(probe.total) {
        if total > 0 {
            ui::set_total(bar, total);
        } else {
            ui::set_unknown(bar);
        }
    } else {
        ui::set_unknown(bar);
    }
    bar.set_position(0);
    bar.set_message("单连接（无断点续传）");

    let mut stream = response.bytes_stream();
    let mut written = 0_u64;
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(StreamError::Cancelled),
            result = tokio::time::timeout(opts.timeout, stream.next()) => result,
        };
        match next {
            Err(_) => return Err(StreamError::Retry(anyhow!("读取数据超时"))),
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                return Err(StreamError::Retry(error.into()));
            }
            Ok(Some(Ok(chunk))) => {
                file.write_all(&chunk)
                    .await
                    .map_err(|error| StreamError::Fatal(error.into()))?;
                written += chunk.len() as u64;
                bar.inc(chunk.len() as u64);
            }
        }
    }

    file.flush()
        .await
        .map_err(|error| StreamError::Fatal(error.into()))?;
    drop(file);

    if let Some(expected) = expected
        && written != expected
    {
        return Err(StreamError::Retry(anyhow!(
            "收到的字节数不完整（{written}/{expected}）"
        )));
    }

    Ok(())
}

fn extract_basic_auth(url: &mut Url) -> Option<BasicAuth> {
    let username = url.username();
    if username.is_empty() {
        let _ = url.set_password(None);
        return None;
    }
    let user = percent_decode_str(username).decode_utf8_lossy().to_string();
    let password = url
        .password()
        .map(|value| percent_decode_str(value).decode_utf8_lossy().to_string())
        .unwrap_or_default();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(BasicAuth { user, password })
}

fn with_auth(builder: RequestBuilder, auth: Option<&BasicAuth>) -> RequestBuilder {
    match auth {
        Some(auth) => builder.basic_auth(&auth.user, Some(&auth.password)),
        None => builder,
    }
}

fn content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse().ok()
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

async fn error_snippet(response: reqwest::Response, timeout: Duration) -> String {
    let limit = timeout.min(Duration::from_secs(5));
    let bytes = match tokio::time::timeout(limit, response.bytes()).await {
        Ok(Ok(bytes)) => bytes,
        _ => return String::new(),
    };
    let text = String::from_utf8_lossy(&bytes);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    util::truncate_chars(&compact, 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(content_range_start("bytes 120-199/1000"), Some(120));
        assert_eq!(content_range_total("bytes 120-199/1000"), Some(1000));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
    }

    #[test]
    fn extracts_and_strips_basic_auth() {
        let mut url = Url::parse("https://alice:se%20cret@example.com/file.zip?q=1").unwrap();
        let auth = extract_basic_auth(&mut url).unwrap();
        assert_eq!(auth.user, "alice");
        assert_eq!(auth.password, "se cret");
        assert_eq!(url.as_str(), "https://example.com/file.zip?q=1");
    }
}
