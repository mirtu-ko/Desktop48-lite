//! 本地 HTTP-FLV 直播转流服务（对照 Electron `src/main/stream.ts` + `http-server.ts`）。
//!
//! 渲染层不直接接触 RTMP：`createLiveStream` 只登记 `liveId → rtmpUrl` 并返回本地地址，
//! 播放器 GET 本地地址时才 spawn 一个 FFmpeg，把 stdout 原样作为 `video/x-flv` 输出。
//! 这样既能让 WebView 播放直播，也把 FFmpeg 生命周期收在主进程里。
//!
//! 与 Electron 版对齐的行为：
//! - 只监听 `127.0.0.1`，端口由系统分配（`bind :0`），不再抢占固定 8080–8090；
//! - 播放路径带 per-session token：只有拿到 `createLiveStream` 返回值的一方才能触发 FFmpeg；
//! - Origin 白名单只放 Tauri WebView 与开发服务器；
//! - 每个播放请求一个 FFmpeg，客户端断开 / 10 秒无数据即结束响应并杀进程；
//! - `stopLiveStream` 立即撤销会话并中止所有在途转流。

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use crate::net;

pub const SERVER_HOST: &str = "127.0.0.1";
/// 首帧迟迟不来 / 上游 TCP 活着但不再吐数据时，主动结束响应，交给播放器重连。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// 单场直播最多并发几条转流：播放器重连时新旧连接可能短暂并存，
/// 留一点余量，但绝不能让人在本机循环 GET 把进程数打满（每个都是 FFmpeg）。
const MAX_STREAMS_PER_SESSION: usize = 3;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LiveStreamSession {
    pub url: String,
    pub live_id: String,
}

#[derive(Debug, Clone)]
struct Session {
    input_url: String,
    ffmpeg_path: PathBuf,
    /// 每次 `create_session` 重新生成；旧 token 立即失效（含在途连接的重连）。
    token: String,
}

type ActiveEntry = (u64, watch::Sender<bool>);
type ActiveMap = Arc<Mutex<HashMap<String, Vec<ActiveEntry>>>>;

struct Inner {
    sessions: Mutex<HashMap<String, Session>>,
    active: ActiveMap,
    active_seq: AtomicU64,
    port: AtomicU16,
    started: AtomicBool,
}

impl Inner {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            active: Arc::new(Mutex::new(HashMap::new())),
            active_seq: AtomicU64::new(0),
            port: AtomicU16::new(0),
            started: AtomicBool::new(false),
        }
    }

    fn register_active(&self, live_id: &str, sender: watch::Sender<bool>) -> u64 {
        let id = self.active_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(live_id.to_string())
            .or_default()
            .push((id, sender));
        id
    }

    fn unregister_active(&self, live_id: &str, id: u64) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entries) = active.get_mut(live_id) {
            entries.retain(|(entry_id, _)| *entry_id != id);
            if entries.is_empty() {
                active.remove(live_id);
            }
        }
    }

    fn cancel_active(&self, live_id: &str) {
        let senders = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(live_id)
            .cloned()
            .unwrap_or_default();
        for (_, sender) in senders {
            let _ = sender.send(true);
        }
    }

    /// 该会话是否还能再起一条转流（每条 = 一个 FFmpeg 进程）。
    fn has_capacity(&self, live_id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(live_id)
            .map(|entries| entries.len() < MAX_STREAMS_PER_SESSION)
            .unwrap_or(true)
    }
}

/// 一个进程一份。`start()` 在 App setup 调用；探针可显式启动，单测默认不监听端口。
#[derive(Clone)]
pub struct MediaServer {
    inner: Arc<Inner>,
}

