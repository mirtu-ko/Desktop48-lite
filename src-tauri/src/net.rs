//! 通用 HTTP 出口（对照 Electron 版 `src/main/net/*` 与 `netRequest` 通道）。
//!
//! 这一层是正式迁移的地基：渲染层原有的数据层（`services/api-urls.ts` 等）全部通过
//! `netRequest` 打 pocketapi，只要这条通道行为一致，直播 / 成员 / 公演等页面就无需改写。
//!
//! 与 Electron 版对齐的四条规则：
//! 1. 白名单覆盖入口与**每一次重定向**；
//! 2. 出口限流：按域名令牌桶 + 在途上限，429 按 Retry-After 整域冷却；
//! 3. 业务接口（pocketapi / h5）补 App 身份头，静态 / 媒体 CDN 保持调用方请求头；
//! 4. 非 2xx 不抛错，状态码 / 响应头原样回渲染层。

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::identity;

/// 单域名每秒补令牌数与可攒 burst（与 Electron 版一致）。
const HOST_RATE_PER_SEC: f64 = 5.0;
const HOST_BURST: f64 = 5.0;
/// 单域名在途上限；全局在途上限。
const HOST_MAX_INFLIGHT: usize = 5;
const GLOBAL_MAX_INFLIGHT: usize = 8;
/// 排队的总时长上限（等待令牌 + 等待在途名额）。
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
/// 单次请求（发出 → 读完响应体）的超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 响应体上限，超过即中止。
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// `imageDataUrl` 单张图片上限（与 Electron 版一致）。
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Retry-After 再长也不锁死整域超过这个时长。
const MAX_PENALTY: Duration = Duration::from_secs(60);

const ALLOWED_HOSTS: &[&str] = &[
    "pocketapi.48.cn",
    "www.cgt48.com",
    "b50.ckg48.com",
    "live.48.cn",
];
const ALLOWED_SUFFIXES: &[&str] = &[".48.cn", ".snh48.com", ".yunxinsvr.com"];
/// 做客户端身份判定的业务域名；媒体 / 静态 CDN 不发身份头。
const APP_IDENTITY_HOSTS: &[&str] = &["pocketapi.48.cn", "h5.48.cn"];

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
}

fn is_allowed_host(host: &str) -> bool {
    ALLOWED_HOSTS.contains(&host) || ALLOWED_SUFFIXES.iter().any(|suffix| host.ends_with(suffix))
}

fn is_http_scheme(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// `netRequest` 放行规则：http(s) + 白名单（解析失败即拒绝）。
pub fn is_allowed_http_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        is_http_scheme(&parsed)
            && parsed
                .host_str()
                .is_some_and(|host| is_allowed_host(&host.to_ascii_lowercase()))
    })
}

/// 业务接口（直连 PocketApi）更严：只接受 https。
pub fn is_allowed_https_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        parsed.scheme() == "https"
            && parsed
                .host_str()
                .is_some_and(|host| is_allowed_host(&host.to_ascii_lowercase()))
    })
}

/// 直播拉流白名单（对照 Electron `isAllowedStreamUrl`）：
/// `rtmp://` 只认 `.48.cn` / `.snh48.com` 后缀，http(s) 源与通用 HTTP 白名单同口径。
/// 渲染层传入的地址会直接交给 FFmpeg，不校验就等于把主进程变成任意地址的代理。
pub fn is_allowed_stream_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        match parsed.scheme() {
            "rtmp" => [".48.cn", ".snh48.com"]
                .iter()
                .any(|suffix| host.ends_with(suffix)),
            "http" | "https" => is_http_scheme(&parsed) && is_allowed_host(&host),
            _ => false,
        }
    })
}

fn needs_app_identity(url: &str) -> bool {
    host_of(url).is_some_and(|host| APP_IDENTITY_HOSTS.contains(&host.as_str()))
}

