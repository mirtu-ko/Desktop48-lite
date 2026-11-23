//! 播放防休眠：持有系统级断言，直到显式释放或进程退出。
//!
//! Electron 版用 `powerSaveBlocker.start('prevent-display-sleep')`；Tauri 没有等价 API，
//! 这里按平台持有系统断言（实现统一在 `platform`）：
//! - macOS：`caffeinate -d`（阻止显示器休眠，与 Electron 语义一致）
//! - Linux：`systemd-inhibit --what=idle ... sleep`（阻止空闲休眠）
//! - Windows：`SetThreadExecutionState`（专用线程持有，释放时在同一线程清除）。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::platform::{SleepBlockerHandle, acquire_sleep_blocker, release_sleep_blocker};

struct Inner {
    next_id: u32,
    active: HashMap<u32, SleepBlockerHandle>,
}

/// 单窗口场景下只需一把锁：重复 `prevent` 幂等复用同一 id，
/// `allow` 允许释放任意现存断言（对应 Electron「以 sender 为准、忽略渲染层 id」）。
pub struct SleepBlocker {
    inner: Mutex<Inner>,
}

impl SleepBlocker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 1,
                active: HashMap::new(),
            }),
        }
    }

    /// 阻止休眠；已在阻止时返回同一 id。
    pub fn prevent(&self) -> Result<u32, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "防休眠状态锁已损坏".to_string())?;
        if let Some((&id, _)) = inner.active.iter().next() {
            return Ok(id);
        }
        let handle = acquire_sleep_blocker()?;
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        inner.active.insert(id, handle);
        Ok(id)
    }

    /// 释放断言。id 不匹配时仍释放现存断言，避免页面刷新后旧 id 造成永久不休眠。
    pub fn allow(&self, id: u32) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let handle = inner
            .active
            .remove(&id)
            .or_else(|| inner.active.drain().map(|(_, handle)| handle).next());
        if let Some(handle) = handle {
            release_sleep_blocker(handle);
        }
    }

    /// 应用退出兜底：杀掉所有仍存活的断言命令。
    pub fn cleanup(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for (_, handle) in inner.active.drain() {
            release_sleep_blocker(handle);
        }
    }
}

impl Default for SleepBlocker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn prevent_is_idempotent_and_allow_releases() {
        let blocker = SleepBlocker::new();
        let first = blocker.prevent().expect("caffeinate 应可用");
        let second = blocker.prevent().expect("重复请求应幂等");
        assert_eq!(first, second);
        assert!(blocker.inner.lock().unwrap().active.contains_key(&first));
        blocker.allow(second);
        assert!(blocker.inner.lock().unwrap().active.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn allow_with_stale_id_still_releases() {
        let blocker = SleepBlocker::new();
        let id = blocker.prevent().expect("caffeinate 应可用");
        blocker.allow(id.saturating_add(99));
        assert!(blocker.inner.lock().unwrap().active.is_empty());
    }

    #[test]
    fn cleanup_is_safe_when_idle() {
        let blocker = SleepBlocker::new();
        blocker.cleanup();
    }
}
