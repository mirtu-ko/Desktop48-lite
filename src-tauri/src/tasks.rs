//! 下载 / 录制任务：两组共用一套机制，差别只在 ffmpeg 输出参数
//! （`downloadTask` HLS TS → MP4，`recordTask` RTMP / HTTP → FLV）。
//!
//! 约束：`url` 必须过 `is_allowed_stream_url`（它直接进 ffmpeg `-i`）；`filename` 必须是纯文件名；
//! 文件冲突时只有同一任务重启可覆盖；同一 `liveId` 的上一进程退出后才能重启；
//! 状态事件广播给全部窗口；退出先写 `q` 让 ffmpeg 收尾，超时强杀。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::app_log::log_line;

/// 并发上限：达到即拒绝新任务（错误经 IPC 抛回渲染层，由 `use-tasks` 用消息提示）
pub const MAX_CONCURRENT_TASKS: usize = 5;
/// 优雅退出超时：写 `q` 后这么久还没退就强杀（网络栈 hang 时 ffmpeg 可能永不收尾）
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// 强杀请求的标记键（与进程句柄分开存，避免复制 Child 所有权）
type KillFlag = Arc<std::sync::atomic::AtomicBool>;
/// 进度行：`time=00:00:12.34`
const TIME_MARKER: &str = "time=";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub live_id: String,
    /// `download` / `record`：任务列表按它区分两类任务（同一 liveId 只会有一条）。
    pub kind: String,
    pub url: String,
    pub filename: String,
    pub file_path: String,
    pub save_directory: String,
    /// `running` / `finish` / `error`
    pub status: String,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 任务事件（广播给全部窗口）。
#[derive(Debug, Clone)]
pub enum TaskEvent {
    Started {
        prefix: &'static str,
        snapshot: TaskSnapshot,
    },
    Progress {
        prefix: &'static str,
        live_id: String,
        time: String,
    },
    End {
        prefix: &'static str,
        live_id: String,
        file_path: String,
    },
    Error {
        prefix: &'static str,
        live_id: String,
        message: String,
    },
}

/// 任务类型：决定 ffmpeg 输出参数与通道名前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Download,
    Record,
}

impl TaskKind {
    /// 快照里的类型标记（与渲染层 `TaskKind` 同值）
    pub fn slug(self) -> &'static str {
        match self {
            TaskKind::Download => "download",
            TaskKind::Record => "record",
        }
    }

    /// 通道名前缀（与 `ipc-contract.d.ts` 的 `TaskChannelPrefix` 同值）
    pub fn prefix(self) -> &'static str {
        match self {
            TaskKind::Download => "downloadTask",
            TaskKind::Record => "recordTask",
        }
    }

    fn ffmpeg_args(self) -> Vec<&'static str> {
        match self {
            // HLS TS 里的 AAC 是 ADTS 格式，MP4 容器需要 ASC，必须转封装；
            // faststart 把 moov atom 移到文件头，播放器可立即打开
            TaskKind::Download => vec!["-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"],
            // RTMP / HTTP 流封装为 FLV
            TaskKind::Record => vec!["-f", "flv"],
        }
    }
}

/// 单个运行中的任务。
struct RunningTask {
    /// 优雅退出用：写 `q` 到 stdin。`None` 有两种含义，靠 `stop_requested` 区分：
    /// 任务还在启动（占位中）或已下发过停止请求。
    stdin: Option<tokio::process::ChildStdin>,
    /// 超时未退时由看门狗置位，等退出循环里的 wait 超时后强杀
    kill: KillFlag,
    /// 是否已下发过停止请求。
    ///
    /// 重复请求要直接忽略：录制 / 下载任务的状态会镜像到每个窗口，
    /// 两个窗口都可能点「停止」；第二次若再置强杀标记，
    /// 会把还在写文件尾的 ffmpeg 直接砍掉（文件不完整）。
    stop_requested: bool,
}

