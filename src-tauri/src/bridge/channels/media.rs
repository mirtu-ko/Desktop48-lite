//! FFmpeg 环境：状态判定、在线下载、目录选择。
//!
//! 阻塞式原生对话框必须丢到阻塞线程，不能卡住异步运行时。
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
        "getFfmpegStatus" => {
            let dir: Option<String> = arg(args, 0)?;
            to_value(&ctx.db.with(|db| ffmpeg::status(db, dir.as_deref())))
        }
        "downloadFfmpeg" => Ok(Value::String(
            ctx.downloader.download(ctx.app.get().cloned()).await?,
        )),
        // 目录选择放在 Rust 侧，渲染层不直接依赖 Tauri 插件；
        // 阻塞式原生对话框必须丢到阻塞线程，不能卡住异步运行时。
        "selectDirectory" => {
            let app = ctx.app.get().cloned().ok_or("目录选择仅在桌面应用内可用")?;
            let picked = tokio::task::spawn_blocking(move || {
                use tauri_plugin_dialog::DialogExt;
                app.dialog()
                    .file()
                    .set_title("选择目录")
                    .blocking_pick_folder()
            })
            .await
            .map_err(|err| format!("目录选择失败: {err}"))?;
            match picked {
                Some(path) => {
                    let path = path
                        .into_path()
                        .map_err(|err| format!("目录路径无效: {err}"))?;
                    Ok(Value::String(path.to_string_lossy().to_string()))
                }
                None => Ok(Value::Null),
            }
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