impl Default for MediaServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaServer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }

    pub fn port(&self) -> u16 {
        self.inner.port.load(Ordering::SeqCst)
    }

    pub fn is_available(&self) -> bool {
        self.port() != 0
    }

    /// 绑定 `127.0.0.1:0`（系统分配端口）并启动 axum。重复调用直接返回当前端口。
    ///
    /// 以前固定 8080–8090 逐个尝试：端口被别的应用占满就整场直播不可用，
    /// 而且那个端口号会暴露给本机任何进程（它们可以直接往
    /// `127.0.0.1:8080/live/<liveId>.flv` 发请求，误伤和探测都无门槛）。
    /// 现在端口随机、路径带 token，两者都拿到才算授权。
    pub fn start(&self) -> Result<u16, String> {
        if self.is_available() {
            return Ok(self.port());
        }
        if self
            .inner
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("本地流媒体服务正在启动中".to_string());
        }

        let listener = match TcpListener::bind((SERVER_HOST, 0)) {
            Ok(listener) => listener,
            Err(err) => {
                self.inner.started.store(false, Ordering::SeqCst);
                return Err(format!("本地流媒体服务启动失败: {err}"));
            }
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => {
                self.inner.started.store(false, Ordering::SeqCst);
                return Err(format!("本地流媒体服务启动失败: {err}"));
            }
        };
        if let Err(err) = listener.set_nonblocking(true) {
            self.inner.started.store(false, Ordering::SeqCst);
            return Err(format!("本地流媒体服务启动失败: {err}"));
        }

        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(err) => {
                    eprintln!("[media] 接管监听端口失败: {err}");
                    return;
                }
            };
            let app = router(inner);
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("[media] 服务退出: {err}");
            }
        });

        self.inner.port.store(port, Ordering::SeqCst);
        eprintln!("[media] 本地流媒体服务器监听 {SERVER_HOST}:{port}");
        Ok(port)
    }

    /// 登记或复用直播会话；不 spawn FFmpeg，真正拉流由播放器 GET 时触发。
    ///
    /// 输入地址变化、或同一地址被重新登记时都会换发新 token：旧播放地址（含在途连接的重连）
    /// 立即失效，避免「已经切场了，旧页面还能把旧流拉起来」。
    pub fn create_session(
        &self,
        live_id: &str,
        input_url: &str,
        ffmpeg_path: PathBuf,
    ) -> Result<LiveStreamSession, String> {
        if !self.is_available() {
            return Err("本地流媒体服务尚未启动，无法播放直播".to_string());
        }
        if !net::is_allowed_stream_url(input_url) {
            return Err(format!("直播流地址不在允许范围内: {input_url}"));
        }

        let safe_id = safe_live_id(live_id);
        let token = new_session_token();
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = match sessions.get(&safe_id) {
            Some(session) => session.input_url != input_url,
            None => true,
        };
        if changed {
            drop(sessions);
            self.inner.cancel_active(&safe_id);
            sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        sessions.insert(
            safe_id.clone(),
            Session {
                input_url: input_url.to_string(),
                ffmpeg_path,
                token: token.clone(),
            },
        );
        drop(sessions);

        Ok(LiveStreamSession {
            url: format!(
                "http://{SERVER_HOST}:{}/live/{token}/{safe_id}.flv",
                self.port()
            ),
            live_id: live_id.to_string(),
        })
    }

    pub fn stop_session(&self, live_id: &str) {
        let safe_id = safe_live_id(live_id);
        let had_session = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&safe_id)
            .is_some();
        self.inner.cancel_active(&safe_id);
        if had_session {
            eprintln!("[media] 已移除直播会话: {live_id}");
        }
    }

    /// 应用退出前清理所有会话与转流。子进程另有 `kill_on_drop` 兜底。
    pub fn cleanup(&self) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let ids: Vec<String> = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.inner.cancel_active(&id);
        }
    }
}