/// 请求头收口：身份头覆盖调用方同名头（大小写不敏感），`Content-Type` 归调用方。
pub fn merge_app_identity(
    headers: Option<&HashMap<String, String>>,
    device_id: &str,
) -> HashMap<String, String> {
    let identity_headers = identity::identity_headers(device_id);
    let override_keys: Vec<String> = identity_headers
        .iter()
        .filter(|(key, _)| *key != "content-type")
        .map(|(key, _)| key.to_string())
        .collect();

    let mut merged: HashMap<String, String> = HashMap::new();
    if let Some(caller) = headers {
        for (key, value) in caller {
            if !override_keys
                .iter()
                .any(|override_key| override_key.eq_ignore_ascii_case(key))
            {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    for (key, value) in identity_headers {
        // Content-Type 归调用方：身份头里的默认值只给直连业务客户端用，
        // 通用代理不能覆盖渲染层为 GET / jsonp 准备的请求头（与 Electron 同口径）。
        if key == "content-type" {
            continue;
        }
        merged.insert(key.to_string(), value);
    }
    merged
}

/// 供业务客户端（`api.rs`）复用的 reqwest 构造：重定向逐跳过白名单。
pub fn new_client() -> Result<reqwest::Client, String> {
    new_client_with_timeout(REQUEST_TIMEOUT)
}

/// 指定整次请求超时的客户端（网络诊断用更短的 8s）。
pub fn new_client_with_timeout(timeout: Duration) -> Result<reqwest::Client, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(8))
        .gzip(true)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url().to_string();
            if is_allowed_http_url(&url) {
                attempt.follow()
            } else {
                attempt.error(io::Error::other(format!("重定向目标不在白名单中: {url}")))
            }
        }))
        .build()
        .map_err(|err| format!("HTTP 客户端初始化失败: {err}"))?;
    Ok(client)
}

// ===== 出口限流 =====

struct TokenState {
    tokens: f64,
    refilled_at: Instant,
    blocked_until: Option<Instant>,
}

struct HostGate {
    inflight: Arc<Semaphore>,
    tokens: Mutex<TokenState>,
}

/// 一次通过闸门的凭证：drop 即归还全局 / 域名在途名额。
pub struct NetPermit {
    _global: OwnedSemaphorePermit,
    _host: OwnedSemaphorePermit,
}

/// 所有对外请求共用的出口闸门。
pub struct NetGate {
    global: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<HostGate>>>,
}

impl Default for NetGate {
    fn default() -> Self {
        Self::new()
    }
}

impl NetGate {
    pub fn new() -> Self {
        Self {
            global: Arc::new(Semaphore::new(GLOBAL_MAX_INFLIGHT)),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    fn host_gate(&self, host: &str) -> Arc<HostGate> {
        let mut hosts = self.hosts.lock().unwrap();
        hosts
            .entry(host.to_string())
            .or_insert_with(|| {
                Arc::new(HostGate {
                    inflight: Arc::new(Semaphore::new(HOST_MAX_INFLIGHT)),
                    tokens: Mutex::new(TokenState {
                        tokens: HOST_BURST,
                        refilled_at: Instant::now(),
                        blocked_until: None,
                    }),
                })
            })
            .clone()
    }

    /// 取一个名额；超时 / 排队过长一律拒绝，避免把过期请求越排越旧。
    pub async fn acquire(&self, host: &str) -> Result<NetPermit, String> {
        let gate = self.host_gate(host);
        let global = self.global.clone();

        let acquired = tokio::time::timeout(QUEUE_TIMEOUT, async move {
            loop {
                let wait = {
                    let mut state = gate.tokens.lock().unwrap();
                    refill_tokens(&mut state);
                    let now = Instant::now();
                    let blocked = state.blocked_until.is_some_and(|until| until > now);
                    if !blocked && state.tokens >= 1.0 {
                        state.tokens -= 1.0;
                        None
                    } else if blocked {
                        Some(
                            state
                                .blocked_until
                                .map(|until| until.saturating_duration_since(now))
                                .unwrap_or_default(),
                        )
                    } else {
                        Some(Duration::from_millis(
                            (((1.0 - state.tokens).max(0.0) * 1000.0) / HOST_RATE_PER_SEC) as u64,
                        ))
                    }
                };

                match wait {
                    Some(wait) => tokio::time::sleep(wait.max(Duration::from_millis(20))).await,
                    None => break,
                }
            }

            let host_permit = gate
                .inflight
                .clone()
                .acquire_owned()
                .await
                .map_err(|err| format!("域名名额已关闭: {err}"))?;
            let global_permit = global
                .acquire_owned()
                .await
                .map_err(|err| format!("全局名额已关闭: {err}"))?;
            Ok::<NetPermit, String>(NetPermit {
                _global: global_permit,
                _host: host_permit,
            })
        })
        .await;

        match acquired {
            Ok(result) => result,
            Err(_) => Err(format!(
                "本地请求排队超时（{}ms），请稍后再试",
                QUEUE_TIMEOUT.as_millis()
            )),
        }
    }

    /// 429 后的整域冷却：只认服务端明确给的 Retry-After。
    pub fn penalize(&self, host: &str, penalty: Duration) {
        if penalty.is_zero() {
            return;
        }
        let gate = self.host_gate(host);
        let mut state = gate.tokens.lock().unwrap();
        let until = Instant::now() + penalty.min(MAX_PENALTY);
        state.blocked_until = Some(match state.blocked_until {
            Some(existing) => existing.max(until),
            None => until,
        });
    }
}

fn refill_tokens(state: &mut TokenState) {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(state.refilled_at);
    if elapsed.is_zero() {
        return;
    }
    state.tokens = (state.tokens + elapsed.as_secs_f64() * HOST_RATE_PER_SEC).min(HOST_BURST);
    state.refilled_at = now;
}

/// Retry-After 支持秒数与 HTTP 日期；无法解析时返回 0（不猜时长）。
pub fn parse_retry_after_ms(value: Option<&str>) -> u64 {
    let Some(raw) = value else {
        return 0;
    };
    let trimmed = raw.trim();
    if let Ok(seconds) = trimmed.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return ((seconds * 1000.0) as u64).min(MAX_PENALTY.as_millis() as u64);
        }
        return 0;
    }
    let Ok(at) = httpdate::parse_http_date(trimmed) else {
        return 0;
    };
    let Ok(now) = SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    let Ok(at) = at.duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    at.saturating_sub(now)
        .as_millis()
        .min(MAX_PENALTY.as_millis()) as u64
}

