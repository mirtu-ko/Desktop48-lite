//! 问题反馈上报（对照 Electron `src/main/feedback.ts`）。
//!
//! 上报地址由 Rust 持有，渲染层不传 URL——否则就得为反馈放宽 `netRequest` 的域名白名单。
//! 环境摘要（版本 / 平台 / 身份档）同样在这里采集，渲染层无法伪造。
//!
//! 上报内容只含用户填写的问题描述、可选联系方式与本模块采集的摘要；
//! 账号凭据、手机号、聊天与房间消息一律不采集，日志里也不打印反馈正文。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::pocket::PocketAuthState;

/// 上报服务地址（POST JSON）。
///
/// **留空**：本项目尚未发布，没有自己的反馈后端。绝不沿用原版的 `48.v2ing.com`——
/// 那是原作者的服务器，把本项目用户的反馈（含版本、平台、问题描述）发过去既不合适也不该发生。
/// 置空串即关闭提交入口：设置页据此禁用「反馈一下」，用户仍可复制环境信息手工反馈。
pub const FEEDBACK_ENDPOINT: &str = "";
/// 反馈正文长度上限（按字符计，超出拒绝而不是静默截断）
pub const FEEDBACK_CONTENT_MAX: usize = 2000;
/// 联系方式长度上限（邮箱 / 社交账号等，可选填）
pub const FEEDBACK_CONTACT_MAX: usize = 100;

const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(15);
const FEEDBACK_RESPONSE_MAX_BYTES: usize = 64 * 1024;

/// 运行环境摘要：由 Rust 采集，不含任何账号或内容信息。
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackMeta {
    pub app_version: String,
    pub platform: String,
    pub arch: String,
    /// 运行时版本：Electron 版这里是 Electron 版本，Tauri 版给 Tauri 版本。
    pub electron: String,
    pub locale: String,
    pub client_profile: String,
    pub logged_in: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackPayload<'a> {
    pub content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<&'a str>,
    pub meta: FeedbackMeta,
}

/// 设置页初始化所需：环境摘要 + 上报通道是否可用。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackMetaResponse {
    pub meta: FeedbackMeta,
    pub endpoint_ready: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackInput {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub contact: Option<String>,
}

/// 校验通过后的规范化输入（已 trim、空联系方式归一为 None）。
#[derive(Debug, PartialEq)]
pub struct NormalizedFeedback {
    pub content: String,
    pub contact: Option<String>,
}

/// 校验并规范化用户输入。超长选择拒绝而非截断：静默切掉尾部会让提交内容
/// 与用户看到的输入框不一致，事后无法解释。
pub fn validate_feedback_input(input: &FeedbackInput) -> Result<NormalizedFeedback, String> {
    let content = input.content.trim().to_string();
    if content.is_empty() {
        return Err("请填写问题描述".to_string());
    }
    if content.chars().count() > FEEDBACK_CONTENT_MAX {
        return Err(format!("问题描述不能超过 {FEEDBACK_CONTENT_MAX} 字"));
    }
    let contact = input
        .contact
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    if let Some(contact) = &contact
        && contact.chars().count() > FEEDBACK_CONTACT_MAX
    {
        return Err(format!("联系方式不能超过 {FEEDBACK_CONTACT_MAX} 字"));
    }
    Ok(NormalizedFeedback { content, contact })
}

pub fn is_endpoint_ready() -> bool {
    FEEDBACK_ENDPOINT.starts_with("http://") || FEEDBACK_ENDPOINT.starts_with("https://")
}

/// 采集环境摘要（只读，不含账号信息）。
pub fn collect_meta(auth: &PocketAuthState) -> FeedbackMeta {
    FeedbackMeta {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        electron: tauri::VERSION.to_string(),
        locale: locale(),
        client_profile: auth.client_profile.clone(),
        logged_in: auth.logged_in,
    }
}

/// 系统语言标记（`zh-CN` 形态）；取不到时回落 `en`。
fn locale() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                let base = value.split('.').next().unwrap_or(value);
                return base.replace('_', "-");
            }
        }
    }
    "en".to_string()
}

