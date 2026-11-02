//! 极简文件日志：追加写 ~/.yi-console/yi-agentd.log，超过 5MB 轮转一份。

use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static TO_STDERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const MAX_SIZE: u64 = 5 * 1024 * 1024;

pub fn init(to_stderr: bool) {
    TO_STDERR.store(to_stderr, std::sync::atomic::Ordering::Relaxed);
    let path = crate::paths::log_path();
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_SIZE {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        *LOG.lock().unwrap() = Some(f);
    }
}

pub fn write(level: &str, msg: std::fmt::Arguments) {
    let line = format!("{} {level} {msg}\n", crate::util::now_rfc3339());
    if TO_STDERR.load(std::sync::atomic::Ordering::Relaxed) {
        eprint!("{line}");
    }
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::write("INFO", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::write("WARN", format_args!($($t)*)) };
}
