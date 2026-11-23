//! 房间域（只读）：频道解析、消息历史、QChat 订阅、未读与小房间。
//!
//! 本项目不做房间写操作（发消息 / 删消息 / 问答提交），契约里也没有对应通道。
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
        // ===== 小房间（暗号频道） =====
        // 重新发现成员的小房间；force=true 用于频道刚失效（立刻枚举，不等检测轮）
        "pocketRefreshSmallChannel" => {
            let user_id: i64 = arg(args, 0)?;
            let force: bool = arg(args, 1).unwrap_or(false);
            match ctx
                .unread
                .refresh_small_channel(&ctx.pocket, &ctx.room_cache, user_id, force)
                .await
            {
                Ok(Some((server_id, channel_id))) => Ok(json!({
                    "serverId": server_id,
                    "channelId": channel_id,
                })),
                Ok(None) => Ok(Value::Null),
                Err(err) => Err(err),
            }
        }
        // 作废成员的小房间归属（房主更新后旧频道失效）
        "pocketInvalidateSmallChannel" => {
            let user_id: i64 = arg(args, 0)?;
            ctx.unread.invalidate_small_channel(user_id);
            Ok(Value::Null)
        }
        // 小房间的暗号问题；null = 无门禁
        "pocketGetRoomQuestion" => {
            let channel_id: String = arg(args, 0)?;
            crate::room::room_question(&ctx.pocket, &channel_id).await
        }
        // 提交小房间暗号：服务端校验通过才放行；万能口令由 Rust 直通，不下发到渲染层判断
        "pocketCheckRoomAnswer" => {
            let _channel_id: String = arg(args, 0)?;
            let question_id: String = arg(args, 1)?;
            let answer: String = arg(args, 2)?;
            crate::room::check_room_answer(&ctx.pocket, &question_id, &answer).await
        }

        // 全量快照：渲染层启动时回取一次，之后变化走 pocketUnreadBatch
        "pocketUnreadState" => to_value(&ctx.unread.snapshot()),
        // 标记已读：拉一页最新消息对齐水位并清零该成员未读
        "pocketMarkRead" => {
            let user_id: i64 = arg(args, 0)?;
            let target: String = arg(args, 1).unwrap_or_else(|_| "room".to_string());
            let mark = ctx.unread.mark_read(&ctx.pocket, user_id, &target).await?;
            ctx.publish_unread();
            Ok(mark.unwrap_or(Value::Null))
        }

        "pocketGetRoomChannel" => {
            let user_id: i64 = arg(args, 0)?;
            crate::room::room_channel(&ctx.pocket, user_id).await
        }
        // 批量解析关注成员的房间频道（左侧列表一次性要全部）
        "pocketGetRoomChannels" => {
            let user_ids: Vec<i64> = arg(args, 0)?;
            crate::room::room_channels_batch(&ctx.room_cache, &ctx.pocket, &user_ids).await
        }
        "pocketGetServerChannels" => {
            let server_id: String = arg(args, 0)?;
            let force: bool = arg(args, 1).unwrap_or(false);
            crate::room::server_channels(&ctx.room_cache, &ctx.pocket, &server_id, force).await
        }
        "pocketGetRoomPermissions" => {
            let channel_id: String = arg(args, 0)?;
            crate::room::room_permissions(&ctx.room_cache, &ctx.pocket, &channel_id).await
        }
        "pocketGetRoomMessages" => {
            let channel_id: String = arg(args, 0)?;
            let server_id: String = arg(args, 1)?;
            let next_time: f64 = arg(args, 2).unwrap_or(0.0);
            let scope: String = arg(args, 3).unwrap_or_else(|_| "member".to_string());
            crate::room::room_messages(&ctx.pocket, &channel_id, &server_id, next_time, &scope)
                .await
        }
        "pocketGetRoomEssenceMessages" => {
            let channel_id: String = arg(args, 0)?;
            let server_id: String = arg(args, 1)?;
            let next_time: f64 = arg(args, 2).unwrap_or(0.0);
            crate::room::room_essence_messages(&ctx.pocket, &channel_id, &server_id, next_time)
                .await
        }
        "pocketGetRoomLastMessages" => {
            let server_ids: Vec<String> = arg(args, 0)?;
            crate::room::room_last_messages(&ctx.pocket, &server_ids).await
        }
        // 进房：起 QChat 双会话并订阅频道；事件经 pocketRoomBatch 推给订阅窗口
        "pocketRoomQChatEnter" => {
            let server_id: String = arg(args, 0)?;
            let channel_id: String = arg(args, 1)?;
            let subscriber = caller
                .map(|window| window.label().to_string())
                .unwrap_or_else(|| "main".to_string());
            let profile = ctx.pocket.auth_state().client_profile;
            let credentials = if ctx.pocket.authorized() {
                match ctx.pocket.im_credentials().await {
                    Ok(pair) => Some(pair),
                    Err(err) => {
                        crate::app_log::log_line(&format!(
                            "[pocket-room-qchat] 获取 IM 凭据失败: {err}"
                        ));
                        None
                    }
                }
            } else {
                None
            };
            let device_id = ctx.device_id.clone();
            let http = ctx.bilibili_http.clone();
            let app = ctx.app_handle().ok();
            let target = subscriber.clone();
            ctx.room_qchat
                .enter(
                    &server_id,
                    &channel_id,
                    &subscriber,
                    credentials,
                    &profile,
                    &device_id,
                    http,
                    move |batch| {
                        if let Some(app) = &app {
                            emit_to_window_label(app, &target, "pocketRoomBatch", &batch);
                        }
                    },
                )
                .await
                .map(|result| to_value(&result))
                .and_then(|result| result)
        }
        "pocketRoomQChatLeave" => {
            let server_id: String = arg(args, 0)?;
            let channel_id: String = arg(args, 1)?;
            let subscriber = caller
                .map(|window| window.label().to_string())
                .unwrap_or_else(|| "main".to_string());
            ctx.room_qchat
                .leave(&server_id, &channel_id, &subscriber)
                .await;
            Ok(Value::Null)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
