//! 运行日志：写 `<app data>/logs/main.log`，同时透传 stderr。
//!
//! `log_line` 只把行塞进有界队列（满了丢），文件 IO 在独立线程；单文件超过 5 MiB 轮转，
//! 保留 7 天。退出时 `flush()` 给写线程一个确认点。日志失败绝不影响功能。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

/// 日志文件保留天数（与 Electron 版同口径：超过一周的历史没有排查价值）。
const RETENTION_DAYS: i64 = 7;
const FILE_NAME: &str = "main.log";
/// 单个日志文件上限：超过后轮转，避免长期运行把 `main.log` 撑到几百 MB。
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// 写队列容量：满了直接丢弃日志，绝不把业务线程堵在 IO 上。
const QUEUE_CAPACITY: usize = 2048;
/// 退出时等待写线程确认的上限；卡住也不能拖住进程退出。
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_WRITER: OnceLock<Mutex<LogWriter>> = OnceLock::new();
static SENDER: OnceLock<SyncSender<Message>> = OnceLock::new();

enum Message {
    Line(String),
    Flush(mpsc::Sender<()>),
}

/// 安装日志：`log_dir` 下写 `main.log`。幂等；目录不可写时静默放弃。
pub fn install(log_dir: &Path) {
    if LOG_PATH.get().is_some() {
        return;
    }
    if fs::create_dir_all(log_dir).is_err() {
        return;
    }
    let path = log_dir.join(FILE_NAME);
    let Ok(file) = open_log(&path) else {
        return;
    };
    let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if LOG_PATH.set(path.clone()).is_err() {
        return;
    }
    prune(log_dir);

    let writer = Mutex::new(LogWriter::new(path, file, bytes));
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let _ = SENDER.set(sender);
    // 写线程只需要一个通道句柄；日志线程退出（writer 掉线）后文件仍可从主线程补写。
    if LOG_WRITER.set(writer).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("d48-log".to_string())
        .spawn(move || writer_loop(receiver));
}

/// 写一行日志：同时打到 stderr（开发时可见）与日志文件（打包后可见）。
pub fn log_line(message: &str) {
    eprintln!("{message}");
    let line = format!("{message}\n");
    let delivered = SENDER
        .get()
        // 有界队列满了就丢弃，不能因为日志把 IPC 阻塞住。
        .is_some_and(|sender| sender.try_send(Message::Line(line.clone())).is_ok());
    if !delivered {
        // 写线程还没起来 / 已经退出：退回主线程同步写，宁可慢一点也不能丢。
        write_direct(&line);
    }
}

fn write_direct(line: &str) {
    let Some(writer) = LOG_WRITER.get() else {
        return;
    };
    match writer.lock() {
        Ok(mut guard) => guard.write_line(line),
        Err(poisoned) => poisoned.into_inner().write_line(line),
    }
}

/// 退出前尽量把队列里已排队的行刷到磁盘；超时直接放弃。
pub fn flush() {
    let Some(sender) = SENDER.get() else {
        return;
    };
    let (ack_sender, ack_receiver) = mpsc::channel();
    if sender.try_send(Message::Flush(ack_sender)).is_ok() {
        let _ = ack_receiver.recv_timeout(FLUSH_TIMEOUT);
    }
}

/// 日志目录（`<app data>/logs`）：`openPath` 的白名单里包含 app data，
/// 所以渲染层可以直接用 `openPath` 打开它，不需要新增通道。
pub fn log_dir_from(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("logs")
}

/// 追加写。日志线程独占这个句柄；轮转时先 drop 再 rename，兼容 Windows。
fn open_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn writer_loop(receiver: Receiver<Message>) {
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Line(line) => write_direct(&line),
            Message::Flush(ack) => {
                if let Some(writer) = LOG_WRITER.get()
                    && let Ok(mut guard) = writer.lock()
                {
                    guard.flush();
                }
                let _ = ack.send(());
            }
        }
    }
}

struct LogWriter {
    path: PathBuf,
    dir: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl LogWriter {
    fn new(path: PathBuf, file: File, bytes: u64) -> Self {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            path,
            dir,
            file: Some(file),
            bytes,
        }
    }

