use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tokio_util::sync::CancellationToken;

use crazydown::cli::Cli;
use crazydown::engine::{self, JobStatus, Options, RunSummary};
use crazydown::util::{absolutize, format_bytes, format_duration};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("错误: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let specs = cli.load_specs()?;
    let options = Options {
        dir: cli.dir.clone(),
        jobs: cli.jobs as usize,
        segments: cli.segments as usize,
        retries: cli.retries as usize,
        timeout: Duration::from_secs(cli.timeout),
        ftp_user: cli.ftp_user.clone(),
        ftp_pass: cli.ftp_pass.clone(),
        force: cli.force,
        insecure: cli.insecure,
        quiet: cli.quiet,
        no_progress: cli.no_progress,
    };

    if !cli.quiet {
        eprintln!(
            "CrazyDown v{} · HTTP/HTTPS · FTP · BitTorrent",
            env!("CARGO_PKG_VERSION")
        );
        eprintln!(
            "保存目录: {} · 并发任务: {} · 分段连接: {}",
            absolutize(&cli.dir).display(),
            cli.jobs,
            cli.segments
        );
        eprintln!();
    }

    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\n收到中断信号，正在保存进度…（再按一次 Ctrl+C 强制退出）");
                cancel.cancel();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });
    }

    let summary = engine::run(specs, options, cancel.clone()).await?;
    let interrupted = cancel.is_cancelled();
    print_summary(&summary, interrupted);

    if interrupted {
        return Ok(ExitCode::from(130));
    }
    if summary.failures() > 0 {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_summary(summary: &RunSummary, interrupted: bool) {
    let succeeded = summary.successes();
    let failed = summary.failures();
    let cancelled = summary.cancelled();
    let total_time = format_duration(summary.elapsed);

    if interrupted {
        println!("已中断：成功 {succeeded}，失败 {failed}，取消 {cancelled}，用时 {total_time}");
    } else {
        println!("完成：成功 {succeeded}，失败 {failed}，用时 {total_time}");
    }

    for report in &summary.reports {
        match report.status {
            JobStatus::Completed => {
                let output = report
                    .output
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "-".to_string());
                let size = report
                    .bytes
                    .map(|bytes| format!(", {}", format_bytes(bytes)))
                    .unwrap_or_default();
                println!(
                    "  [{}] 完成 {} -> {}{}, {}",
                    report.id,
                    report.source,
                    output,
                    size,
                    format_duration(report.elapsed)
                );
            }
            JobStatus::Failed => {
                println!(
                    "  [{}] 失败 {}: {}",
                    report.id,
                    report.source,
                    report.error.as_deref().unwrap_or("未知错误")
                );
            }
            JobStatus::Cancelled => {
                println!(
                    "  [{}] 已取消 {}: {}",
                    report.id,
                    report.source,
                    report.error.as_deref().unwrap_or("已中断")
                );
            }
        }
    }

    if summary.unstarted > 0 {
        println!("  还有 {} 个任务未开始", summary.unstarted);
    }
}