/// 每条会话的路径令牌：16 字节随机数转 hex，既不可猜也不含路径分隔符。
fn new_session_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 定长比较：不因前缀相同而提前返回（路径里的 token 本来就是公开随机数，
/// 这里只是不给计时侧信道留口子，成本可以忽略）。
fn token_matches(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    expected
        .bytes()
        .zip(candidate.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub fn safe_live_id(live_id: &str) -> String {
    let safe: String = live_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "live".to_string()
    } else {
        safe
    }
}

fn router(inner: Arc<Inner>) -> Router {
    Router::new()
        .route("/live/{token}/{file}", get(live_flv).options(preflight))
        .fallback(not_found)
        .with_state(inner)
}

fn allowed_origin(origin: &str) -> bool {
    // Tauri 生产环境：macOS/Linux 自定义协议或 Windows 的 tauri.localhost。
    // 开发环境固定 1420（vite strictPort）；file:// / null 兼容旧打包形态。
    origin == "null"
        || origin.starts_with("file://")
        || origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin == "https://tauri.localhost"
        || origin == "http://localhost:1420"
        || origin == "http://127.0.0.1:1420"
}

/// 本地服务只服务 Tauri WebView / 开发服务器。
/// 缺 Origin 的请求（curl、任意本机进程）一律拒绝：地址虽然只绑 127.0.0.1，
/// 但把「本机能访问」当成授权会把服务变成任意进程可用的 RTMP 代理。
fn require_webview_origin(headers: &HeaderMap) -> Result<&str, &'static str> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or("Forbidden origin: missing Origin header")?;
    if !allowed_origin(origin) {
        return Err("Forbidden origin");
    }
    Ok(origin)
}

fn apply_cors(mut response: Response, origin: Option<&str>) -> Response {
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, HEAD"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Range"),
        );
    }
    response
}

fn no_cache_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(header::EXPIRES, HeaderValue::from_static("0"));
}

fn error_response(status: StatusCode, message: &str, origin: Option<&str>) -> Response {
    let mut response = (status, message.to_string()).into_response();
    no_cache_headers(response.headers_mut());
    if status != StatusCode::OK {
        // 文本错误体用 text/plain，避免浏览器按 FLV 解析。
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
    }
    apply_cors(response, origin)
}

async fn preflight(headers: HeaderMap) -> Response {
    let origin = match require_webview_origin(&headers) {
        Ok(origin) => origin,
        Err(message) => return error_response(StatusCode::FORBIDDEN, message, None),
    };
    let mut response = apply_cors(StatusCode::NO_CONTENT.into_response(), Some(origin));
    response.headers_mut().insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

async fn not_found(headers: HeaderMap) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if let Some(origin) = origin
        && !allowed_origin(origin)
    {
        return error_response(StatusCode::FORBIDDEN, "Forbidden origin", None);
    }
    error_response(StatusCode::NOT_FOUND, "File not found", origin)
}

