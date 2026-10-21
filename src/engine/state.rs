use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
const STATE_SUFFIX: &str = ".crazydown.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateMode {
    Segmented,
    Stream,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentState {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub version: u32,
    pub source_key: String,
    pub total: u64,
    pub mode: StateMode,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub segments: Vec<SegmentState>,
    #[serde(default)]
    pub downloaded: u64,
}

impl DownloadState {
    pub fn new(
        source: &str,
        total: u64,
        mode: StateMode,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Self {
        Self {
            version: STATE_VERSION,
            source_key: source_key(source),
            total,
            mode,
            etag,
            last_modified,
            segments: Vec::new(),
            downloaded: 0,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != STATE_VERSION {
            bail!(
                "断点文件版本不受支持（{}，当前支持 {}）",
                self.version,
                STATE_VERSION
            );
        }
        if self.mode == StateMode::Segmented {
            self.validate_segments()?;
        }
        Ok(())
    }

    fn validate_segments(&self) -> Result<()> {
        if self.segments.is_empty() {
            bail!("断点文件缺少分段信息");
        }
        let mut cursor = 0_u64;
        for segment in &self.segments {
            if segment.start != cursor || segment.end < segment.start {
                bail!("断点文件分段范围不连续");
            }
            let length = segment.end - segment.start + 1;
            if segment.done > length {
                bail!("断点文件分段进度超出范围");
            }
            cursor = segment.end.checked_add(1).context("断点文件分段范围溢出")?;
        }
        if cursor != self.total {
            bail!("断点文件大小与分段范围不一致");
        }
        Ok(())
    }
}

pub fn source_key(source: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn state_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(STATE_SUFFIX);
    PathBuf::from(name)
}

pub fn load_if_exists(path: &Path) -> Result<Option<DownloadState>> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("无法读取断点文件 {}", path.display()));
        }
    };
    let state: DownloadState = serde_json::from_slice(&content)
        .with_context(|| format!("断点文件损坏：{}", path.display()))?;
    state.validate()?;
    Ok(Some(state))
}

pub fn save(path: &Path, state: &DownloadState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建断点目录 {}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(state).context("序列化断点信息失败")?;
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".tmp.{}", std::process::id()));
    let temp_path = PathBuf::from(temp);
    {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("无法写入断点文件 {}", temp_path.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("无法写入断点文件 {}", temp_path.display()))?;
        file.flush().ok();
    }
    fs::rename(&temp_path, path).with_context(|| format!("无法更新断点文件 {}", path.display()))?;
    Ok(())
}

pub fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("无法删除断点文件 {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_key_is_stable_and_does_not_include_secret() {
        let source = "ftp://alice:secret@example.com/file.iso";
        let key = source_key(source);
        assert_eq!(key, source_key(source));
        assert_eq!(key.len(), 16);
        assert!(!key.contains("secret"));
    }

    #[test]
    fn validates_contiguous_segments() {
        let mut state = DownloadState::new("url", 100, StateMode::Segmented, None, None);
        state.segments = vec![
            SegmentState {
                start: 0,
                end: 49,
                done: 10,
            },
            SegmentState {
                start: 50,
                end: 99,
                done: 0,
            },
        ];
        assert!(state.validate().is_ok());

        state.segments[1].start = 51;
        assert!(state.validate().is_err());
    }

    #[test]
    fn roundtrips_state_file() {
        let dir = std::env::temp_dir().join(format!("crazydown-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let output = dir.join("file.bin");
        let mut state = DownloadState::new("url", 10, StateMode::Segmented, None, None);
        state.segments = vec![SegmentState {
            start: 0,
            end: 9,
            done: 4,
        }];
        let path = state_path(&output);
        save(&path, &state).unwrap();
        let loaded = load_if_exists(&path).unwrap().unwrap();
        assert_eq!(loaded.segments[0].done, 4);
        remove_if_exists(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
