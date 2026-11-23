//! 平台差异的唯一出口。
//!
//! 其它模块**不允许**再写 `#[cfg(target_os = ...)]`：需要按平台分叉的行为统统收在这里，
//! 这样「这个项目到底依赖哪些系统能力」可以一个文件看完，砍平台 / 加平台也只动这里。
//!
//! 目前收编的几件小事：防休眠的外部工具、打开外部程序、大小写敏感、
//! 旧 Electron 版的数据目录。

use std::path::PathBuf;
use std::process::{Child, Command};

/// 防休眠句柄：Unix 是外部命令子进程，Windows 是持有系统状态的专用线程。
#[cfg(target_os = "windows")]
pub type SleepBlockerHandle = windows_sleep::Guard;
#[cfg(not(target_os = "windows"))]
pub type SleepBlockerHandle = Child;

/// 请求系统保持唤醒；返回的句柄必须交给 [`release_sleep_blocker`]。
pub fn acquire_sleep_blocker() -> Result<SleepBlockerHandle, String> {
    #[cfg(target_os = "windows")]
    {
        windows_sleep::acquire()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut child = spawn_sleep_blocker()?;
        // 命令立刻退出（缺 caffeinate / systemd-inhibit）不该被当成成功。
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("防休眠命令提前退出: {status}"));
        }
        Ok(child)
    }
}

/// 释放防休眠断言；id 不匹配时由调用方决定，这里只负责清掉传入的句柄。
pub fn release_sleep_blocker(handle: SleepBlockerHandle) {
    #[cfg(target_os = "windows")]
    {
        windows_sleep::release(handle);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut child = handle;
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(target_os = "windows")]
mod windows_sleep {
    use std::sync::mpsc::{Sender, channel};
    use std::thread;
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    /// `SetThreadExecutionState` 是线程级状态：acquire / release 必须在同一线程调用，
    /// 因此由专用线程持有状态，收到释放信号后在同一线程清除。
    pub struct Guard(Sender<()>);

    pub fn acquire() -> Result<Guard, String> {
        let (release_tx, release_rx) = channel::<()>();
        let (result_tx, result_rx) = channel::<Result<(), String>>();
        thread::Builder::new()
            .name("D48 sleep blocker".into())
            .spawn(move || {
                // SAFETY: 只传入本模块定义的位标志，无指针参数。
                let previous = unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
                    )
                };
                let _ = result_tx.send(if previous == 0 {
                    Err("SetThreadExecutionState 调用失败".to_string())
                } else {
                    Ok(())
                });
                let _ = release_rx.recv();
                unsafe {
                    SetThreadExecutionState(ES_CONTINUOUS);
                }
            })
            .map_err(|err| format!("启动防休眠线程失败: {err}"))?;
        result_rx
            .recv()
            .map_err(|_| "防休眠线程未返回结果".to_string())??;
        Ok(Guard(release_tx))
    }

    pub fn release(guard: Guard) {
        let _ = guard.0.send(());
    }
}

/// 启动一个外部防休眠进程（Unix 路径，由 [`acquire_sleep_blocker`] 调用）。
///
/// - macOS：`caffeinate -d`（阻止显示器休眠；系统自带）
/// - Linux：`systemd-inhibit` 阻塞 idle
/// - 其它平台：明确返回不支持，而不是悄悄放行
pub fn spawn_sleep_blocker() -> Result<Child, String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("caffeinate")
            .arg("-d")
            .spawn()
            .map_err(|err| format!("启动 caffeinate 失败: {err}"))
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("systemd-inhibit")
            .args([
                "--what=idle",
                "--who=Desktop48 lite",
                "--why=正在播放",
                "--mode=block",
                "sleep",
                "2147483647",
            ])
            .spawn()
            .map_err(|err| format!("启动 systemd-inhibit 失败: {err}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("当前平台不支持防休眠".to_string())
    }
}

/// 用系统默认程序打开一个路径或 URL（调用方负责先做白名单 / scheme 校验）。
///
/// 参数只通过进程参数传递，不拼 shell，避免路径或 URL 里的 `&` / `|` 被当成命令分隔符。
/// Windows 用 `rundll32`：`cmd /C start` 会把 URL 里的 `&` 当命令分隔符。
pub fn open_with_default_app(target: &str) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", target])
            .status()
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(target).status()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(target).status()
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        Err(std::io::Error::other("当前平台不支持打开外部程序"))
    }
}

/// 文件系统是否按大小写不敏感比较（Windows）。
pub fn path_case_insensitive() -> bool {
    cfg!(target_os = "windows")
}

/// 旧 Electron 版的数据目录（`app.getPath('userData')` 对应位置）。
pub fn legacy_data_dir(legacy_dir_name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    #[cfg(not(any(unix, target_os = "windows")))]
    let base: Option<PathBuf> = None;

    base.map(|base| base.join(legacy_dir_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_data_dir_joins_the_app_name() {
        let dir = legacy_data_dir("Desktop48");
        if let Some(dir) = dir {
            assert!(dir.ends_with("Desktop48"), "{dir:?}");
        }
    }
}
