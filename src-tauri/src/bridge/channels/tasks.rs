//! 下载 / 录制任务：两组共用一套机制，差别只在 ffmpeg 输出参数。
//!
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::Value;

use super::super::{BridgeContext, arg, to_value};
use crate::ffmpeg;

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    _caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // ===== 下载 / 录制任务 =====
        // 两组共用同一套机制，差别只在 ffmpeg 输出参数（见 tasks.rs 的 TaskKind）
        "downloadTaskStart" | "recordTaskStart" => {
            let kind = if channel == "recordTaskStart" {
                crate::tasks::TaskKind::Record
            } else {
                crate::tasks::TaskKind::Download
            };
            let url: String = arg(args, 0)?;
            let filename: String = arg(args, 1)?;
            let live_id: String = arg(args, 2)?;
            let save_dir = ctx
                .db
                .with(|db| db.config("downloadDirectory"))?
                .as_str()
                .unwrap_or_default()
                .to_string();
            let ffmpeg_dir = ctx.db.with(ffmpeg::ffmpeg_dir);
            let binary = ffmpeg::resolve(&ffmpeg_dir)
                .map_err(|reason| format!("{reason}，请在「设置」中下载或重新选择 FFmpeg 目录"))?;
            ctx.tasks
                .start(kind, &url, &filename, &live_id, &save_dir, &binary)
                .await
                .map(Value::String)
        }
        "downloadTaskStop" | "recordTaskStop" => {
            let live_id: String = arg(args, 0)?;
            ctx.tasks.stop(&live_id).await;
            Ok(Value::Null)
        }
        "downloadTaskList" => to_value(&ctx.tasks.list(crate::tasks::TaskKind::Download)),
        "recordTaskList" => to_value(&ctx.tasks.list(crate::tasks::TaskKind::Record)),
        "downloadTaskRemove" | "recordTaskRemove" => {
            let live_id: String = arg(args, 0)?;
            ctx.tasks.remove(&live_id);
            Ok(Value::Null)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
