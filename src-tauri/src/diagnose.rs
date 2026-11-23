//! 网络诊断（对照 Electron 版 `src/main/net/net-diagnose.ts` + `net-diagnose-chain.ts`）。
//!
//! 探测应用实际依赖的端点，区分「网络不可达 / 服务端拒绝 / 服务端异常」：
//! - 口袋 API 同一地址分别用站点身份、App 身份探测，用 403 差异定位身份限制；
//! - 成员名单按「快照 → jsonp」主备链探测，任一来源接通即算该行正常；
//! - 头像/封面、房间音视频、官网、曲库各一条。
//!
//! 身份头只用于发请求，不会随结果回渲染层（含 token 的版本不做）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::{api::LIVE_LIST_URL, identity, net};

const MEMBER_SNAPSHOT_URL: &str = "https://h5.48.cn/resource/json/allmembers_0.jsonp";
const MEMBER_JSONP_URL: &str = "https://h5.48.cn/resource/jsonp/allmembers.php?gid=00";
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const DETAIL_BYTES: usize = 512;
const DETAIL_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseItem {
    pub label: String,
    pub url: String,
    /// HTTP 状态码；None 表示未收到响应。
    pub status: Option<u16>,
    /// 从发起到收到响应头或失败的耗时（ms）。
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

struct Target {
    label: String,
    url: String,
    method: &'static str,
    body: Option<String>,
    headers: HashMap<String, String>,
    fallbacks: Vec<Target>,
}

fn live_list_target(label: &str, headers: HashMap<String, String>) -> Target {
    Target {
        label: label.to_string(),
        url: LIVE_LIST_URL.to_string(),
        method: "POST",
        body: Some(
            json!({ "groupId": 0, "next": "0", "debug": false, "record": false }).to_string(),
        ),
        headers,
        fallbacks: Vec::new(),
    }
}

fn site_headers() -> HashMap<String, String> {
    HashMap::from([
        ("user-agent".to_string(), identity::site_user_agent()),
        ("content-type".to_string(), "application/json".to_string()),
    ])
}

fn app_headers(device_id: &str) -> HashMap<String, String> {
    // 站点 UA + Content-Type 作为调用方请求头，身份头覆盖 UA / appInfo / pa。
    net::merge_app_identity(Some(&site_headers()), device_id)
}

fn build_targets(device_id: &str) -> Vec<Target> {
    let mut targets = vec![live_list_target(
        "口袋48 API（站点身份对照）",
        site_headers(),
    )];
    targets.push(live_list_target(
        "口袋48 API（App 身份）",
        app_headers(device_id),
    ));

    targets.push(Target {
        label: "成员名单（快照 / jsonp）".to_string(),
        url: MEMBER_SNAPSHOT_URL.to_string(),
        method: "GET",
        body: None,
        headers: app_headers(device_id),
        fallbacks: vec![Target {
            label: "成员名单 jsonp".to_string(),
            url: MEMBER_JSONP_URL.to_string(),
            method: "GET",
            body: None,
            headers: app_headers(device_id),
            fallbacks: Vec::new(),
        }],
    });

    for (label, url) in [
        ("头像/封面媒体", "https://source.48.cn/"),
        ("房间音视频", "https://mp4.48.cn/"),
        ("官网 / 媒体源", "https://www.snh48.com/"),
        ("CGT48 曲库", "https://www.cgt48.com/json/music_snh.json"),
    ] {
        targets.push(Target {
            label: label.to_string(),
            url: url.to_string(),
            method: "GET",
            body: None,
            headers: site_headers(),
            fallbacks: Vec::new(),
        });
    }
    targets
}

/// 去掉 HTML 标签并压缩空白，避免把整页拦截内容塞进诊断结果。
fn clip_detail(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(DETAIL_CHARS)
        .collect()
}

fn describe(item: &DiagnoseItem) -> String {
    match item.status {
        Some(status) => format!("HTTP {status}"),
        None => item.error.clone().unwrap_or_else(|| "无响应".to_string()),
    }
}

/// 主备链结论合成：任一来源 < 400 即该行正常；全部失败才判不可用（对照 `resolveSources`）。
fn resolve_sources(label: &str, results: Vec<DiagnoseItem>) -> DiagnoseItem {
    if results.is_empty() {
        return DiagnoseItem {
            label: label.to_string(),
            url: String::new(),
            status: None,
            ms: 0,
            error: Some("没有可用的探测来源".to_string()),
            detail: None,
        };
    }

    if let Some(hit_index) = results
        .iter()
        .position(|item| item.status.is_some_and(|status| status < 400))
    {
        let mut hit = results[hit_index].clone();
        hit.label = label.to_string();
        if results.len() == 1 {
            return hit;
        }
        hit.detail = Some(format!(
            "主源不可用（{}），已由备源接通",
            describe(&results[0])
        ));
        return hit;
    }

    let summary = results
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let prefix = if index == 0 {
                "主源".to_string()
            } else {
                format!("备源{index}")
            };
            format!("{prefix} {}", describe(item))
        })
        .collect::<Vec<_>>()
        .join("；");

    let mut item = results[0].clone();
    item.label = label.to_string();
    if results.len() > 1 {
        item.detail = Some(format!("主备全部不可用：{summary}"));
    }
    item
}

