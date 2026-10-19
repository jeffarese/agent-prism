//! File logger: appends to `<state>/agent-prism.log`, rotates at 2 MB, keeps the
//! last lines in memory for the Log view. Level from `AGP_LOG` (default
//! info). Callers must never log tokens.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const KEEP_LINES: usize = 200;

static RECENT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

struct FileLogger {
    file: PathBuf,
    level: LevelFilter,
    lock: Mutex<()>,
}

impl Log for FileLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let ts = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S");
        let line = format!(
            "{ts} {:<5} [{}] {}",
            r.level(),
            std::process::id(),
            r.args()
        );
        {
            let mut q = RECENT.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(line.clone());
            while q.len() > KEEP_LINES {
                q.pop_front();
            }
        }
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if std::fs::metadata(&self.file).is_ok_and(|m| m.len() > MAX_BYTES) {
            let _ = std::fs::rename(&self.file, self.file.with_extension("log.1"));
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
        {
            // One write per line: parallel agent-prism sessions append to the same file.
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn flush(&self) {}
}

/// Installs the logger once. Errors (e.g. no state dir) are ignored.
pub fn init(file: PathBuf) {
    let level = match prism_core::env::var("AGP_LOG").as_deref() {
        Some("trace") => LevelFilter::Trace,
        Some("debug") => LevelFilter::Debug,
        Some("warn") => LevelFilter::Warn,
        Some("error") => LevelFilter::Error,
        Some("off") => LevelFilter::Off,
        _ => LevelFilter::Info,
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let logger = Box::new(FileLogger {
        file,
        level,
        lock: Mutex::new(()),
    });
    if log::set_logger(Box::leak(logger)).is_ok() {
        log::set_max_level(level);
    }
}

/// The last log lines of this process, newest last.
pub fn recent() -> Vec<String> {
    RECENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}
