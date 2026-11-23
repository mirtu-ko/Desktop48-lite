//! 系统集成：打开路径 / 外链、图片剪贴板与保存。
//!
//! 路径先过白名单，外链只放行 http/https。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::Value;

use super::super::{BridgeContext, arg, open_external_url};
use crate::image_file;

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    _caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // ===== 系统集成（对照 register-system-ipc.ts / register-danmaku-ipc.ts） =====
        "openPath" => {
            let file_path: String = arg(args, 0)?;
            ctx.open_path(&file_path)?;
            Ok(Value::Null)
        }
        "openExternal" => {
            let url: String = arg(args, 0)?;
            open_external_url(&url)?;
            Ok(Value::Null)
        }
        "copyImageToClipboard" => {
            let data_url: String = arg(args, 0)?;
            let bytes = image_file::decode_png_data_url(&data_url)?;
            ctx.copy_png_to_clipboard(bytes).await?;
            Ok(Value::Null)
        }
        "saveImageFile" => {
            let data_url: String = arg(args, 0)?;
            let suggested_name: String = arg(args, 1)?;
            let bytes = image_file::decode_png_data_url(&data_url)?;
            let safe_name = image_file::safe_png_file_name(&suggested_name);
            match ctx.save_png(bytes, safe_name).await? {
                Some(path) => Ok(Value::String(path)),
                None => Ok(Value::Null),
            }
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