// ===== 请求 =====

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetRequestOptions {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NetResponse {
    pub status: u16,
    pub body: String,
    pub headers: HashMap<String, String>,
}

/// 执行一次 `netRequest`：白名单 → 补身份头 → 排队 → 发请求 → 读体。
pub async fn request(
    client: &reqwest::Client,
    gate: &NetGate,
    device_id: &str,
    options: &NetRequestOptions,
) -> Result<NetResponse, String> {
    if !is_allowed_http_url(&options.url) {
        return Err(format!("请求被拒绝：域名不在白名单中 ({})", options.url));
    }

    let method = options
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("不支持的请求方法: {method}"))?;

    let headers = if needs_app_identity(&options.url) {
        merge_app_identity(options.headers.as_ref(), device_id)
    } else {
        options.headers.clone().unwrap_or_default()
    };

    let host = host_of(&options.url).ok_or_else(|| "URL 解析失败".to_string())?;
    let _permit = gate.acquire(&host).await?;

    let attempt = async {
        let mut request = client.request(method, &options.url);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = options.body.as_deref().filter(|body| !body.is_empty()) {
            request = request.body(body.to_string());
        }

        let response = request
            .send()
            .await
            .map_err(|err| format!("请求失败: {err}"))?;
        let status = response.status().as_u16();

        let mut response_headers = HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                response_headers
                    .entry(name.as_str().to_ascii_lowercase())
                    .or_insert_with(|| value.to_string());
            }
        }

        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| format!("读取响应失败: {err}"))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(format!(
                    "响应体超过上限（{MAX_RESPONSE_BYTES} 字节）: {}",
                    options.url
                ));
            }
        }
        let body = String::from_utf8_lossy(&body).to_string();
        Ok::<NetResponse, String>(NetResponse {
            status,
            body,
            headers: response_headers,
        })
    };

    let response = tokio::time::timeout(REQUEST_TIMEOUT, attempt)
        .await
        .map_err(|_| {
            format!(
                "请求超时（{}ms）: {}",
                REQUEST_TIMEOUT.as_millis(),
                options.url
            )
        })??;

    if response.status == 429 {
        gate.penalize(
            &host,
            Duration::from_millis(parse_retry_after_ms(
                response.headers.get("retry-after").map(String::as_str),
            )),
        );
    }

    Ok(response)
}

