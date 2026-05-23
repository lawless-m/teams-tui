//! Desktop notifications and status file writer.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

pub struct Notifier;

impl Notifier {
    pub fn new() -> Self {
        Self
    }

    pub fn notify(&self, prefix: &str, sender: &str, body: &str) -> Result<()> {
        let preview: String = body.chars().take(100).collect();
        let line = format!("{prefix}:{sender}: {preview}");
        notify_rust::Notification::new()
            .appname("teams-tui")
            .summary("teams-tui")
            .body(&line)
            .urgency(notify_rust::Urgency::Normal)
            .show()
            .context("failed to show desktop notification")?;
        Ok(())
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StatusCounters {
    unread: AtomicU64,
    mentions: AtomicU64,
    path: PathBuf,
}

impl StatusCounters {
    pub fn new(path: PathBuf) -> Self {
        Self {
            unread: AtomicU64::new(0),
            mentions: AtomicU64::new(0),
            path,
        }
    }

    pub fn add_unread(&self) {
        self.unread.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_mention(&self) {
        self.mentions.fetch_add(1, Ordering::Relaxed);
        self.unread.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.unread.store(0, Ordering::Relaxed);
        self.mentions.store(0, Ordering::Relaxed);
    }

    pub fn write(&self) -> Result<()> {
        let u = self.unread.load(Ordering::Relaxed);
        let m = self.mentions.load(Ordering::Relaxed);
        write_status_file(&self.path, u, m)
    }
}

pub fn format_status_line(unread: u64, mentions: u64) -> String {
    format!("unread={unread} mentions={mentions}\n")
}

pub fn write_status_file(path: &Path, unread: u64, mentions: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create status dir {}", parent.display()))?;
    }
    let content = format_status_line(unread, mentions);
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    std::fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("failed to write status tmp {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename status into place {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("teams-tui-status-{tag}-{n}"));
        p
    }

    #[test]
    fn format_produces_expected_line() {
        assert_eq!(format_status_line(3, 1), "unread=3 mentions=1\n");
        assert_eq!(format_status_line(0, 0), "unread=0 mentions=0\n");
    }

    #[test]
    fn atomic_write_leaves_no_partial_file() {
        let dir = tempdir("atomic");
        let path = dir.join("status");

        write_status_file(&path, 5, 2).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unread=5 mentions=2\n"
        );

        write_status_file(&path, 7, 3).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unread=7 mentions=3\n"
        );

        let mut tmp_os = path.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        assert!(!tmp.exists(), "tmp file should not be left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counters_increment_and_reset() {
        let dir = tempdir("counters");
        let path = dir.join("status");
        let c = StatusCounters::new(path.clone());
        c.add_unread();
        c.add_mention();
        c.write().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unread=2 mentions=1\n"
        );

        c.reset();
        c.write().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unread=0 mentions=0\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