async fn live_flv(
    State(inner): State<Arc<Inner>>,
    AxumPath((token, file)): AxumPath<(String, String)>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let origin = match require_webview_origin(&headers) {
        Ok(origin) => Some(origin),
        Err(message) => return error_response(StatusCode::FORBIDDEN, message, None),
    };

    let Some(live_id) = file.strip_suffix(".flv").filter(|id| !id.is_empty()) else {
        return error_response(StatusCode::NOT_FOUND, "File not found", origin);
    };

    // 会话 + token 必须在同一把锁里取：否则 stop / 重新登记后仍可能拿到旧会话。
    let session = {
        let sessions = inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .get(live_id)
            .filter(|session| token_matches(&session.token, &token))
            .cloned()
    };
    // token 不匹配与「会话不存在」返回同一状态：不给本机进程留下「这个 liveId 存在」的信号。
    let Some(session) = session else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("直播流不存在: {live_id}"),
            origin,
        );
    };

    // HEAD 不 spawn FFmpeg：探测可用性不应产生转流进程。
    if method == Method::HEAD {
        let mut response = StatusCode::OK.into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("video/x-flv"),
        );
        no_cache_headers(response.headers_mut());
        return apply_cors(response, origin);
    }

    if !inner.has_capacity(live_id) {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("同一场直播的并发转流已达上限（{MAX_STREAMS_PER_SESSION}）"),
            origin,
        );
    }

    eprintln!(
        "[media] 收到播放请求: /live/{live_id}.flv origin={}",
        origin.unwrap_or("-")
    );
    let mut command = Command::new(&session.ffmpeg_path);
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-fflags")
        .arg("nobuffer")
        .arg("-flags")
        .arg("low_delay")
        .arg("-rtmp_live")
        .arg("live")
        .arg("-i")
        .arg(&session.input_url)
        .arg("-map")
        .arg("0:v:0?")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-c:v")
        .arg("copy")
        .arg("-c:a")
        .arg("copy")
        .arg("-f")
        .arg("flv")
        .arg("pipe:1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("[media] spawn FFmpeg 失败 ({live_id}): {err}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("启动 FFmpeg 失败: {err}"),
                origin,
            );
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法读取 FFmpeg 输出",
            origin,
        );
    };
    if let Some(stderr) = child.stderr.take() {
        let live_id = live_id.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[media] ffmpeg stderr ({live_id}): {line}");
            }
        });
    }

    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let active_id = inner.register_active(live_id, cancel_tx);
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let inner_for_task = inner.clone();
    let live_id_owned = live_id.to_string();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            tokio::select! {
                changed = cancel_rx.changed() => {
                    if changed.is_err() || *cancel_rx.borrow() {
                        break;
                    }
                }
                read = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stdout.read(&mut buf)) => {
                    match read {
                        Err(_) => {
                            eprintln!("[media] {live_id_owned} 超过 {}ms 无流数据，结束响应", STREAM_IDLE_TIMEOUT.as_millis());
                            break;
                        }
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            if tx.send(Ok(chunk)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Err(err)) => {
                            eprintln!("[media] 读取 {live_id_owned} 流失败: {err}");
                            let _ = tx.send(Err(err)).await;
                            break;
                        }
                    }
                }
            }
        }

        // 客户端断开、空闲、上游结束或会话被 stop：都尽快结束 FFmpeg。
        let _ = child.start_kill();
        let _ = child.wait().await;
        inner_for_task.unregister_active(&live_id_owned, active_id);
        eprintln!("[media] {live_id_owned} 转流进程已退出");
    });

    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("video/x-flv"),
    );
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    no_cache_headers(response.headers_mut());
    apply_cors(response, origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("d48-media-{name}-{unique}"))
    }

    #[cfg(unix)]
    fn fake_ffmpeg(dir: &std::path::Path, payload: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let payload_path = dir.join("payload.bin");
        std::fs::write(&payload_path, payload).unwrap();
        let script = dir.join("ffmpeg");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nexec cat '{}'\n", payload_path.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[test]
    fn live_id_is_sanitized_like_electron() {
        assert_eq!(safe_live_id("1330968594399170560"), "1330968594399170560");
        assert_eq!(safe_live_id("a/b:c"), "a_b_c");
        assert_eq!(safe_live_id(""), "live");
    }

    #[test]
    fn rejects_non_allowlisted_stream_url() {
        assert!(!net::is_allowed_stream_url(
            "rtmp://evil.example.com/live/1"
        ));
        assert!(net::is_allowed_stream_url("rtmp://idol.tv.48.cn/live/1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serves_flv_and_enforces_origin() {
        let dir = temp_dir("serve");
        let payload = b"FLV\x01\x05\x00\x00\x00\x09fake-stream";
        let script = fake_ffmpeg(&dir, payload);
        let media = MediaServer::new();
        let port = media.start().unwrap();
        let session = media
            .create_session("test-live", "rtmp://idol.tv.48.cn/live/test", script)
            .unwrap();
        let base = format!("http://{SERVER_HOST}:{port}");
        // 直接用 create_session 返回的地址：端口随机、路径带会话 token。
        let url = session.url.clone();
        let client = reqwest::Client::new();

        let response = client
            .get(&url)
            .header(header::ORIGIN, "http://localhost:1420")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("video/x-flv")
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("http://localhost:1420")
        );
        let body = response.bytes().await.unwrap();
        assert_eq!(&body[..], payload);

        let forbidden = client
            .get(&url)
            .header(header::ORIGIN, "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let unknown = client
            .get(format!(
                "{base}/live/{}/missing.flv",
                session_token_of(&media, "test-live")
            ))
            .header(header::ORIGIN, "http://localhost:1420")
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // 没有 Origin 的本机进程（curl 等）不能借用转流能力。
        let no_origin = client.get(&url).send().await.unwrap();
        assert_eq!(no_origin.status(), StatusCode::FORBIDDEN);

        media.cleanup();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话 token（测试用：直接读内部表，不经过 URL 解析）。
    fn session_token_of(media: &MediaServer, live_id: &str) -> String {
        media
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(live_id)
            .map(|session| session.token.clone())
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wrong_or_missing_token_cannot_start_ffmpeg() {
        let dir = temp_dir("token");
        let script = fake_ffmpeg(&dir, b"FLV");
        let media = MediaServer::new();
        let port = media.start().unwrap();
        media
            .create_session("live-a", "rtmp://idol.tv.48.cn/live/a", script)
            .unwrap();
        let base = format!("http://{SERVER_HOST}:{port}");
        let token = session_token_of(&media, "live-a");
        let client = reqwest::Client::new();

        // 猜错 token / 省略 token：一律 404，且不产生转流进程。
        for path in [
            format!("/live/{}/live-a.flv", "0".repeat(32)),
            "/live/live-a.flv".to_string(),
            format!("/live/{token}x/live-a.flv"),
        ] {
            let response = client
                .get(format!("{base}{path}"))
                .header(header::ORIGIN, "http://localhost:1420")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "路径 {path} 不应被接受"
            );
        }
        assert!(
            media.inner.active.lock().unwrap().is_empty(),
            "鉴权失败不能登记转流"
        );

        media.cleanup();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_urls_carry_a_fresh_token() {
        let media = MediaServer::new();
        media.start().unwrap();
        let first = media
            .create_session(
                "66",
                "rtmp://idol.tv.48.cn/live/66",
                PathBuf::from("ffmpeg"),
            )
            .unwrap();
        let second = media
            .create_session(
                "66",
                "rtmp://idol.tv.48.cn/live/66",
                PathBuf::from("ffmpeg"),
            )
            .unwrap();
        assert_ne!(first.url, second.url, "重新登记必须换发 token");
        assert!(first.url.contains("/live/"));
        assert!(first.url.ends_with("/66.flv"));
        let token = session_token_of(&media, "66");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(second.url.contains(&token), "URL 里必须是当前有效 token");
        assert!(!first.url.contains(&token), "旧 URL 的 token 已失效");
        media.cleanup();
    }

    #[test]
    fn token_comparison_rejects_prefix_and_length_mismatch() {
        assert!(token_matches("abcd", "abcd"));
        assert!(!token_matches("abcd", "abce"));
        assert!(!token_matches("abcd", "abc"));
        assert!(!token_matches("abcd", ""));
    }

    #[test]
    fn started_port_is_system_assigned() {
        let media = MediaServer::new();
        let port = media.start().unwrap();
        assert_ne!(port, 0, "必须拿到真实端口");
        // 不再固定落在 8080–8090：系统分配端口，冲突不再让直播整体不可用。
        assert_eq!(media.start().unwrap(), port, "重复 start 应幂等");
        media.cleanup();
    }

    #[test]
    fn online_streams_are_capped_per_session() {
        let inner = Inner::new();
        assert!(inner.has_capacity("x"));
        for _ in 0..MAX_STREAMS_PER_SESSION {
            let (tx, _rx) = watch::channel(false);
            inner.register_active("x", tx);
        }
        assert!(!inner.has_capacity("x"), "达到上限后应拒绝新转流");
        assert!(inner.has_capacity("y"), "上限按会话独立计算");
    }

    #[test]
    fn session_requires_started_server() {
        let media = MediaServer::new();
        let err = media
            .create_session("x", "rtmp://idol.tv.48.cn/live/x", PathBuf::from("ffmpeg"))
            .unwrap_err();
        assert!(err.contains("尚未启动"), "{err}");
    }
}
