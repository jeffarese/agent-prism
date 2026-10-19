//! On-disk usage cache shared between agent-prism processes (DESIGN §6.4).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use prism_core::model::UsageSnapshot;
use serde::{Deserialize, Serialize};

/// Samples older than this are pruned.
pub const SAMPLE_RETENTION_SECS: i64 = 7 * 24 * 3600;

/// The cached usage state of one profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedUsage {
    /// Last successful snapshot, kept across failures.
    pub snapshot: Option<UsageSnapshot>,
    /// When a fetch was last attempted (unix seconds).
    pub last_attempt: i64,
    /// Short, token-free description of the last failure.
    pub last_error: Option<String>,
    /// No fetch before this time (after a 429).
    pub backoff_until: Option<i64>,
    /// Token expiry seen at the last attempt.
    pub token_expires_at: Option<i64>,
}

/// One utilization sample for projections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub ts: i64,
    pub u5h: Option<f64>,
    pub u7d: Option<f64>,
}

/// An exclusive fetch lock on one profile; released on drop.
#[derive(Debug)]
pub struct CacheLock {
    _file: File,
}

/// Usage cache in one directory: `<id>.json`, `<id>.lock`,
/// `<id>.samples.jsonl`.
#[derive(Clone, Debug)]
pub struct UsageCache {
    dir: PathBuf,
}

impl UsageCache {
    /// A cache rooted at `dir` (created on first write).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{id}.{ext}"))
    }

    /// Reads `<id>.json`; `None` when missing or unreadable.
    pub fn load(&self, id: &str) -> Option<CachedUsage> {
        let s = fs::read_to_string(self.path(id, "json")).ok()?;
        serde_json::from_str(&s).ok()
    }

    /// Writes `<id>.json` atomically (temp file + rename).
    pub fn store(&self, id: &str, v: &CachedUsage) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir).context("create usage cache dir")?;
        let dst = self.path(id, "json");
        let tmp = self
            .dir
            .join(format!(".{id}.json.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(v)?).context("write usage cache")?;
        fs::rename(&tmp, &dst).context("rename usage cache")?;
        Ok(())
    }

    /// Takes the exclusive lock `<id>.lock`; `None` when held elsewhere.
    pub fn try_lock(&self, id: &str) -> anyhow::Result<Option<CacheLock>> {
        fs::create_dir_all(&self.dir).context("create usage cache dir")?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path(id, "lock"))
            .context("open usage lock")?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(CacheLock { _file: file })),
            Err(fs4::TryLockError::WouldBlock) => Ok(None),
            Err(fs4::TryLockError::Error(e)) => Err(e).context("lock usage cache"),
        }
    }

    /// Appends a sample to `<id>.samples.jsonl`, pruning old ones now and then.
    pub fn append_sample(&self, id: &str, s: &Sample) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir).context("create usage cache dir")?;
        let path = self.path(id, "samples.jsonl");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("open samples")?;
        writeln!(f, "{}", serde_json::to_string(s)?).context("append sample")?;
        drop(f);
        // Prune roughly once an hour of samples to keep the file small.
        if s.ts % 3600 < 120 {
            let keep = self.samples(id, s.ts - SAMPLE_RETENTION_SECS);
            let mut out = String::new();
            for k in &keep {
                out.push_str(&serde_json::to_string(k)?);
                out.push('\n');
            }
            let tmp = self
                .dir
                .join(format!(".{id}.samples.{}.tmp", std::process::id()));
            fs::write(&tmp, out)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(())
    }

    /// Samples with `ts >= since`, oldest first. Bad lines are skipped.
    pub fn samples(&self, id: &str, since: i64) -> Vec<Sample> {
        let Ok(f) = File::open(self.path(id, "samples.jsonl")) else {
            return Vec::new();
        };
        let mut v: Vec<Sample> = BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<Sample>(&l).ok())
            .filter(|s| s.ts >= since)
            .collect();
        v.sort_by_key(|s| s.ts);
        v
    }

    /// Modification time of `<id>.json`.
    pub fn modified(&self, id: &str) -> Option<SystemTime> {
        fs::metadata(self.path(id, "json")).ok()?.modified().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_load_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let c = UsageCache::new(d.path().join("usage"));
        assert!(c.load("a").is_none() && c.modified("a").is_none());
        let v = CachedUsage {
            last_attempt: 7,
            last_error: Some("x".into()),
            ..Default::default()
        };
        c.store("a", &v).unwrap();
        assert_eq!(c.load("a"), Some(v));
        assert!(c.modified("a").is_some());
    }

    #[test]
    fn lock_contention() {
        let d = tempfile::tempdir().unwrap();
        let c = UsageCache::new(d.path().to_path_buf());
        let l = c.try_lock("a").unwrap();
        assert!(l.is_some());
        assert!(c.try_lock("a").unwrap().is_none());
        assert!(c.try_lock("b").unwrap().is_some());
        drop(l);
        assert!(c.try_lock("a").unwrap().is_some());
    }

    #[test]
    fn samples_append_filter_prune() {
        let d = tempfile::tempdir().unwrap();
        let c = UsageCache::new(d.path().to_path_buf());
        let s = |ts| Sample {
            ts,
            u5h: Some(1.0),
            u7d: None,
        };
        c.append_sample("a", &s(100)).unwrap();
        c.append_sample("a", &s(200)).unwrap();
        assert_eq!(c.samples("a", 150), vec![s(200)]);
        // A prune-triggering timestamp far later drops both.
        let late = 3600 * 1000;
        c.append_sample("a", &s(late)).unwrap();
        assert_eq!(c.samples("a", 0), vec![s(late)]);
    }
}
