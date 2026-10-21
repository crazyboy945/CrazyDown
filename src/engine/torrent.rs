use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions, Session,
    SessionOptions, TorrentStatsState,
};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::ui::{self, Ui};

pub async fn download(
    source: &str,
    dir: &Path,
    session: &OnceCell<Arc<Session>>,
    ui: &Ui,
    bar: &ProgressBar,
    cancel: &CancellationToken,
    force: bool,
) -> Result<PathBuf> {
    let session = session
        .get_or_try_init(|| async {
            let options = SessionOptions {
                dht: Some(DhtSessionConfig {
                    bootstrap_addrs: None,
                    port: None,
                    persistence: None,
                }),
                listen: Some(ListenerOptions::default()),
                client_name_and_version: Some(format!("CrazyDown/{}", env!("CARGO_PKG_VERSION"))),
                ..Default::default()
            };
            Session::new_with_opts(dir.to_path_buf(), options)
                .await
                .context("无法启动 BT 会话（DHT/监听端口初始化失败）")
        })
        .await?
        .clone();

    ui::set_unknown(bar);
    bar.set_message("获取种子元数据");

    let list = tokio::select! {
        _ = cancel.cancelled() => bail!("已取消"),
        result = async {
            let add = if source.starts_with("magnet:") {
                AddTorrent::from_url(source)
            } else {
                AddTorrent::from_local_filename(source)?
            };
            let options = AddTorrentOptions {
                list_only: true,
                overwrite: true,
                ..Default::default()
            };
            session.add_torrent(add, Some(options)).await
        } => result.context("无法加载种子元数据")?,
    };

    let AddTorrentResponse::ListOnly(list) = list else {
        bail!("获取种子元数据失败");
    };

    let output_folder = list.output_folder.clone();
    let mut files = Vec::new();
    for details in list.info.iter_file_details() {
        files.push((details.filename.to_pathbuf(), details.len));
    }
    if files.is_empty() {
        bail!("种子不包含任何文件");
    }

    let mut any_existing = false;
    let mut oversized = Vec::new();
    for (relative, expected) in &files {
        if relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("种子包含非法文件路径：{}", relative.display());
        }
        let path = output_folder.join(relative);
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                any_existing = true;
                if metadata.is_dir() {
                    bail!("目标位置存在同名目录，无法写入文件：{}", path.display());
                }
                if metadata.len() > *expected {
                    oversized.push(path);
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("无法检查目标文件 {}", path.display()));
            }
        }
    }

    if !oversized.is_empty() && !force {
        bail!(
            "以下同名文件比种子中的文件更大，可能是其他文件：{}。如确认覆盖请使用 --force",
            oversized
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("、")
        );
    }

    if any_existing {
        if force {
            ui.warn("目标位置已有同名文件，将按种子校验并覆盖缺失部分");
        } else {
            ui.warn("发现已有文件，将按种子校验已有数据并继续下载（不会删除文件）");
            bar.set_message("校验已有数据");
        }
    }

    let torrent_bytes = list.torrent_bytes;
    let options = AddTorrentOptions {
        paused: true,
        overwrite: force || any_existing,
        ..Default::default()
    };
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("已取消"),
        result = session.add_torrent(AddTorrent::from_bytes(torrent_bytes), Some(options)) => {
            result.context("无法添加 BT 任务")?
        }
    };
    let handle = match response {
        AddTorrentResponse::Added(_, handle) | AddTorrentResponse::AlreadyManaged(_, handle) => {
            handle
        }
        AddTorrentResponse::ListOnly(_) => bail!("BT 任务启动失败"),
    };

    if cancel.is_cancelled() {
        let _ = session.pause(&handle).await;
        bail!("已取消");
    }

    let name = handle.name().unwrap_or_else(|| "BT 任务".to_string());
    ui.log(format!("BT 任务: {name}"));

    let stats = handle.stats();
    let total = stats.total_bytes;
    if total > 0 {
        ui::set_total(bar, total);
    } else {
        ui::set_unknown(bar);
    }
    bar.set_position(stats.progress_bytes);

    let mut unpaused = false;
    loop {
        if cancel.is_cancelled() {
            let _ = session.pause(&handle).await;
            bail!("已中断，已下载的数据已保留；重新运行相同命令会校验并继续");
        }

        let stats = handle.stats();
        if let Some(error) = &stats.error {
            bail!("BT 下载失败: {error}");
        }
        if matches!(stats.state, TorrentStatsState::Error) {
            bail!("BT 下载进入错误状态");
        }
        if matches!(stats.state, TorrentStatsState::Paused) && !unpaused {
            if let Err(error) = session.unpause(&handle).await {
                let _ = session.pause(&handle).await;
                return Err(error).context("无法启动 BT 下载");
            }
            unpaused = true;
            continue;
        }
        if stats.total_bytes > 0 {
            bar.set_length(stats.total_bytes);
        }
        bar.set_position(stats.progress_bytes);
        if stats.finished {
            break;
        }
        if let Some(live) = &stats.live {
            bar.set_message(format!("下载中 · {}", live.download_speed));
        } else if matches!(stats.state, TorrentStatsState::Paused) {
            bar.set_message("等待中");
        } else {
            bar.set_message("连接节点中");
        }

        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    let _ = session.pause(&handle).await;
    bar.set_position(handle.stats().total_bytes);

    let root = handle.output_folder().to_path_buf();
    let path = if files.len() == 1 {
        root.join(&files[0].0)
    } else {
        root
    };
    if !path.exists() {
        return Err(anyhow!(
            "BT 下载已完成，但输出路径不存在：{}",
            path.display()
        ));
    }
    Ok(path)
}
