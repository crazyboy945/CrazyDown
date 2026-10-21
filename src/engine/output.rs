use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::engine::state;
use crate::util;

#[derive(Default)]
pub struct OutputRegistry {
    reserved: Mutex<HashSet<PathBuf>>,
}

pub struct OutputPlan {
    pub path: PathBuf,
}

impl OutputRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn choose(
        &self,
        dir: &Path,
        suggested: &str,
        explicit: Option<PathBuf>,
        force: bool,
    ) -> Result<OutputPlan> {
        let suggested = util::sanitize_filename(suggested).unwrap_or_else(|| "download".into());
        let mut reserved = self.reserved.lock().expect("output registry poisoned");

        if let Some(explicit) = explicit {
            let explicit = util::absolutize(&explicit);
            let path = if is_directory_hint(&explicit) {
                explicit.join(&suggested)
            } else {
                explicit
            };
            if path.is_dir() {
                bail!("输出路径是一个目录：{}", path.display());
            }
            if !reserved.insert(path.clone()) {
                bail!("多个任务使用了相同的输出路径：{}", path.display());
            }
            if path.exists() && !force && !state::state_path(&path).exists() {
                bail!(
                    "目标文件已存在：{}。使用 --force 覆盖，或改用其他输出文件名",
                    path.display()
                );
            }
            ensure_parent(&path)?;
            return Ok(OutputPlan { path });
        }

        for index in 0..10_000 {
            let name = util::unique_filename(&suggested, index);
            let path = dir.join(name);
            if reserved.contains(&path) {
                continue;
            }
            if path.exists() && !force && !state::state_path(&path).exists() {
                continue;
            }
            if path.is_dir() {
                continue;
            }
            reserved.insert(path.clone());
            ensure_parent(&path)?;
            return Ok(OutputPlan { path });
        }

        bail!("无法为 {} 找到可用的输出文件名", suggested)
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }
    Ok(())
}

fn is_directory_hint(path: &Path) -> bool {
    if path.is_dir() {
        return true;
    }
    let text = path.to_string_lossy();
    text.ends_with('/') || text.ends_with('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::state::{DownloadState, StateMode};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crazydown-output-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn avoids_overwriting_derived_names() {
        let dir = temp_dir("derive");
        std::fs::write(dir.join("file.zip"), b"existing").unwrap();
        let registry = OutputRegistry::new();
        let plan = registry
            .choose(&dir, "file.zip", None, false)
            .expect("choose output");
        assert_eq!(plan.path.file_name().unwrap(), "file (1).zip");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lets_existing_state_resume() {
        let dir = temp_dir("resume");
        let target = dir.join("file.zip");
        std::fs::write(&target, b"partial").unwrap();
        let state = DownloadState::new("url", 100, StateMode::Segmented, None, None);
        state::save(&state::state_path(&target), &state).ok();

        let registry = OutputRegistry::new();
        let plan = registry
            .choose(&dir, "file.zip", None, false)
            .expect("choose output");
        assert_eq!(plan.path, target);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_output_requires_force_when_file_exists() {
        let dir = temp_dir("explicit");
        let target = dir.join("out.bin");
        std::fs::write(&target, b"existing").unwrap();
        assert!(
            OutputRegistry::new()
                .choose(&dir, "out.bin", Some(target.clone()), false)
                .is_err()
        );
        assert!(
            OutputRegistry::new()
                .choose(&dir, "out.bin", Some(target), true)
                .is_ok()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
