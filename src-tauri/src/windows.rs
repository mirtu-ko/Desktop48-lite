//! 主窗口唤回。

use tauri::{AppHandle, Manager};

/// 取消最小化、显示并聚焦主窗口。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}