/// 提交反馈。失败一律返回 `Err`（渲染层展示 message），成功返回 `()`。
/// 端点契约：接收 `FeedbackPayload` 的 JSON，返回 2xx 即视为成功，响应体不解析。
pub async fn submit_feedback(
    http: &reqwest::Client,
    input: &FeedbackInput,
    meta: FeedbackMeta,
) -> Result<(), String> {
    if !is_endpoint_ready() {
        return Err("反馈上报地址未配置，暂无法提交".to_string());
    }
    let normalized = validate_feedback_input(input)?;
    let payload = FeedbackPayload {
        content: &normalized.content,
        contact: normalized.contact.as_deref(),
        meta,
    };
    let response = http
        .post(FEEDBACK_ENDPOINT)
        .timeout(FEEDBACK_TIMEOUT)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .body(serde_json::to_string(&payload).map_err(|err| format!("反馈内容序列化失败: {err}"))?)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                format!(
                    "提交超时（{}ms），请检查网络后重试",
                    FEEDBACK_TIMEOUT.as_millis()
                )
            } else {
                format!("提交失败：{err}")
            }
        })?;
    let status = response.status();
    if !(200..300).contains(&status.as_u16()) {
        return Err(format!(
            "提交失败（HTTP {status}），请稍后重试或通过其它渠道反馈"
        ));
    }
    // 必须把响应体读干净，否则连接可能不被释放；只计数不解析。
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("读取响应失败：{err}"))?;
    if body.len() > FEEDBACK_RESPONSE_MAX_BYTES {
        return Err("响应体过大，已放弃".to_string());
    }
    // 只记长度不记正文：反馈内容可能含用户个人信息
    crate::app_log::log_line(&format!(
        "[feedback] 已提交反馈（正文 {} 字，{}）",
        normalized.content.chars().count(),
        if normalized.contact.is_some() {
            "含联系方式"
        } else {
            "无联系方式"
        }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(content: &str, contact: Option<&str>) -> FeedbackInput {
        FeedbackInput {
            content: content.to_string(),
            contact: contact.map(str::to_string),
        }
    }

    #[test]
    fn normalizes_whitespace_and_empty_contact() {
        let normalized = validate_feedback_input(&input("  视频播不了 ", Some("   "))).unwrap();
        assert_eq!(normalized.content, "视频播不了");
        assert_eq!(normalized.contact, None);
    }

    #[test]
    fn rejects_empty_and_oversized_input() {
        assert!(validate_feedback_input(&input("   ", None)).is_err());
        let long = "字".repeat(FEEDBACK_CONTENT_MAX + 1);
        assert!(
            validate_feedback_input(&input(&long, None))
                .unwrap_err()
                .contains("不能超过")
        );
        let contact = "x".repeat(FEEDBACK_CONTACT_MAX + 1);
        assert!(
            validate_feedback_input(&input("ok", Some(&contact)))
                .unwrap_err()
                .contains("联系方式")
        );
        // 刚好到上限要放行
        assert!(validate_feedback_input(&input(&"字".repeat(FEEDBACK_CONTENT_MAX), None)).is_ok());
    }

    #[test]
    fn endpoint_is_unconfigured_and_meta_has_no_account_secrets() {
        // 本项目尚未发布：反馈端点留空，设置页据此禁用提交入口
        assert!(!is_endpoint_ready());
        let auth = PocketAuthState {
            logged_in: true,
            client_profile: "pocket48".to_string(),
            mobile_masked: "139****5678".to_string(),
            ..PocketAuthState::default()
        };
        let meta = collect_meta(&auth);
        assert_eq!(meta.client_profile, "pocket48");
        assert!(meta.logged_in);
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("139"), "环境摘要不得包含手机号");
        // 契约字段名与 `common/feedback.ts` 的 FeedbackMeta 一致
        assert!(json.contains("\"appVersion\""));
        assert!(json.contains("\"clientProfile\""));
    }
}
