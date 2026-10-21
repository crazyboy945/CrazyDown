use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::util;

const EXAMPLES: &str = "\
示例:
  crazydown https://example.com/ubuntu.iso
  crazydown -o image.iso https://example.com/files/image.iso
  crazydown -d D:\\Downloads -j 4 https://example.com/a.zip https://example.com/b.zip
  crazydown -i urls.txt -d D:\\Downloads
  crazydown -d D:\\Downloads \"magnet:?xt=urn:btih:...\"
  crazydown -d D:\\Downloads D:\\torrents\\debian.torrent

批量列表每行一个地址，可在地址后用空格或引号指定目标文件名（# 开头为注释）:
  https://example.com/a.zip
  https://example.com/b.zip  saved-b.zip
  \"ftp://user:pass@example.com/path/c.iso\"  \"c copy.iso\"
  magnet:?xt=urn:btih:...";

#[derive(Debug, Parser)]
#[command(
    name = "crazydown",
    version,
    about = "CrazyDown — 支持 HTTP/HTTPS/FTP/BT 的多线程、断点续传下载器",
    long_about = "CrazyDown 是一个多协议并发下载器。\n\
                  HTTP/HTTPS：多连接分段下载、断点续传、自动重试。\n\
                  FTP：在服务器支持 REST 时多连接下载，并支持断点续传。\n\
                  BitTorrent：支持磁力链接和本地 .torrent 种子。\n\
                  支持批量列表、并发任务和实时进度显示。",
    after_help = EXAMPLES,
    arg_required_else_help = true
)]
pub struct Cli {
    /// 下载地址：http(s)://、ftp://、magnet: 或本地 .torrent 文件
    #[arg(value_name = "SOURCE")]
    pub sources: Vec<String>,

    /// 批量下载列表文件
    #[arg(short = 'i', long = "input", value_name = "FILE")]
    pub input: Option<PathBuf>,

    /// 保存目录
    #[arg(short = 'd', long = "dir", value_name = "DIR", default_value = ".")]
    pub dir: PathBuf,

    /// 输出文件名（仅限单个 HTTP/FTP 任务；可以是目录）
    #[arg(short = 'o', long = "output", value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// 同时下载的任务数
    #[arg(
        short = 'j',
        long = "jobs",
        value_name = "N",
        default_value_t = 3,
        value_parser = clap::value_parser!(u32).range(1..=64)
    )]
    pub jobs: u32,

    /// 每个文件使用的分段连接数
    #[arg(
        short = 's',
        long = "segments",
        visible_alias = "connections",
        value_name = "N",
        default_value_t = 8,
        value_parser = clap::value_parser!(u32).range(1..=32)
    )]
    pub segments: u32,

    /// 网络失败时的重试次数
    #[arg(
        long = "retries",
        value_name = "N",
        default_value_t = 3,
        value_parser = clap::value_parser!(u32).range(0..=20)
    )]
    pub retries: u32,

    /// 连接与读取超时（秒）
    #[arg(
        long = "timeout",
        value_name = "SECS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..=3600)
    )]
    pub timeout: u64,

    /// FTP 用户名（地址中未提供时使用）
    #[arg(long = "ftp-user", value_name = "USER")]
    pub ftp_user: Option<String>,

    /// FTP 密码（地址中未提供时使用）
    #[arg(long = "ftp-pass", visible_alias = "ftp-password", value_name = "PASS")]
    pub ftp_pass: Option<String>,

    /// 覆盖已存在的文件或断点记录
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// 跳过 HTTPS 证书校验（不安全）
    #[arg(long = "insecure")]
    pub insecure: bool,

    /// 不显示动态进度条，仅输出任务日志
    #[arg(long = "no-progress")]
    pub no_progress: bool,

    /// 安静模式：只输出最终结果与错误
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub source: String,
    pub output: Option<PathBuf>,
}

impl Cli {
    pub fn load_specs(&self) -> Result<Vec<SourceSpec>> {
        let mut specs: Vec<SourceSpec> = self
            .sources
            .iter()
            .map(|source| SourceSpec {
                source: source.clone(),
                output: None,
            })
            .collect();

        if let Some(path) = &self.input {
            let mut loaded = parse_batch_file(path)?;
            specs.append(&mut loaded);
        }

        if specs.is_empty() {
            bail!("请至少提供一个下载地址，或使用 -i 指定批量列表文件");
        }

        if self.output.is_some() && specs.len() > 1 {
            bail!("--output 只能用于单个任务；批量下载请在列表文件中逐行指定目标文件名");
        }

        if let Some(output) = &self.output {
            let output = util::absolutize(output);
            specs[0].output = Some(output);
            return Ok(specs);
        }

        let dir = util::absolutize(&self.dir);
        for spec in &mut specs {
            if let Some(output) = &spec.output
                && !output.is_absolute()
            {
                spec.output = Some(dir.join(output));
            }
        }

        Ok(specs)
    }
}

pub fn parse_batch_file(path: &Path) -> Result<Vec<SourceSpec>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("无法读取批量列表文件 {}", path.display()))?;
    parse_batch_content(&content)
        .with_context(|| format!("解析批量列表文件 {} 失败", path.display()))
}

pub(crate) fn parse_batch_content(content: &str) -> Result<Vec<SourceSpec>> {
    let mut specs = Vec::new();
    for (index, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens = tokenize(line).with_context(|| format!("第 {} 行", index + 1))?;
        if tokens.is_empty() {
            continue;
        }
        if tokens.len() > 2 {
            bail!("第 {} 行参数过多；文件名包含空格时请使用双引号", index + 1);
        }
        specs.push(SourceSpec {
            source: tokens[0].clone(),
            output: tokens.get(1).map(PathBuf::from),
        });
    }
    Ok(specs)
}

fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            },
        }
    }

    if quote.is_some() {
        bail!("引号未闭合");
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batch_lines_with_quotes_and_comments() {
        let specs = parse_batch_content(
            "# comment\n\
             https://example.com/a.zip\n\
             https://example.com/b.zip  target.zip\n\
             \"ftp://example.com/my file.iso\"  \"my file.iso\"\n",
        )
        .unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].source, "https://example.com/a.zip");
        assert_eq!(specs[1].output.as_deref(), Some(Path::new("target.zip")));
        assert_eq!(specs[2].source, "ftp://example.com/my file.iso");
        assert_eq!(specs[2].output.as_deref(), Some(Path::new("my file.iso")));
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(parse_batch_content("\"https://example.com/a.zip\n").is_err());
    }

    #[test]
    fn tokenizes_windows_path_with_backslashes() {
        let tokens = tokenize(r#""D:\torrents\debian.torrent""#).unwrap();
        assert_eq!(tokens, vec![r"D:\torrents\debian.torrent"]);
    }
}