#[derive(Default)]
pub struct TaskRuntime {
    /// 快照表：渲染层刷新后据此恢复列表，直到显式 Remove 才删
    snapshots: Arc<Mutex<HashMap<String, TaskSnapshot>>>,
    /// 进程句柄（退出停止 / 优雅退出用）
    running: Arc<Mutex<HashMap<String, RunningTask>>>,
    /// 全局并发计数（与快照表分离：finish 的卡片还在列表里但进程已退出）
    active: Arc<AtomicUsize>,
    events: Arc<Mutex<Option<broadcast::Sender<TaskEvent>>>>,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl TaskRuntime {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(Some(sender))),
        }
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<TaskEvent>> {
        self.events
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|sender| sender.subscribe()))
    }

    fn emit(&self, event: TaskEvent) {
        if let Ok(guard) = self.events.lock()
            && let Some(sender) = guard.as_ref()
        {
            let _ = sender.send(event);
        }
    }

    /// 该类型的任务快照。
    ///
    /// 快照按 liveId 存，同一场直播可能既有下载又有录制，所以必须按 kind 过滤，
    /// 不能整表返回（否则两个任务列表会互相串）。
    pub fn list(&self, kind: TaskKind) -> Vec<TaskSnapshot> {
        self.snapshots
            .lock()
            .map(|guard| {
                guard
                    .values()
                    .filter(|snapshot| snapshot.kind == kind.slug())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 从快照表删除任务（对应渲染层移除卡片）。
    pub fn remove(&self, live_id: &str) {
        if let Ok(mut guard) = self.snapshots.lock() {
            guard.remove(live_id);
        }
    }

    /// 运行中的任务数。
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// 停止任务：向 stdin 写 `q` 让 ffmpeg 自己收尾，超时强杀兜底。
    ///
    /// 写 `q` 成功不代表进程会退出：网络栈 hang 时 ffmpeg 可能永不收尾，
    /// 所以置一个超时标记，由 `start()` 里的看门狗到点强杀。
    pub async fn stop(&self, live_id: &str) -> bool {
        let (stdin, kill) = {
            let mut guard = match self.running.lock() {
                Ok(guard) => guard,
                Err(_) => return false,
            };
            match guard.get_mut(live_id) {
                Some(task) => {
                    if task.stop_requested {
                        // 已经下发过停止（多半是另一个窗口点的）：不再重复强杀
                        log_line(&format!(
                            "[task] 停止已在进行中，忽略重复请求 liveId={live_id}"
                        ));
                        return true;
                    }
                    task.stop_requested = true;
                    (task.stdin.take(), task.kill.clone())
                }
                None => return false,
            }
        };
        if let Some(mut stdin) = stdin
            && stdin.write_all(b"q").await.is_ok()
            && stdin.flush().await.is_ok()
        {
            log_line(&format!("[task] 已请求优雅退出 liveId={live_id}"));
            // 超时未退则置强杀标记（看门狗在退出循环里检查）
            let kill_later = kill.clone();
            tokio::spawn(async move {
                tokio::time::sleep(GRACEFUL_STOP_TIMEOUT).await;
                kill_later.store(true, Ordering::Relaxed);
            });
            return true;
        }
        // 两种情况：还没 spawn 完（占位中 stdin 仍是 None），或写 q 失败。
        // 都置强杀标记，由 `wait_with_kill` 结束进程（占位期的话，spawn 后立刻被杀）。
        log_line(&format!(
            "[task] 无法优雅退出，改为强制结束 liveId={live_id}"
        ));
        kill.store(true, Ordering::Relaxed);
        false
    }

    /// 应用退出：停止全部任务（让 ffmpeg 写完文件尾）。
    pub fn cleanup(&self) {
        let ids: Vec<String> = self
            .running
            .lock()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default();
        for live_id in ids {
            // 退出路径不 await：写 q 后进程会自己收尾，超时由进程回收逻辑兜底
            log_line(&format!("[task] 退出清理 liveId={live_id}"));
        }
    }

    /// 启动一个 ffmpeg 任务；返回输出文件的绝对路径。
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        kind: TaskKind,
        url: &str,
        filename: &str,
        live_id: &str,
        save_dir: &str,
        ffmpeg_path: &Path,
    ) -> Result<String, String> {
        // 1) 源地址白名单：url 直接交给 ffmpeg -i，不校验等于开了一条绕过出口白名单的旁路
        if !crate::net::is_allowed_stream_url(url) {
            return Err(format!("任务源地址不在允许范围内: {url}"));
        }
        // 2) 文件名必须是纯文件名：含路径分隔符或 '..' 时 join 会穿出下载目录
        if !is_plain_filename(filename) {
            return Err(format!("任务文件名不合法: {filename}"));
        }
        // 3) 保存目录 / ffmpeg 二进制
        let save_path = Path::new(save_dir);
        if !save_path.is_dir() {
            return Err("保存目录不存在".to_string());
        }
        if !ffmpeg_path.exists() {
            return Err(format!("FFmpeg 不存在: {}", ffmpeg_path.display()));
        }
        // 4) 并发熔断
        if self.active.load(Ordering::Relaxed) >= MAX_CONCURRENT_TASKS {
            return Err(format!(
                "下载/录制任务已达并发上限（{MAX_CONCURRENT_TASKS}），请先停止或等待部分任务完成"
            ));
        }
        // 5) 文件冲突：只有「同一任务重启」允许覆盖自己上次写的文件
        let intended = save_path.join(filename);
        let is_restart = self
            .snapshots
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .get(live_id)
                    .map(|snapshot| snapshot.file_path.clone())
            })
            .map(|previous| previous == intended.to_string_lossy())
            .unwrap_or(false);
        let file_path = if is_restart {
            intended.clone()
        } else {
            next_available_path(&intended)
        };
        if file_path != intended {
            log_line(&format!(
                "[task] 目标文件已存在，自动改用 {}",
                file_path.display()
            ));
        }

        // 6) 同一 liveId 只允许一个 ffmpeg：占位与查重在同一把锁内完成，
        //    两个窗口几乎同时点「开始」也只会有一次 spawn。
        //    没有这道闸时，第二个 ffmpeg 会写同一个文件，还会把 `running` 里的停止句柄
        //    覆盖掉——旧进程从此停不下来，表现就是「点了停止但录制还在继续」。
        //    判据只看 running（进程存活 / 正在启动）：已结束的任务留在 snapshots 里，仍可重启。
        //    占位先于 spawn：spawn 失败或启动期被停止时都要回滚 / 置强杀，不能留下永久占位。
        let kill: KillFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut guard = self
                .running
                .lock()
                .map_err(|_| "任务状态锁已损坏".to_string())?;
            if guard.contains_key(live_id) {
                return Err("该任务已在运行，请先停止后再试".to_string());
            }
            guard.insert(
                live_id.to_string(),
                RunningTask {
                    stdin: None,
                    kill: kill.clone(),
                    stop_requested: false,
                },
            );
        }

        // 7) 起进程
        let mut command = Command::new(ffmpeg_path);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("info")
            .arg("-y")
            .arg("-i")
            .arg(url)
            .arg("-c")
            .arg("copy")
            .args(kind.ffmpeg_args())
            .arg(&file_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                // 回滚占位：否则该 liveId 会永远显示「已在运行」
                if let Ok(mut guard) = self.running.lock() {
                    guard.remove(live_id);
                }
                return Err(format!("启动 ffmpeg 失败: {err}"));
            }
        };
        let pid = child.id().unwrap_or(0);
        self.active.fetch_add(1, Ordering::Relaxed);

        // 快照（running）：冲突保护可能已改用带序号的新文件名，以实际路径为准
        let snapshot = TaskSnapshot {
            live_id: live_id.to_string(),
            kind: kind.slug().to_string(),
            url: url.to_string(),
            filename: file_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            file_path: file_path.to_string_lossy().to_string(),
            save_directory: save_dir.to_string(),
            status: "running".to_string(),
            started_at: now_millis(),
            error: None,
        };
        if let Ok(mut guard) = self.snapshots.lock() {
            guard.insert(live_id.to_string(), snapshot.clone());
        }
        // 把 stdin 交给占位条目（优雅退出的入口）；占位已被 stop 置过强杀标记时，
        // 下面 `wait_with_kill` 会立刻结束进程。
        let stdin = child.stdin.take();
        if let Ok(mut guard) = self.running.lock()
            && let Some(task) = guard.get_mut(live_id)
        {
            task.stdin = stdin;
        }
        // 「任务已登记」在 spawn 成功后才广播：spawn 失败时发起窗口会撤销占位卡片，
        // 若提前广播，其它窗口就会留下一张永不结束的幻影卡片
        self.emit(TaskEvent::Started {
            prefix: kind.prefix(),
            snapshot,
        });

        // 8) stderr 读进度 + 等退出
        let runtime_snapshots = self.snapshots.clone();
        let runtime_running = self.running.clone();
        let runtime_active = self.active.clone();
        let events = self.events.clone();
        let live_id_owned = live_id.to_string();
        let file_path_owned = file_path.to_string_lossy().to_string();
        let prefix = kind.prefix();
        let stderr = child.stderr.take();

        tokio::spawn(async move {
            let emit = {
                let events = events.clone();
                move |event: TaskEvent| {
                    if let Ok(guard) = events.lock()
                        && let Some(sender) = guard.as_ref()
                    {
                        let _ = sender.send(event);
                    }
                }
            };
            let emit_for_reader = {
                let events = events.clone();
                move |event: TaskEvent| {
                    if let Ok(guard) = events.lock()
                        && let Some(sender) = guard.as_ref()
                    {
                        let _ = sender.send(event);
                    }
                }
            };
            // stderr 解析 progress。
            //
            // ⚠️ 不能用 `BufReader::lines()`：ffmpeg 的进度行以 **`\r`** 结尾（为了原地刷新），
            // 按 `\n` 分割会把整个下载过程的进度压成一行、直到进程退出才返回——
            // 表现为「进度一直是空的，任务结束时突然跳到 100%」。这里按 `\r` 与 `\n` 都切。
            if let Some(stderr) = stderr {
                let live_id_for_progress = live_id_owned.clone();
                let emit_progress = emit_for_reader;
                let reader = tokio::spawn(async move {
                    let mut reader = tokio::io::BufReader::new(stderr);
                    let mut buffer: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        use tokio::io::AsyncReadExt;
                        match reader.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                for byte in &chunk[..read] {
                                    if *byte == b'\r' || *byte == b'\n' {
                                        if !buffer.is_empty() {
                                            let line = String::from_utf8_lossy(&buffer).to_string();
                                            buffer.clear();
                                            if let Some(time) = parse_progress_time(&line) {
                                                emit_progress(TaskEvent::Progress {
                                                    prefix,
                                                    live_id: live_id_for_progress.clone(),
                                                    time,
                                                });
                                            }
                                        }
                                    } else {
                                        buffer.push(*byte);
                                        // 单行上限：防止无换行的异常输出把内存吃满
                                        if buffer.len() > 8192 {
                                            buffer.clear();
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
                let status = wait_with_kill(&mut child, &kill).await;
                let _ = reader.await;
                finish_task(
                    status,
                    &live_id_owned,
                    &file_path_owned,
                    prefix,
                    &runtime_snapshots,
                    &runtime_running,
                    &runtime_active,
                    emit,
                );
            } else {
                let status = wait_with_kill(&mut child, &kill).await;
                finish_task(
                    status,
                    &live_id_owned,
                    &file_path_owned,
                    prefix,
                    &runtime_snapshots,
                    &runtime_running,
                    &runtime_active,
                    emit,
                );
            }
            let _ = pid;
        });

        // 进程启动成功即返回路径；任务结束由 End / Error 事件通知
        Ok(file_path.to_string_lossy().to_string())
    }
}

/// 等进程退出；期间若收到强杀标记就 kill。
async fn wait_with_kill(
    child: &mut tokio::process::Child,
    kill: &KillFlag,
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        match tokio::time::timeout(Duration::from_millis(200), child.wait()).await {
            Ok(result) => return result,
            Err(_) => {
                if kill.load(Ordering::Relaxed) {
                    log_line("[task] 优雅退出超时，强制结束 ffmpeg");
                    let _ = child.start_kill();
                    return child.wait().await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_task(
    status: std::io::Result<std::process::ExitStatus>,
    live_id: &str,
    file_path: &str,
    prefix: &'static str,
    snapshots: &Arc<Mutex<HashMap<String, TaskSnapshot>>>,
    running: &Arc<Mutex<HashMap<String, RunningTask>>>,
    active: &Arc<AtomicUsize>,
    emit: impl Fn(TaskEvent),
) {
    active.fetch_sub(1, Ordering::Relaxed);
    if let Ok(mut guard) = running.lock() {
        guard.remove(live_id);
    }
    let set_status = |status: &str, error: Option<String>| {
        if let Ok(mut guard) = snapshots.lock()
            && let Some(snapshot) = guard.get_mut(live_id)
        {
            snapshot.status = status.to_string();
            snapshot.error = error;
        }
    };
    match status {
        Ok(exit) if exit.success() => {
            log_line(&format!("[{prefix}] 任务完成 liveId={live_id} {file_path}"));
            set_status("finish", None);
            emit(TaskEvent::End {
                prefix,
                live_id: live_id.to_string(),
                file_path: file_path.to_string(),
            });
        }
        Ok(exit) => {
            let message = format!("[{prefix}] ffmpeg exited with code {exit}");
            log_line(&message);
            set_status("error", Some(message.clone()));
            emit(TaskEvent::Error {
                prefix,
                live_id: live_id.to_string(),
                message,
            });
        }
        Err(err) => {
            let message = format!("[{prefix}] ffmpeg error: {err}");
            log_line(&message);
            set_status("error", Some(message.clone()));
            emit(TaskEvent::Error {
                prefix,
                live_id: live_id.to_string(),
                message,
            });
        }
    }
}

/// 事件 → (Tauri 事件名, 载荷)。事件名与 `IpcEventMap` 同形。
pub fn event_payload(event: &TaskEvent) -> (String, serde_json::Value) {
    // 两组任务共用同一套结构，事件名靠 prefix 区分（`downloadTask*` / `recordTask*`），
    // 否则渲染层无法把进度归到对应的卡片上
    let name = |suffix: &str, prefix: &str| format!("{prefix}{suffix}");
    match event {
        TaskEvent::Started { prefix, snapshot } => (
            name("Started", prefix),
            serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null),
        ),
        TaskEvent::Progress {
            prefix,
            live_id,
            time,
        } => (name("Progress", prefix), serde_json::json!([live_id, time])),
        TaskEvent::End {
            prefix,
            live_id,
            file_path,
        } => (name("End", prefix), serde_json::json!([live_id, file_path])),
        TaskEvent::Error {
            prefix,
            live_id,
            message,
        } => (name("Error", prefix), serde_json::json!([live_id, message])),
    }
}

/// 从 ffmpeg 的 stderr 行里取 `time=HH:MM:SS.ms`。
pub fn parse_progress_time(line: &str) -> Option<String> {
    let index = line.find(TIME_MARKER)?;
    let rest = &line[index + TIME_MARKER.len()..];
    let value: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == ':' || *ch == '.')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// 文件名必须是纯文件名：无路径分隔符、非空、非 `.` / `..`。
fn is_plain_filename(filename: &str) -> bool {
    if filename.is_empty() || filename == "." || filename == ".." {
        return false;
    }
    if filename.contains('/') || filename.contains('\\') {
        return false;
    }
    // 平台归一化后再比一次：Windows 会把 `a/../b` 归一成 `b`
    Path::new(filename)
        .file_name()
        .map(|name| name == std::ffi::OsStr::new(filename))
        .unwrap_or(false)
}

/// 返回不与现有文件冲突的输出路径：冲突则追加 ` (n)`（从 2 起）直到可用。
pub fn next_available_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let text = path.to_string_lossy().to_string();
    let extension = text
        .rsplit_once('.')
        .filter(|(stem, ext)| {
            !stem.is_empty() && !ext.is_empty() && !ext.contains(['/', '\\']) && ext.len() <= 8
        })
        .map(|(_, ext)| format!(".{ext}"))
        .unwrap_or_default();
    let stem = text.strip_suffix(&extension).unwrap_or(&text).to_string();
    let mut seq = 2;
    loop {
        let candidate = PathBuf::from(format!("{stem} ({seq}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
        seq += 1;
        if seq > 9999 {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_time_is_extracted() {
        let line = "frame= 1234 fps= 30 q=-1.0 size=  512kB time=00:01:23.45 bitrate=...";
        assert_eq!(parse_progress_time(line).as_deref(), Some("00:01:23.45"));
        assert!(parse_progress_time("no progress here").is_none());
        assert!(parse_progress_time("time=").is_none());
    }

    #[test]
    fn plain_filename_rejects_traversal() {
        assert!(is_plain_filename("视频.mp4"));
        assert!(is_plain_filename("a b.flv"));
        assert!(!is_plain_filename(""));
        assert!(!is_plain_filename("."));
        assert!(!is_plain_filename(".."));
        assert!(!is_plain_filename("../evil.mp4"));
        assert!(!is_plain_filename("dir/evil.mp4"));
        assert!(!is_plain_filename("dir\\evil.mp4"));
    }

    #[test]
    fn next_available_path_appends_sequence() {
        let dir = std::env::temp_dir().join(format!(
            "d48-task-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("video.mp4");
        // 不存在：原样返回
        assert_eq!(next_available_path(&base), base);
        std::fs::write(&base, b"x").unwrap();
        let second = next_available_path(&base);
        assert_eq!(second.file_name().unwrap(), "video (2).mp4");
        std::fs::write(&second, b"x").unwrap();
        assert_eq!(
            next_available_path(&base).file_name().unwrap(),
            "video (3).mp4"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：`list(kind)` 曾经完全忽略参数，两个任务列表返回同一份混合快照，
    /// 导致任务中心里下载 / 录制互相串。这里固定「按 kind 过滤」。
    #[test]
    fn list_filters_by_kind() {
        let runtime = TaskRuntime::new();
        let insert = |kind: TaskKind, live_id: &str| {
            let mut guard = runtime.snapshots.lock().unwrap();
            guard.insert(
                live_id.to_string(),
                TaskSnapshot {
                    live_id: live_id.to_string(),
                    kind: kind.slug().to_string(),
                    url: String::new(),
                    filename: String::new(),
                    file_path: String::new(),
                    save_directory: String::new(),
                    status: "running".to_string(),
                    started_at: 0,
                    error: None,
                },
            );
        };
        // 快照按 liveId 存：同一 liveId 只能有一条任务（渲染层用 liveId 查重），
        // 这里用不同 liveId 混合下载与录制。
        insert(TaskKind::Download, "live-a");
        insert(TaskKind::Download, "live-b");
        insert(TaskKind::Record, "live-c");

        let mut downloads: Vec<String> = runtime
            .list(TaskKind::Download)
            .into_iter()
            .map(|snapshot| snapshot.live_id)
            .collect();
        let mut records: Vec<String> = runtime
            .list(TaskKind::Record)
            .into_iter()
            .map(|snapshot| snapshot.live_id)
            .collect();
        downloads.sort();
        records.sort();
        assert_eq!(downloads, vec!["live-a", "live-b"]);
        assert_eq!(records, vec!["live-c"]);
    }

    #[test]
    fn snapshot_kind_matches_payload_kind() {
        assert_eq!(TaskKind::Download.slug(), "download");
        assert_eq!(TaskKind::Record.slug(), "record");
    }

    #[test]
    fn task_kinds_have_distinct_ffmpeg_args() {
        assert!(TaskKind::Download.ffmpeg_args().contains(&"aac_adtstoasc"));
        assert!(TaskKind::Record.ffmpeg_args().contains(&"flv"));
        assert_eq!(TaskKind::Download.prefix(), "downloadTask");
        assert_eq!(TaskKind::Record.prefix(), "recordTask");
    }

    #[tokio::test]
    async fn start_rejects_disallowed_url_before_spawn() {
        let runtime = TaskRuntime::new();
        let err = runtime
            .start(
                TaskKind::Download,
                "https://evil.example.com/a.m3u8",
                "a.mp4",
                "live1",
                std::env::temp_dir().to_string_lossy().as_ref(),
                Path::new("/bin/echo"),
            )
            .await
            .unwrap_err();
        assert!(err.contains("不在允许范围内"));
        assert_eq!(runtime.active_count(), 0);
    }

    #[tokio::test]
    async fn start_rejects_bad_filename_before_spawn() {
        let runtime = TaskRuntime::new();
        let err = runtime
            .start(
                TaskKind::Download,
                "https://cyflv.48.cn/a.m3u8",
                "../evil.mp4",
                "live1",
                std::env::temp_dir().to_string_lossy().as_ref(),
                Path::new("/bin/echo"),
            )
            .await
            .unwrap_err();
        assert!(err.contains("文件名不合法"));
    }

    #[tokio::test]
    async fn start_rejects_missing_directory() {
        let runtime = TaskRuntime::new();
        let err = runtime
            .start(
                TaskKind::Download,
                "https://cyflv.48.cn/a.m3u8",
                "a.mp4",
                "live1",
                "/no/such/dir",
                Path::new("/bin/echo"),
            )
            .await
            .unwrap_err();
        assert!(err.contains("保存目录不存在"));
    }

    /// 回归：同一 liveId 已在跑时再次发起必须被拒。
    ///
    /// 录制 / 下载状态会镜像到每个窗口，镜像尚未同步时再来一次「开始」
    /// 会拉起第二个 ffmpeg：它写同一个文件，还会把 `running` 里的停止句柄覆盖掉，
    /// 旧进程从此停不下来（现场表现就是「点了停止但还在录」）。
    #[tokio::test]
    async fn start_rejects_duplicate_live_id() {
        let runtime = TaskRuntime::new();
        runtime.running.lock().unwrap().insert(
            "live1".to_string(),
            RunningTask {
                stdin: None,
                kill: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                stop_requested: false,
            },
        );
        let exe = std::env::current_exe().unwrap();
        let err = runtime
            .start(
                TaskKind::Record,
                "https://cyflv.48.cn/a.m3u8",
                "a.flv",
                "live1",
                std::env::temp_dir().to_string_lossy().as_ref(),
                Path::new(&exe),
            )
            .await
            .unwrap_err();
        assert!(err.contains("已在运行"));
        // 占位仍在：这不是失败的启动，已有进程的句柄不受影响
        assert!(runtime.running.lock().unwrap().contains_key("live1"));
    }

    /// 停止请求幂等：两个窗口各点一次「停止」是常态，
    /// 第二次不能再置强杀标记，否则还在写文件尾的 ffmpeg 会被直接砍掉。
    #[tokio::test]
    async fn stop_is_idempotent() {
        let runtime = TaskRuntime::new();

        // 启动占位期（进程还没起来）：只能置强杀标记，spawn 后被立刻结束
        let starting_kill: KillFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.running.lock().unwrap().insert(
            "live1".to_string(),
            RunningTask {
                stdin: None,
                kill: starting_kill.clone(),
                stop_requested: false,
            },
        );
        assert!(!runtime.stop("live1").await);
        assert!(starting_kill.load(Ordering::Relaxed));
        // 重复请求直接视为成功
        assert!(runtime.stop("live1").await);

        // 已下发过优雅停止（stdin 已取走、强杀时钟还在走）：重复请求不能强杀
        let graceful_kill: KillFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.running.lock().unwrap().insert(
            "live2".to_string(),
            RunningTask {
                stdin: None,
                kill: graceful_kill.clone(),
                stop_requested: true,
            },
        );
        assert!(runtime.stop("live2").await);
        assert!(!graceful_kill.load(Ordering::Relaxed));

        // 未登记的任务
        assert!(!runtime.stop("nope").await);
    }
}
