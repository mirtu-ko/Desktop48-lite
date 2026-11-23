//! 口袋直播弹幕：聊天室会话订阅 + 直播复盘。
//!
//! 事件经 `pocketDanmakuBatch` 只推给订阅了该聊天室的窗口。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::Value;

use super::super::{BridgeContext, arg, emit_to_window_label, to_value};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // false = 未登录 / 凭据失效 / 参数无效；事件经 pocketDanmakuBatch 推给订阅窗口
        "pocketDanmakuStart" => {
            let chatroom_id: String = arg(args, 0)?;
            let _live_id: String = arg(args, 1)?;
            let subscriber = caller
                .map(|window| window.label().to_string())
                .unwrap_or_else(|| "main".to_string());
            let profile = ctx.pocket.auth_state().client_profile;
            // 未登录直接 false（不打扰用户）；已登录但凭据取不到同样降级为无会话
            let credentials = if ctx.pocket.authorized() {
                match ctx.pocket.im_credentials().await {
                    Ok(pair) => Some(pair),
                    Err(err) => {
                        crate::app_log::log_line(&format!(
                            "[pocket-danmaku] 获取 IM 凭据失败: {err}"
                        ));
                        None
                    }
                }
            } else {
                None
            };
            let device_id = ctx.device_id.clone();
            // 事件转发是长任务：只捕获 AppHandle 与 label，不借用 self / 局部变量
            let app = ctx.app_handle().ok();
            let target = subscriber.clone();
            let started = ctx.danmaku.start(
                &chatroom_id,
                &subscriber,
                credentials,
                &profile,
                &device_id,
                move |batch| {
                    if let Some(app) = &app {
                        emit_to_window_label(app, &target, "pocketDanmakuBatch", &batch);
                    }
                },
            )?;
            Ok(Value::Bool(started))
        }
        // 直播 ID → 聊天室 ID（与 Electron `getChatroomId` 同源）
        "pocketGetChatroomId" => {
            let live_id: String = arg(args, 0)?;
            let chatroom_id = ctx.pocket.live_chatroom_id(&live_id).await?;
            Ok(Value::String(chatroom_id.unwrap_or_default()))
        }
        // 直播复盘（下播后的总观看 / 时长 / 礼物流水 / 榜单）
        "pocketGetLiveResult" => {
            let live_id: String = arg(args, 0)?;
            ctx.pocket.live_result(&live_id).await
        }
        "pocketDanmakuStop" => {
            let chatroom_id: String = arg(args, 0)?;
            let subscriber = caller
                .map(|window| window.label().to_string())
                .unwrap_or_else(|| "main".to_string());
            ctx.danmaku.stop(&chatroom_id, &subscriber);
            Ok(Value::Null)
        }

        // 房间相册批量下载：目录只在 Rust 侧弹一次，随后逐张请求并写盘。
        // 进度经 roomAlbumDownloadProgress 推给发起窗口（与原版 sendIpc 同口径）。
        "roomAlbumDownload" => {
            let sources: Vec<crate::room_album::AlbumSource> = arg(args, 0)?;
            let filter_small: bool = arg(args, 1).unwrap_or(false);
            let app = ctx.app.get().cloned();
            let picked = match app {
                Some(app) => {
                    let directory = tokio::task::spawn_blocking(move || {
                        use tauri_plugin_dialog::DialogExt;
                        app.dialog()
                            .file()
                            .set_title("选择相册图片保存目录")
                            .blocking_pick_folder()
                    })
                    .await
                    .map_err(|err| format!("目录选择失败: {err}"))?;
                    match directory {
                        Some(path) => Some(
                            path.into_path()
                                .map_err(|err| format!("目录路径无效: {err}"))?,
                        ),
                        None => None,
                    }
                }
                None => None,
            };
            let caller_label = caller.map(|window| window.label().to_string());
            let app = ctx.app.get().cloned();
            let result = crate::room_album::run_download(
                &ctx.bilibili_http,
                &sources,
                filter_small,
                picked,
                |progress| {
                    if let (Some(app), Some(label)) = (app.as_ref(), caller_label.as_ref()) {
                        emit_to_window_label(app, label, "roomAlbumDownloadProgress", &progress);
                    }
                },
            )
            .await;
            to_value(&result)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