/// `imageDataUrl`：取图片二进制并转 data URL。
///
/// 这条通道服务于生日贺卡等需要把跨域图片画进 canvas 的场景（直接画会污染画布），
/// 因此与 `netRequest` 共用出口闸门和白名单，避免成为绕过限流的第二条路。
pub async fn fetch_image_data_url(
    client: &reqwest::Client,
    gate: &NetGate,
    url: &str,
) -> Result<String, String> {
    use base64::Engine as _;

    if !is_allowed_http_url(url) {
        return Err(format!("请求被拒绝：域名不在白名单中 ({url})"));
    }
    let host = host_of(url).ok_or_else(|| "URL 解析失败".to_string())?;
    let _permit = gate.acquire(&host).await?;

    let attempt = async {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|err| format!("图片请求失败: {err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("图片请求失败（{}）: {url}", status.as_u16()));
        }

        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !mime.starts_with("image/") {
            return Err(format!("响应不是图片: {url}"));
        }

        let mut data = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| format!("读取图片失败: {err}"))?
        {
            data.extend_from_slice(&chunk);
            if data.len() > MAX_IMAGE_BYTES {
                return Err(format!("图片超过上限（{MAX_IMAGE_BYTES} 字节）: {url}"));
            }
        }
        Ok::<String, String>(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(data)
        ))
    };

    tokio::time::timeout(REQUEST_TIMEOUT, attempt)
        .await
        .map_err(|_| format!("请求超时（{}ms）: {url}", REQUEST_TIMEOUT.as_millis()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_accepts_exact_and_suffix_hosts() {
        assert!(is_allowed_http_url(
            "https://pocketapi.48.cn/live/api/v1/live/getLiveList"
        ));
        assert!(is_allowed_http_url("https://idol-vod.48.cn/a.m3u8"));
        assert!(is_allowed_http_url("http://www.cgt48.com/music.json"));
        assert!(is_allowed_http_url("https://x.snh48.com/a.png"));
    }

    #[test]
    fn whitelist_rejects_other_hosts_and_schemes() {
        for url in [
            "https://example.com/",
            "https://48.cn.evil.com/",
            "ftp://pocketapi.48.cn/a",
            "not a url",
        ] {
            assert!(!is_allowed_http_url(url), "{url} 不应放行");
        }
        // 业务直连只认 https
        assert!(!is_allowed_https_url("http://pocketapi.48.cn/a"));
    }

    #[test]
    fn app_identity_only_for_business_hosts() {
        assert!(needs_app_identity("https://pocketapi.48.cn/a"));
        assert!(needs_app_identity("https://h5.48.cn/a"));
        assert!(!needs_app_identity("https://source.48.cn/a.jpg"));
        assert!(!needs_app_identity("https://idol-vod.48.cn/a.m3u8"));
    }

    #[test]
    fn identity_merge_overrides_case_insensitively_but_keeps_content_type() {
        let mut caller = HashMap::new();
        caller.insert("User-Agent".to_string(), "browser".to_string());
        caller.insert("content-type".to_string(), "application/json".to_string());
        caller.insert("X-Trace".to_string(), "1".to_string());

        let merged = merge_app_identity(Some(&caller), "0123456789abcdef");

        assert!(merged["user-agent"].starts_with("PocketFans201807/"));
        assert_eq!(merged["content-type"], "application/json");
        // 调用方自带的非身份头保持原键名大小写（与 Electron 的合并结果一致）
        assert_eq!(merged["X-Trace"], "1");
        assert!(!merged.contains_key("User-Agent"));
        assert!(merged.contains_key("appinfo"));
        assert!(merged.contains_key("pa"));
    }

    #[test]
    fn retry_after_parses_seconds_and_http_dates() {
        assert_eq!(parse_retry_after_ms(Some("3")), 3000);
        assert_eq!(parse_retry_after_ms(Some("999999")), 60_000);
        assert_eq!(parse_retry_after_ms(Some("garbage")), 0);
        assert_eq!(parse_retry_after_ms(None), 0);
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(5));
        let parsed = parse_retry_after_ms(Some(&date));
        assert!(parsed <= 60_000);
    }

    #[tokio::test]
    async fn gate_grants_and_releases_slots() {
        let gate = NetGate::new();
        let permit = gate.acquire("pocketapi.48.cn").await.unwrap();
        drop(permit);
        // 归还后仍可再次拿到（令牌桶初始 burst > 1）
        let permit = gate.acquire("pocketapi.48.cn").await.unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn penalty_blocks_new_requests() {
        let gate = NetGate::new();
        gate.penalize("pocketapi.48.cn", Duration::from_millis(150));
        let started = Instant::now();
        let _permit = gate.acquire("pocketapi.48.cn").await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(140));
    }

    #[test]
    fn net_response_shape_matches_contract() {
        let response = NetResponse {
            status: 200,
            body: "{}".to_string(),
            headers: HashMap::from([("content-type".to_string(), "application/json".to_string())]),
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["status"], 200);
        assert_eq!(value["headers"]["content-type"], "application/json");
    }
}