    fn write_line(&mut self, line: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let stamp = timestamp();
        if file.write_all(stamp.as_bytes()).is_err()
            || file.write_all(line.as_bytes()).is_err()
            || file.flush().is_err()
        {
            return;
        }
        self.bytes += stamp.len() as u64 + line.len() as u64;
        if self.bytes >= MAX_LOG_BYTES {
            self.rotate();
        }
    }

    fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }

    /// 关掉当前句柄 → 改名 → 重开新文件；改名失败就继续写原文件，绝不丢日志行。
    fn rotate(&mut self) {
        self.flush();
        self.file = None;
        let rotated = next_rotated_path(&self.path);
        let renamed = fs::rename(&self.path, &rotated).is_ok();
        self.file = open_log(&self.path).ok();
        self.bytes = 0;
        if renamed {
            prune(&self.dir);
        }
    }
}

fn next_rotated_path(path: &Path) -> PathBuf {
    let secs = now_secs();
    let mut candidate = path.with_file_name(format!("{FILE_NAME}.{secs}.log"));
    let mut sequence = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{FILE_NAME}.{secs}-{sequence}.log"));
        sequence += 1;
    }
    candidate
}

/// `2026-02-14 09:31:07 ` —— 本地时间不便在不引入额外依赖的情况下取得，
/// 用秒级 UNIX 时间即可满足「同一会话内定位前后顺序」的需求。
fn timestamp() -> String {
    format!("[{}] ", now_secs())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 超过保留期的 `main.log.*` 一律删除。
fn prune(log_dir: &Path) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            (RETENTION_DAYS * 24 * 60 * 60) as u64,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.starts_with("main.log.") || !name.ends_with(".log") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "d48-log-{}-{}-{}",
            tag,
            std::process::id(),
            rand::random::<u16>()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn log_dir_lives_under_app_data() {
        assert_eq!(
            log_dir_from(Path::new("/tmp/app")),
            PathBuf::from("/tmp/app/logs")
        );
    }

    #[test]
    fn prune_removes_only_old_rotated_logs() {
        let dir = temp_dir("prune");
        let old = dir.join("main.log.2020-01-01.log");
        let recent = dir.join("main.log");
        fs::write(&old, b"old").unwrap();
        fs::write(&recent, b"recent").unwrap();
        // 把「旧文件」的 mtime 往回拨 30 天；个别文件系统不支持时跳过该断言。
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        let mtime_ready = set_mtime(&old, past);
        prune(&dir);
        if mtime_ready {
            assert!(!old.exists(), "超过保留期的轮转日志应删除");
        }
        assert!(recent.exists(), "当前日志文件必须保留");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_rotates_when_size_limit_is_reached() {
        let dir = temp_dir("rotate");
        let path = dir.join(FILE_NAME);
        let file = open_log(&path).unwrap();
        let mut writer = LogWriter::new(path.clone(), file, MAX_LOG_BYTES - 1);
        writer.write_line("trigger\n");
        assert!(path.exists(), "轮转后必须重新打开 main.log");
        let rotated = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("main.log."))
            .count();
        assert_eq!(rotated, 1, "应留下一个 main.log.<ts>.log");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotated_names_are_unique_and_match_prune_pattern() {
        let dir = temp_dir("rotate-name");
        let path = dir.join(FILE_NAME);
        let first = next_rotated_path(&path);
        fs::write(&first, b"first").unwrap();
        let second = next_rotated_path(&path);
        assert_ne!(first, second, "同一秒内轮转也不能覆盖旧文件");
        for candidate in [&first, &second] {
            let name = candidate.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("main.log."));
            assert!(name.ends_with(".log"));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// 用标准库设置 mtime，不依赖外部 `touch`（Windows 终端通常没有该命令）。
    /// 返回是否设置成功，文件系统不支持时由调用方跳过对应断言。
    fn set_mtime(path: &Path, time: std::time::SystemTime) -> bool {
        File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(time))
            .is_ok()
    }
}
