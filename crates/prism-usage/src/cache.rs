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

/// The buckets of [`history_5h`]: 24 hours in half hours.
pub const HISTORY_BUCKETS: usize = 48;
const HISTORY_BUCKET_SECS: i64 = 1800;

/// The 5h utilization over the last 24 hours, oldest first: the peak
/// sampled in each half hour of the clock (the last bucket is the running
/// one), and `None` where nothing was sampled.
pub fn history_5h(samples: &[Sample], now: i64) -> Vec<Option<f64>> {
    let end = now - now.rem_euclid(HISTORY_BUCKET_SECS) + HISTORY_BUCKET_SECS;
    let start = end - HISTORY_BUCKETS as i64 * HISTORY_BUCKET_SECS;
    let mut out = vec![None; HISTORY_BUCKETS];
    for s in samples.iter().filter(|s| s.ts >= start && s.ts <= now) {
        let k = ((s.ts - start) / HISTORY_BUCKET_SECS) as usize;
        if let (Some(v), Some(slot)) = (s.u5h, out.get_mut(k)) {
            *slot = Some(slot.map_or(v, |x: f64| x.max(v)));
        }
    }
    out
}

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
    fn history_in_half_hours() {
        // Ten minutes after a full hour (1_800_000_000 is one): the running
        // bucket has 20 minutes to go.
        let now = 1_800_000_000 + 600;
        let s = |ago: i64, u5h: Option<f64>| Sample {
            ts: now - ago,
            u5h,
            u7d: Some(1.0),
        };
        let samples = [
            // Older than the 24 hours of the history.
            s(24 * 3600, Some(99.0)),
            // The first bucket: the half hour that began 23.5 hours ago.
            s(23 * 3600 + 1200, Some(7.0)),
            // The peak of a bucket counts.
            s(3 * 3600, Some(20.0)),
            s(3 * 3600 - 60, Some(60.0)),
            s(3 * 3600 - 120, Some(40.0)),
            // A sample without a 5h window.
            s(3600, None),
            s(0, Some(5.0)),
        ];
        let h = history_5h(&samples, now);
        assert_eq!(h.len(), HISTORY_BUCKETS);
        assert_eq!(h[0], Some(7.0));
        assert_eq!(h[41], Some(60.0));
        assert_eq!(h[47], Some(5.0));
        assert_eq!(h.iter().flatten().count(), 3);
    }

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
