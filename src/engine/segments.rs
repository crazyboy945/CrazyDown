use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::state::{self, DownloadState, SegmentState};

const MIN_SEGMENT_SIZE: u64 = 64 * 1024;

pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub done: AtomicU64,
}

impl Segment {
    pub fn new(start: u64, end: u64, done: u64) -> Self {
        Self {
            start,
            end,
            done: AtomicU64::new(done),
        }
    }

    pub fn length(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn completed(&self) -> bool {
        self.done.load(Ordering::Relaxed) >= self.length()
    }

    pub fn offset(&self) -> u64 {
        self.start + self.done.load(Ordering::Relaxed)
    }

    pub fn remaining(&self) -> u64 {
        self.length()
            .saturating_sub(self.done.load(Ordering::Relaxed))
    }

    pub fn snapshot(&self) -> SegmentState {
        SegmentState {
            start: self.start,
            end: self.end,
            done: self.done.load(Ordering::Relaxed).min(self.length()),
        }
    }
}

pub fn split_ranges(total: u64, parts: usize) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    let wanted = (parts.max(1) as u64).min(total.div_ceil(MIN_SEGMENT_SIZE));
    let count = wanted.max(1);
    let base = total / count;
    let remainder = total % count;

    let mut ranges = Vec::with_capacity(count as usize);
    let mut start = 0_u64;
    for index in 0..count {
        let length = base + u64::from(index < remainder);
        let end = start + length - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

pub fn from_state(state: &DownloadState) -> Vec<Arc<Segment>> {
    state
        .segments
        .iter()
        .map(|segment| {
            Arc::new(Segment::new(
                segment.start,
                segment.end,
                segment.done.min(segment.end - segment.start + 1),
            ))
        })
        .collect()
}

pub fn new_segments(total: u64, parts: usize) -> Vec<Arc<Segment>> {
    split_ranges(total, parts)
        .into_iter()
        .map(|(start, end)| Arc::new(Segment::new(start, end, 0)))
        .collect()
}

pub fn total_done(segments: &[Arc<Segment>]) -> u64 {
    segments
        .iter()
        .map(|segment| segment.done.load(Ordering::Relaxed))
        .sum()
}

pub fn snapshot_state(template: &DownloadState, segments: &[Arc<Segment>]) -> DownloadState {
    let mut snapshot = template.clone();
    snapshot.segments = segments.iter().map(|segment| segment.snapshot()).collect();
    snapshot.downloaded = snapshot.segments.iter().map(|segment| segment.done).sum();
    snapshot
}

pub fn save_snapshot(
    path: &std::path::Path,
    template: &DownloadState,
    segments: &[Arc<Segment>],
) -> anyhow::Result<()> {
    let snapshot = snapshot_state(template, segments);
    state::save(path, &snapshot)
}

pub fn spawn_saver(
    path: PathBuf,
    template: DownloadState,
    segments: Vec<Arc<Segment>>,
    stop: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut warned = false;
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = save_snapshot(&path, &template, &segments)
                        && !warned
                    {
                        eprintln!("警告: 无法保存断点信息: {error:#}");
                        warned = true;
                    }
                }
            }
        }
    })
}

#[cfg(windows)]
pub fn write_all_at(file: &File, buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    let mut written = 0;
    while written < buf.len() {
        match file.seek_write(&buf[written..], offset)? {
            0 => return Err(io::Error::new(io::ErrorKind::WriteZero, "写入文件失败")),
            n => {
                written += n;
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn write_all_at(file: &File, buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    let mut written = 0;
    while written < buf.len() {
        match file.write_at(&buf[written..], offset)? {
            0 => return Err(io::Error::new(io::ErrorKind::WriteZero, "写入文件失败")),
            n => {
                written += n;
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_into_contiguous_balanced_ranges() {
        let ranges = split_ranges(1_000_003, 8);
        assert_eq!(ranges.len(), 8);
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, 1_000_002);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0);
        }
        let lengths: Vec<u64> = ranges.iter().map(|(s, e)| e - s + 1).collect();
        assert!(lengths.iter().max().unwrap() - lengths.iter().min().unwrap() <= 1);
    }

    #[test]
    fn limits_segments_for_small_files() {
        assert_eq!(split_ranges(100, 8).len(), 1);
        assert_eq!(split_ranges(0, 8).len(), 0);
    }

    #[test]
    fn restores_done_offsets() {
        let mut state = DownloadState::new("url", 10, state::StateMode::Segmented, None, None);
        state.segments = vec![
            SegmentState {
                start: 0,
                end: 4,
                done: 2,
            },
            SegmentState {
                start: 5,
                end: 9,
                done: 5,
            },
        ];
        let segments = from_state(&state);
        assert_eq!(total_done(&segments), 7);
        assert!(segments[1].completed());
    }
}