async fn probe(client: &reqwest::Client, target: &Target) -> DiagnoseItem {
    let started = Instant::now();
    let mut request = client.request(
        target
            .method
            .parse::<reqwest::Method>()
            .unwrap_or(reqwest::Method::GET),
        &target.url,
    );
    for (name, value) in &target.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(body) = &target.body {
        request = request.body(body.clone());
    }

    let send = tokio::time::timeout(PROBE_TIMEOUT, request.send()).await;
    let mut item = DiagnoseItem {
        label: target.label.clone(),
        url: target.url.clone(),
        status: None,
        ms: 0,
        error: None,
        detail: None,
    };

    let mut response = match send {
        Err(_) => {
            item.ms = started.elapsed().as_millis() as u64;
            item.error = Some(format!("超时（{} 秒无响应）", PROBE_TIMEOUT.as_secs()));
            return item;
        }
        Ok(Err(err)) => {
            item.ms = started.elapsed().as_millis() as u64;
            item.error = Some(err.to_string());
            return item;
        }
        Ok(Ok(response)) => response,
    };

    let status = response.status().as_u16();
    item.status = Some(status);
    if status < 400 {
        item.ms = started.elapsed().as_millis() as u64;
        return item;
    }

    let mut bytes = Vec::new();
    let read = async {
        while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
            let remaining = DETAIL_BYTES.saturating_sub(bytes.len());
            if remaining > 0 {
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            if bytes.len() >= DETAIL_BYTES {
                break;
            }
        }
        Ok::<(), String>(())
    };
    match tokio::time::timeout(PROBE_TIMEOUT, read).await {
        Ok(Ok(())) => {
            let detail = clip_detail(&String::from_utf8_lossy(&bytes));
            if !detail.is_empty() {
                item.detail = Some(detail);
            }
        }
        Ok(Err(err)) => item.detail = Some(err),
        Err(_) => item.detail = Some("（响应体未在超时内结束）".to_string()),
    }
    item.ms = started.elapsed().as_millis() as u64;
    item
}

async fn probe_chain(client: &reqwest::Client, target: Target) -> DiagnoseItem {
    let mut results = Vec::new();
    let mut sources = vec![target];
    // 主源失败才续探备源；任一来源 < 400 即结束。
    while let Some(source) = sources.pop() {
        let result = probe(client, &source).await;
        let ok = result.status.is_some_and(|status| status < 400);
        results.push(result);
        if ok {
            break;
        }
        // 备源按声明顺序探测：栈式 DFS 需反序压入。
        sources.extend(source.fallbacks.into_iter().rev());
    }
    let label = results
        .first()
        .map(|item| item.label.clone())
        .unwrap_or_default();
    resolve_sources(&label, results)
}

/// 并发探测全部目标；单个目标失败不影响其余结果，返回顺序与目标表一致。
pub async fn diagnose_targets(
    client: reqwest::Client,
    device_id: &str,
) -> Result<Vec<DiagnoseItem>, String> {
    let targets = build_targets(device_id);
    let mut set = tokio::task::JoinSet::new();
    for (index, target) in targets.into_iter().enumerate() {
        let client = client.clone();
        set.spawn(async move { (index, probe_chain(&client, target).await) });
    }

    let mut items = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (index, item) = joined.map_err(|err| format!("诊断任务失败: {err}"))?;
        items.push((index, item));
    }
    items.sort_by_key(|(index, _)| *index);
    Ok(items.into_iter().map(|(_, item)| item).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(status: Option<u16>, error: Option<&str>) -> DiagnoseItem {
        DiagnoseItem {
            label: "src".to_string(),
            url: "https://example/".to_string(),
            status,
            ms: 1,
            error: error.map(str::to_string),
            detail: None,
        }
    }

    #[test]
    fn single_source_keeps_its_own_result() {
        let resolved = resolve_sources("主", vec![item(Some(200), None)]);
        assert_eq!(resolved.label, "主");
        assert_eq!(resolved.status, Some(200));
        assert!(resolved.detail.is_none());
    }

    #[test]
    fn fallback_success_is_reported_as_healthy() {
        let resolved = resolve_sources(
            "成员名单",
            vec![item(Some(403), None), item(Some(200), None)],
        );
        assert_eq!(resolved.label, "成员名单");
        assert_eq!(resolved.status, Some(200));
        assert_eq!(
            resolved.detail.as_deref(),
            Some("主源不可用（HTTP 403），已由备源接通")
        );
    }

    #[test]
    fn all_sources_down_lists_each_status() {
        let resolved = resolve_sources(
            "成员名单",
            vec![item(None, Some("超时")), item(Some(502), None)],
        );
        assert_eq!(resolved.status, None);
        assert_eq!(
            resolved.detail.as_deref(),
            Some("主备全部不可用：主源 超时；备源1 HTTP 502")
        );
    }

    #[test]
    fn detail_strips_tags_and_clips_length() {
        let raw = format!(
            "<html><body><h1>拦截</h1><script>alert(1)</script>{}</body></html>",
            "x".repeat(400)
        );
        let clipped = clip_detail(&raw);
        assert!(!clipped.contains('<'));
        assert!(clipped.chars().count() <= DETAIL_CHARS);
    }

    #[test]
    fn targets_cover_identity_and_member_chain() {
        let targets = build_targets("0123456789abcdef");
        let labels: Vec<&str> = targets.iter().map(|target| target.label.as_str()).collect();
        assert!(labels.contains(&"口袋48 API（站点身份对照）"));
        assert!(labels.contains(&"口袋48 API（App 身份）"));
        assert!(labels.contains(&"成员名单（快照 / jsonp）"));
        assert_eq!(targets.len(), 7);
        assert_eq!(targets[2].fallbacks.len(), 1);
    }
}
