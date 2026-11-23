//! 通用 HTTP 代理：`netRequest` / `imageDataUrl` / `netDiagnose`。
//!
//! 渲染层的数据层（`services/*`、`api-urls.ts`）全部走这里：白名单逐跳、令牌桶限流、
//! 429 冷却与客户端身份头都在 `net` 模块完成，桥只负责转发。
//! 参数 / 返回值口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::Value;

use super::super::{BridgeContext, arg, to_value};
use crate::{diagnose, net};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    _caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // 通用 HTTP 代理。渲染层现有的数据层（services/*、api-urls.ts）走这条，
        // 行为对齐 Electron 的 netRequest：白名单 / 限流 / App 身份 / 非 2xx 原样返回。
        "netRequest" => {
            let options: net::NetRequestOptions = arg(args, 0)?;
            let response = net::request(&ctx.http, &ctx.gate, &ctx.device_id, &options).await?;
            to_value(&response)
        }

        // 图片转 data URL：生日贺卡等需要把跨域图片画进 canvas 的场景。
        "imageDataUrl" => {
            let url: String = arg(args, 0)?;
            Ok(Value::String(
                net::fetch_image_data_url(&ctx.http, &ctx.gate, &url).await?,
            ))
        }

        // 网络诊断：并发探测业务端点，结果只含展示字段（身份头不外传）。
        "netDiagnose" => {
            to_value(&diagnose::diagnose_targets(ctx.diagnose_http.clone(), &ctx.device_id).await?)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
