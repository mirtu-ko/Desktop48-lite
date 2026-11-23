//! 设置页与账号：路径拼接、版本 / 反馈、直播转流、登录签到、窗口壳。
//!
//! 自动更新（检查 / 下载 / 安装）已整块移除：设置页只保留一个跳 GitHub Releases 的静态入口，
//! 该跳转走通用的 `openExternal` 通道，不需要这里再单独提供更新通道。
//!
//! 账号通道只由主窗发起（`FLOAT_WINDOW_CHANNELS` 里没有登录写操作）。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::{Value, json};

use super::super::{BridgeContext, arg, arg_str, to_value};
use crate::{feedback, ffmpeg, identity};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    _caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // ===== 设置页（对照 register-system-ipc.ts / register-feedback-ipc.ts） =====
        "pathJoin" => {
            let parts: Vec<String> = args.iter().map(arg_str).collect::<Vec<_>>();
            let joined = parts
                .iter()
                .fold(std::path::PathBuf::new(), |acc, part| acc.join(part));
            Ok(Value::String(joined.to_string_lossy().to_string()))
        }
        "getAppVersion" => Ok(Value::String(env!("CARGO_PKG_VERSION").to_string())),
        "feedbackMeta" => to_value(&feedback::FeedbackMetaResponse {
            meta: feedback::collect_meta(&ctx.pocket.auth_state()),
            endpoint_ready: feedback::is_endpoint_ready(),
        }),
        "submitFeedback" => {
            let input: feedback::FeedbackInput = arg(args, 0)?;
            let meta = feedback::collect_meta(&ctx.pocket.auth_state());
            feedback::submit_feedback(&ctx.bilibili_http, &input, meta).await?;
            Ok(Value::Null)
        }
        "createLiveStream" => {
            let rtmp_url: String = arg(args, 0)?;
            let live_id: String = arg(args, 1)?;
            let directory = ctx.db.with(ffmpeg::ffmpeg_dir);
            let binary = ffmpeg::resolve(&directory)
                .map_err(|reason| format!("{reason}，请在「设置」中下载或重新选择 FFmpeg 目录"))?;
            to_value(&ctx.media.create_session(&live_id, &rtmp_url, binary)?)
        }
        "preventSleep" => {
            let id = ctx.sleep.prevent()?;
            Ok(Value::Number(id.into()))
        }
        "allowSleep" => {
            let id = arg::<u64>(args, 0)?;
            ctx.sleep.allow(id as u32);
            Ok(Value::Null)
        }
        "stopLiveStream" => {
            let live_id: String = arg(args, 0)?;
            ctx.media.stop_session(&live_id);
            Ok(Value::Null)
        }

        // 凭证只在 Rust 侧；渲染层拿到的永远是脱敏后的状态（不含 token）。
        "pocketSendSms" => {
            let mobile: String = arg(args, 0)?;
            let area: Option<String> = arg(args, 1)?;
            let answer: Option<String> = arg(args, 2)?;
            let profile: Option<String> = arg(args, 3)?;
            to_value(&ctx.pocket.send_sms(&mobile, area, answer, profile).await?)
        }
        "pocketLoginByCode" => {
            let mobile: String = arg(args, 0)?;
            let code: String = arg(args, 1)?;
            let area: Option<String> = arg(args, 2)?;
            let profile: Option<String> = arg(args, 3)?;
            let state = ctx
                .pocket
                .login_by_code(&mobile, &code, area, profile)
                .await?;
            ctx.refresh_unread_after_login();
            to_value(&state)
        }
        "pocketLoginByToken" => {
            let token: String = arg(args, 0)?;
            let mobile: Option<String> = arg(args, 1)?;
            let profile: Option<String> = arg(args, 2)?;
            let state = ctx.pocket.login_by_token(&token, mobile, profile).await?;
            ctx.refresh_unread_after_login();
            to_value(&state)
        }
        "pocketGetAuthState" => to_value(&ctx.pocket.auth_state()),
        "pocketLogout" => {
            ctx.pocket.logout()?;
            // 退出后未读属于上一个账号，不能留在角标里。
            // 轮询里的 `tick_inner` 也会兜底清，但要等下一轮（60s），用户会先看到残留。
            ctx.unread.clear_account_unread();
            ctx.publish_unread();
            Ok(Value::Null)
        }
        "pocketVerifyLogin" => to_value(&ctx.pocket.verify_login().await?),
        "pocketCheckIn" => to_value(&ctx.pocket.check_in().await?),
        "pocketGetFollowedIds" => Ok(json!(ctx.pocket.followed_ids().await?)),
        "pocketFollowMember" => {
            let user_id: i64 = arg(args, 0)?;
            ctx.pocket.follow_member(user_id).await?;
            Ok(Value::Null)
        }
        "pocketUnfollowMember" => {
            let user_id: i64 = arg(args, 0)?;
            ctx.pocket.unfollow_member(user_id).await?;
            Ok(Value::Null)
        }

        // 自检信息：确认 UI 确实经由 Rust。
        "getAppInfo" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "device": identity::device_label(),
            "deviceId": ctx.device_id,
        })),

        other => Err(format!("未实现的通道: {other}")),
    }
}
