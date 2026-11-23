//! 独立播放窗：创建、载荷回取、按比例定形、置顶策略。
//!
//! 窗口控制按发起窗口定位，播放窗与主窗共用同一份渲染层。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::{Value, json};
use tauri::Manager;

use super::super::{BridgeContext, arg, to_value};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // ===== 独立播放窗（对照 register-float-window-ipc.ts） =====
        "openFloatWindow" => {
            let kind: String = arg(args, 0)?;
            if kind != "live" && kind != "playback" {
                return Err(format!("未知的播放窗类型: {kind}"));
            }
            let payload: crate::float_window::FloatPlayerPayload = arg(args, 1)?;
            let app = ctx
                .app
                .get()
                .cloned()
                .ok_or_else(|| "独立播放窗仅在桌面应用内可用".to_string())?;
            let preference = ctx
                .db
                .with(|db| db.config("floatTopmost"))
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "playing".to_string());
            crate::float_window::open(&app, &ctx.floats, &kind, payload, &preference)?;
            Ok(Value::Null)
        }
        // 播放窗 reload 后仍可用（按 kind:liveId 查表）
        "floatPlayerGetPayload" => {
            let kind: String = arg(args, 0)?;
            let live_id: String = arg(args, 1)?;
            match ctx.floats.payload(&kind, &live_id) {
                Some(payload) => to_value(&payload),
                None => Ok(Value::Null),
            }
        }
        // 视频宽高比 → 按比例定形（用户手动缩放过则不再打扰）
        "floatWindowFitAspect" => {
            let aspect: f64 = arg(args, 0)?;
            let Some(window) = caller else {
                return Ok(Value::Null);
            };
            let current = window
                .inner_size()
                .map(|size| {
                    let scale = window.scale_factor().unwrap_or(1.0);
                    crate::float_window::Size {
                        w: size.width as f64 / scale,
                        h: size.height as f64 / scale,
                    }
                })
                .unwrap_or(crate::float_window::Size { w: 0.0, h: 0.0 });
            if !ctx.floats.fit_aspect(window.label(), aspect) {
                return Ok(Value::Null);
            }
            let area = crate::float_window::anchor_area(&ctx.app_handle()?);
            let size = crate::float_window::compute_initial_window_size(aspect, area);
            // 保持右上角不动：变宽时向左扩，不会顶出屏幕。
            // 位置与尺寸都用**逻辑像素（DIP）**：inner_size 是物理像素，
            // 直接混用会在 2x 屏上得到双倍尺寸。
            let position = window
                .outer_position()
                .map_err(|err| format!("读取窗口位置失败: {err}"))?;
            let scale = window.scale_factor().unwrap_or(1.0);
            let x = position.x as f64 / scale + current.w - size.w;
            let y = position.y as f64 / scale;
            let x = x
                .max(area.x)
                .min((area.x + area.width - size.w).max(area.x));
            let y = y
                .max(area.y)
                .min((area.y + area.height - size.h).max(area.y));
            let _ = window.set_position(tauri::LogicalPosition::new(x, y));
            // 顺序要紧：先把目标尺寸记成「程序设置的尺寸」，再改窗口，最后落定。
            // `Resized` 事件是异步送达的，此刻 record 里已经是新尺寸，
            // note_resized 才能正确分辨「这次变化是程序干的」而不是用户拖的。
            ctx.floats.set_programmatic_size(window.label(), size);
            let _ = window.set_size(tauri::LogicalSize::new(size.w, size.h));
            ctx.floats.decide_auto_fit(window.label());
            Ok(Value::Null)
        }
        // 播放状态 → 目标置顶态（策略 × 播放状态，只此一处判定）
        "floatWindowSetPlaying" => {
            let playing: bool = arg(args, 0)?;
            let Some(window) = caller else {
                return Ok(Value::Null);
            };
            if let Some(desired) = ctx.floats.set_playing(window.label(), playing) {
                let _ = window.set_always_on_top(desired);
            }
            Ok(Value::Null)
        }
        // 图钉初值：本窗当前策略
        "floatWindowGetTopmost" => {
            let Some(window) = caller else {
                return Ok(Value::Null);
            };
            match ctx.floats.topmost_of(window.label()) {
                Some(topmost) => Ok(Value::String(topmost)),
                None => Ok(Value::Null),
            }
        }
        // 图钉切换：只改本窗、不落盘（关窗即失效）
        "floatWindowSetTopmost" => {
            let topmost: String = arg(args, 0)?;
            if !["always", "playing", "never"].contains(&topmost.as_str()) {
                return Ok(Value::Null);
            }
            let Some(window) = caller else {
                return Ok(Value::Null);
            };
            if let Some((changed, desired)) = ctx.floats.set_topmost(window.label(), &topmost) {
                if changed {
                    let _ = window.set_always_on_top(desired);
                }
                ctx.broadcast_to(window.label(), "windowOnTopmostChange", json!(topmost));
            }
            Ok(Value::Null)
        }
        // 全局偏好覆盖各窗的临时图钉
        "floatWindowSetTopmostPreference" => {
            let topmost: String = arg(args, 0)?;
            ctx.db.with(|db| db.set_config("floatTopmost", &topmost))?;
            let updates = ctx.floats.apply_preference(&topmost);
            if let Some(app) = ctx.app.get() {
                for (label, desired) in updates {
                    if let Some(window) = app.get_webview_window(&label) {
                        let _ = window.set_always_on_top(desired);
                    }
                }
            }
            ctx.apply_float_topmost(&topmost);
            Ok(Value::Null)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
