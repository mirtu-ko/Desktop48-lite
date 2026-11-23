//! 播放器侧零散通道：下架广播中转、B 站弹幕、B 站房间信息。
//!
//! 播放窗与主窗不共享 EventBus，跨窗事件必须经主进程转发。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::{Value, json};

use super::super::{BridgeContext, arg, emit_to_window_label, to_value};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // 播放窗 → 主进程 → 主窗口：跨渲染进程的 live-unavailable 中转。
        // 两个窗口不共享 EventBus，必须经主进程转发，列表页才能刷新。
        "notifyLiveUnavailable" => {
            let live_id: String = arg(args, 0)?;
            ctx.broadcast("liveUnavailable", json!(live_id));
            Ok(Value::Null)
        }

        // ===== B 站直播弹幕 =====
        // false = 握手失败（房间号取不到 / WBI 被拒 / 网络异常），渲染层据此提示一次
        "danmakuStart" => {
            let room_id: u64 = arg(args, 0)?;
            let subscriber = caller
                .map(|window| window.label().to_string())
                .unwrap_or_else(|| "main".to_string());
            match ctx
                .danmaku_sessions
                .start(room_id, ctx.bilibili_http.clone())
                .await
            {
                Ok(_) => {
                    // 起一条转发任务：批次推给发起窗口
                    if let Some(mut receiver) = ctx.danmaku_sessions.subscribe(room_id).await {
                        let app = ctx.app_handle().ok();
                        let target = subscriber.clone();
                        tokio::spawn(async move {
                            loop {
                                match receiver.recv().await {
                                    Ok(batch) => {
                                        if let Some(app) = &app {
                                            emit_to_window_label(
                                                app,
                                                &target,
                                                "danmakuBatch",
                                                &batch,
                                            );
                                        }
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                        continue;
                                    }
                                    Err(_) => return,
                                }
                            }
                        });
                    }
                    Ok(Value::Bool(true))
                }
                Err(err) => {
                    crate::app_log::log_line(&format!("[danmaku] 房间 {room_id} 握手失败: {err}"));
                    Ok(Value::Bool(false))
                }
            }
        }
        "danmakuStop" => {
            let room_id: u64 = arg(args, 0)?;
            ctx.danmaku_sessions.stop(room_id).await;
            Ok(Value::Null)
        }
        "bilibiliRoomInfos" => to_value(&ctx.bilibili.room_infos(&ctx.bilibili_http).await),

        other => Err(format!("未实现的通道: {other}")),
    }
}
