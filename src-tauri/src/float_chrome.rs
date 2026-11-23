//! 播放窗外观收尾：藏掉 macOS 系统交通灯（Tauri 未暴露该开关）。
//!
//! `traffic_light_position` 只在重绘时生效、`minimizable(false)` 只禁用不隐藏，
//! 透明播放窗上两个都留了按钮，所以直接调 `standardWindowButton().setHidden(true)`。

/// 必须在窗口上屏后调用；调用方在 `Resized` 时补调一次（AppKit 会复位按钮）。
#[cfg(target_os = "macos")]
pub fn hide_macos_traffic_lights(window: &tauri::WebviewWindow<tauri::Wry>) {
    use objc2_app_kit::{NSWindow, NSWindowButton};
    use tauri::Manager as _;

    let Ok(raw) = window.ns_window() else {
        return;
    };
    let ptr = raw as usize;
    let app = window.app_handle().clone();
    let _ = app.run_on_main_thread(move || {
        // SAFETY: 指针来自 `ns_window()`，此时已在主线程，窗口存活期间有效。
        let ns_window: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
        for button in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(view) = ns_window.standardWindowButton(button) {
                view.setHidden(true);
            }
        }
    });
}

/// 非 macOS 空实现，调用点无需 `cfg`。
#[cfg(not(target_os = "macos"))]
pub fn hide_macos_traffic_lights(_window: &tauri::WebviewWindow<tauri::Wry>) {}
