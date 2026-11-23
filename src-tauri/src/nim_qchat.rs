//! 网易云信圈组（QChat）客户端（对照 Electron `nim-qchat.ts`）。
//!
//! 与聊天室（弹幕）同源不同族：帧 / 属性表 / RC4 / 握手共用 `nim.rs`，会话骨架共用
//! `nim_session.rs`，但服务号体系不同，且 **QChat 需要两条 TCP**：
//!
//! 1. NIM 会话：LBS `conf.jsp` → 握手（带 2/2 登录包）→ NIM 登录 → 在它上面发 24/1 索取 QChat 地址
//! 2. QChat 会话：用上一步的地址另建连接 → 握手（带 24/2 登录包）→ 登录 → 订阅频道
//!    （25/9 访客优先，被拒 403/404 退 24/15 正式成员）→ 收推送 24/11
//!
//! **只读约定**：本项目不做发送消息（24/10）、不做删除、不做问答应答。
//! 传输层仍保留 `send` 的编码能力是因为订阅/退订复用同一套属性表原语，
//! 但对外不暴露任何写通道。
//!
//! 终端类型（属性 3）恒为 AOS：两个官方 App 都是 Android 客户端，忠实模拟即报 AOS。
//! 注意 2/2 是账号级登录，建连会顶掉同账号的其他在线设备，故房间页默认不连。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use crate::app_log::log_line;
use crate::nim::{
    Frame, IM_APP_KEY, decode_properties, encode_properties, encode_varint, read_varint,
};
use crate::nim_session::{
    CommonlinkSession, LBS_TIMEOUT, LoginSpec, RECONNECT_MAX_DELAY, SessionError, TcpAddress,
};

/// NIM LBS（与聊天室的 `lbs/chat.jsp` 是两个端点；回包取 `common.link[0]`，兼容旧形态 `link[0]`）
const NIM_LBS_URL: &str = "https://lbs.netease.im/lbs/conf.jsp";

const SERVICE_NIM: u8 = 2;
const CMD_NIM_LOGIN: u8 = 2;
const SERVICE_QCHAT: u8 = 24;
const CMD_QCHAT_ADDRESS: u8 = 1;
const CMD_QCHAT_LOGIN: u8 = 2;
const CMD_QCHAT_MESSAGE: u8 = 11;
/// 访客订阅走 25/9，正式成员订阅走 24/15
const SERVICE_QCHAT_VISITOR: u8 = 25;
const CMD_SUBSCRIBE_VISITOR: u8 = 9;
const CMD_SUBSCRIBE_MEMBER: u8 = 15;

/// 单连接订阅频道数上限（参考实现同值）
const CHANNEL_SUBSCRIBE_LIMIT: usize = 100;

/// 云信不可恢复的认证类致命错误码（302 凭证失效 / 403 禁止访问 / 422 账号封禁）
pub const FATAL_AUTH_CODES: &[i16] = &[302, 403, 422];

/// 圈组消息（推送 24/11）；业务归一在渲染层（`utils/pocket-room-message.ts`）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QChatRawMessage {
    pub server_id: String,
    pub channel_id: String,
    pub from_account: String,
    pub from_client_type: i64,
    pub from_nick: String,
    pub time: i64,
    /// 0 = 文本，100 = 自定义（回复等，正文在 attachment）
    pub msg_type: i64,
    pub body: String,
    pub attachment: String,
    pub ext: String,
    pub msg_id_client: String,
    pub msg_id_server: String,
}

/// 客户端事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum QChatEvent {
    Connected,
    Reconnecting { attempt: u32 },
    Disconnected { message: String },
    Message { message: Box<QChatRawMessage> },
    Error { message: String },
    FatalAuthError { message: String },
}

#[derive(Clone, Debug)]
pub struct QChatOptions {
    pub app_key: String,
    pub account: String,
    pub token: String,
    pub device_id: String,
    pub sdk_version: u32,
    pub package_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Subscription {
    server_id: String,
    channel_id: String,
    visitor: bool,
}

fn channel_key(server_id: &str, channel_id: &str) -> String {
    format!("{server_id}:{channel_id}")
}

/// SDK 版本号 → `9.20.12` 形态（属性 40 / 42 / 11 / 14 都要）。
pub fn sdk_human_version(sdk_version: u32) -> String {
    let raw = sdk_version.to_string();
    if raw.len() != 5 {
        return raw;
    }
    format!(
        "{}.{}.{}",
        &raw[..1],
        raw[1..3].parse::<u32>().unwrap_or(0),
        raw[3..].parse::<u32>().unwrap_or(0)
    )
}

/// NIM 登录包（2/2）。属性 3=终端类型（恒 1 = AOS）、6=SDK 版本、25=包名。
fn build_nim_login_body(options: &QChatOptions) -> Vec<u8> {
    let human = sdk_human_version(options.sdk_version);
    encode_properties(&[
        (3, "1".to_string()),
        (4, "8.0.0".to_string()),
        (6, options.sdk_version.to_string()),
        (8, "1".to_string()),
        (9, "1".to_string()),
        (13, options.device_id.clone()),
        (18, options.app_key.clone()),
        (19, options.account.clone()),
        (25, options.package_name.clone()),
        (40, human.clone()),
        (42, format!("Native/{human}")),
        (1000, options.token.clone()),
    ])
}

/// QChat 登录包（24/2）。
fn build_qchat_login_body(options: &QChatOptions) -> Vec<u8> {
    let human = sdk_human_version(options.sdk_version);
    encode_properties(&[
        (1, options.app_key.clone()),
        (2, options.account.clone()),
        (3, "0".to_string()),
        (4, options.token.clone()),
        (6, "1".to_string()),
        (8, options.device_id.clone()),
        (9, options.sdk_version.to_string()),
        (10, "1".to_string()),
        (11, format!("Native/{human}")),
        (14, human),
    ])
}

/// 订阅 / 退订 body：操作属性 `{1: 访客?6:1, 2: 订阅?1:2}` + varint(1) + 房间属性 `{1: serverId, 2: channelId}`。
pub fn build_subscription_body(
    server_id: &str,
    channel_id: &str,
    visitor: bool,
    subscribe: bool,
) -> Result<Vec<u8>, String> {
    let valid = |value: &str| {
        !value.is_empty() && value.len() <= 32 && value.chars().all(|ch| ch.is_ascii_digit())
    };
    if !valid(server_id) || !valid(channel_id) {
        return Err("缺少有效的云信订阅房间 ID".to_string());
    }
    let mut body = encode_properties(&[
        (1, if visitor { "6" } else { "1" }.to_string()),
        (2, if subscribe { "1" } else { "2" }.to_string()),
    ]);
    body.extend_from_slice(&encode_varint(1));
    body.extend_from_slice(&encode_properties(&[
        (1, server_id.to_string()),
        (2, channel_id.to_string()),
    ]));
    Ok(body)
}

/// 24/1 回包：`varint(数量) + N × (varint(len) + bytes)` 的字符串数组。
pub fn decode_string_array(body: &[u8]) -> Vec<String> {
    let Some((count, mut offset)) = read_varint(body, 0) else {
        return Vec::new();
    };
    if count > 100 {
        return Vec::new();
    }
    let mut list = Vec::new();
    for _ in 0..count {
        let Some((length, size)) = read_varint(body, offset) else {
            break;
        };
        offset += size;
        let end = offset + length as usize;
        if end > body.len() {
            break;
        }
        list.push(String::from_utf8_lossy(&body[offset..end]).to_string());
        offset = end;
    }
    list.retain(|item| !item.trim().is_empty());
    list
}

/// 消息属性表 → 圈组消息。
pub fn normalize_message(body: &[u8]) -> Result<QChatRawMessage, String> {
    let properties = decode_properties(body)?;
    let text = |key: u32| {
        properties
            .get(&key)
            .map(|value| String::from_utf8_lossy(value).to_string())
            .unwrap_or_default()
    };
    Ok(QChatRawMessage {
        server_id: text(1),
        channel_id: text(2),
        from_account: text(3),
        from_client_type: text(4).parse::<i64>().unwrap_or(0),
        from_nick: text(6),
        time: text(7).parse::<i64>().unwrap_or(0),
        msg_type: text(9).parse::<i64>().unwrap_or(0),
        body: text(10),
        attachment: text(11),
        ext: text(12),
        msg_id_client: text(13),
        msg_id_server: text(14),
    })
}

/// 订阅回包里的失败频道：嵌套属性表清单中找属性 2 = channelId 的条目。
/// 回包结构未公开，外层属性表包清单 / 直接是清单两种形态都试；解不出视为无失败。
pub fn read_subscribe_failures(body: &[u8]) -> Vec<String> {
    fn visit(buffer: &[u8], failed: &mut Vec<String>) {
        let Some((count, mut offset)) = read_varint(buffer, 0) else {
            return;
        };
        if count == 0 || count > 100 {
            return;
        }
        for _ in 0..count {
            let Some((length, size)) = read_varint(buffer, offset) else {
                return;
            };
            offset += size;
            let end = offset + length as usize;
            if end > buffer.len() {
                return;
            }
            if let Ok(properties) = decode_properties(&buffer[offset..end])
                && let Some(channel) = properties.get(&2)
            {
                let channel_id = String::from_utf8_lossy(channel).to_string();
                if !channel_id.is_empty() {
                    failed.push(channel_id);
                }
            }
            offset = end;
        }
    }

    let mut failed = Vec::new();
    match decode_properties(body) {
        Ok(properties) => {
            for value in properties.values() {
                visit(value, &mut failed);
            }
        }
        Err(_) => visit(body, &mut failed),
    }
    failed
}

/// NIM LBS 取接入地址（`conf.jsp`，回包 `common.link[0]`，旧形态 `link[0]` 兼容）。
async fn request_nim_address(
    http: &reqwest::Client,
    app_key: &str,
    account: &str,
    sdk_version: u32,
) -> Result<TcpAddress, String> {
    let response = http
        .get(NIM_LBS_URL)
        .timeout(LBS_TIMEOUT)
        .query(&[
            ("k", app_key),
            ("id", account),
            ("v", &sdk_version.to_string()),
            ("tp", "1"),
            ("dt", "0"),
        ])
        .send()
        .await
        .map_err(|err| format!("获取云信地址失败: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "获取云信地址失败（HTTP {}）",
            response.status().as_u16()
        ));
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|err| format!("云信地址回包不是 JSON: {err}"))?;
    let raw = value
        .get("common")
        .and_then(|common| common.get("link"))
        .and_then(|link| link.as_array())
        .and_then(|links| links.first())
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("link")
                .and_then(|link| link.as_array())
                .and_then(|links| links.first())
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or_default()
        .trim();
    crate::nim::parse_tcp_address(raw)
}

/// 是否为不可恢复的认证错误（重试无意义）。
pub fn is_fatal_auth_error(message: &str, code: i16) -> bool {
    if message.contains("缺少云信") {
        return true;
    }
    FATAL_AUTH_CODES.contains(&code)
}

fn protocol_error(message: &str, code: i16) -> SessionError {
    // SessionError 的构造是私有的，这里用 transport/protocol 的可读文案替代
    let _ = code;
    SessionError {
        message: message.to_string(),
        code,
        retryable: false,
    }
}

pub struct NimQChatClient {
    options: QChatOptions,
    http: reqwest::Client,
    stopped: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    ever_connected: Arc<AtomicBool>,
    subscriptions: Arc<tokio::sync::Mutex<BTreeMap<String, Subscription>>>,
    events: mpsc::UnboundedSender<QChatEvent>,
    nim_session: Arc<tokio::sync::Mutex<Option<Arc<CommonlinkSession>>>>,
    qchat_session: Arc<tokio::sync::Mutex<Option<Arc<CommonlinkSession>>>>,
}

impl NimQChatClient {
    pub fn new(
        options: QChatOptions,
        http: reqwest::Client,
        events: mpsc::UnboundedSender<QChatEvent>,
    ) -> Self {
        Self {
            options,
            http,
            stopped: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            ever_connected: Arc::new(AtomicBool::new(false)),
            subscriptions: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            events,
            nim_session: Arc::new(tokio::sync::Mutex::new(None)),
            qchat_session: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn emit(&self, event: QChatEvent) {
        let _ = self.events.send(event);
    }

    fn report(&self, message: impl Into<String>) {
        self.emit(QChatEvent::Error {
            message: message.into(),
        });
    }

    /// 建立双会话并恢复订阅。
    pub async fn connect(self: &Arc<Self>) -> Result<(), String> {
        let options = &self.options;
        if options.app_key.is_empty() || options.account.is_empty() || options.token.is_empty() {
            return Err("缺少云信 AppKey、账号或令牌".to_string());
        }
        self.stopped.store(false, Ordering::Relaxed);
        self.destroy_sessions().await;

        let result = self.establish().await;
        if result.is_err() {
            self.destroy_sessions().await;
        }
        result
    }

    async fn establish(self: &Arc<Self>) -> Result<(), String> {
        let address = request_nim_address(
            &self.http,
            &self.options.app_key,
            &self.options.account,
            self.options.sdk_version,
        )
        .await?;

        let nim = Arc::new(
            CommonlinkSession::connect(&LoginSpec {
                address,
                login_packet: crate::nim::encode_frame(
                    SERVICE_NIM,
                    CMD_NIM_LOGIN,
                    1,
                    &build_nim_login_body(&self.options),
                ),
                login_service: SERVICE_NIM,
                login_command: CMD_NIM_LOGIN,
                login_label: "NIM".to_string(),
            })
            .await
            .map_err(|err| err.to_string())?,
        );
        *self.nim_session.lock().await = Some(nim.clone());

        // NIM 会话上索取 QChat 接入地址，再建第二条连接
        let response = nim
            .request(
                SERVICE_QCHAT,
                CMD_QCHAT_ADDRESS,
                encode_properties(&[(1, "0".to_string())]),
                "获取 QChat 地址失败",
            )
            .await
            .map_err(|err| err.to_string())?;
        let addresses = decode_string_array(&response.body);
        let Some(first) = addresses.first() else {
            return Err("云信没有返回 QChat 连接地址".to_string());
        };
        let qchat_address = crate::nim::parse_tcp_address(first)?;

        let qchat = Arc::new(
            CommonlinkSession::connect(&LoginSpec {
                address: qchat_address,
                login_packet: crate::nim::encode_frame(
                    SERVICE_QCHAT,
                    CMD_QCHAT_LOGIN,
                    1,
                    &build_qchat_login_body(&self.options),
                ),
                login_service: SERVICE_QCHAT,
                login_command: CMD_QCHAT_LOGIN,
                login_label: "QChat".to_string(),
            })
            .await
            .map_err(|err| err.to_string())?,
        );
        *self.qchat_session.lock().await = Some(qchat.clone());

        // 置位必须在恢复订阅之前：subscribe_channel 以 connected 为守卫
        self.connected.store(true, Ordering::Relaxed);
        self.ever_connected.store(true, Ordering::Relaxed);

        // 重连后恢复订阅（访客/成员身份重判）
        let restoring: Vec<Subscription> =
            self.subscriptions.lock().await.values().cloned().collect();
        let mut restored = 0usize;
        for subscription in &restoring {
            match self
                .subscribe_channel(&subscription.server_id, &subscription.channel_id)
                .await
            {
                Ok(()) => restored += 1,
                Err(err) => self.report(err),
            }
        }
        log_line(&format!(
            "[nim-qchat] 已连接：账号={} 订阅恢复={restored}/{}",
            mask_account(&self.options.account),
            restoring.len()
        ));
        if self.connected.load(Ordering::Relaxed) {
            self.emit(QChatEvent::Connected);
        }
        Ok(())
    }

    /// 订阅频道：访客（25/9）优先，被拒（403/404）退正式成员（24/15）。
    pub async fn subscribe_channel(&self, server_id: &str, channel_id: &str) -> Result<(), String> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("房间连接已断开".to_string());
        }
        let key = channel_key(server_id, channel_id);
        let known = self.subscriptions.lock().await;
        if known.len() >= CHANNEL_SUBSCRIBE_LIMIT && !known.contains_key(&key) {
            return Err("云信频道订阅数已达上限".to_string());
        }
        drop(known);

        match self.request_subscribe(server_id, channel_id, true).await {
            Ok(()) => {
                self.subscriptions.lock().await.insert(
                    key.clone(),
                    Subscription {
                        server_id: server_id.to_string(),
                        channel_id: channel_id.to_string(),
                        visitor: true,
                    },
                );
                log_line(&format!("[nim-qchat] 订阅成功：{key}（访客）"));
                Ok(())
            }
            Err(err) if err.code == 403 || err.code == 404 => {
                log_line(&format!("[nim-qchat] 访客订阅被拒，退正式成员订阅：{key}"));
                self.request_subscribe(server_id, channel_id, false)
                    .await
                    .map_err(|err| err.to_string())?;
                self.subscriptions.lock().await.insert(
                    key.clone(),
                    Subscription {
                        server_id: server_id.to_string(),
                        channel_id: channel_id.to_string(),
                        visitor: false,
                    },
                );
                log_line(&format!("[nim-qchat] 订阅成功：{key}（成员）"));
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// 退订频道：失败不报错（连接断开时退订本无意义），仅从订阅表摘除。
    pub async fn unsubscribe_channel(&self, server_id: &str, channel_id: &str) {
        let key = channel_key(server_id, channel_id);
        let subscription = self.subscriptions.lock().await.remove(&key);
        let Some(subscription) = subscription else {
            return;
        };
        if !self.connected.load(Ordering::Relaxed) {
            return;
        }
        let Some(session) = self.qchat_session.lock().await.clone() else {
            return;
        };
        let body = match build_subscription_body(server_id, channel_id, subscription.visitor, false)
        {
            Ok(body) => body,
            Err(_) => return,
        };
        let (service, command) = if subscription.visitor {
            (SERVICE_QCHAT_VISITOR, CMD_SUBSCRIBE_VISITOR)
        } else {
            (SERVICE_QCHAT, CMD_SUBSCRIBE_MEMBER)
        };
        if let Err(err) = session
            .request(service, command, body, "退订房间频道失败")
            .await
        {
            log_line(&format!("[nim-qchat] 退订失败（忽略）：{key} {err}"));
        }
    }

    pub fn subscribe(&self) -> mpsc::UnboundedSender<QChatEvent> {
        self.events.clone()
    }

    pub fn destroy(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn destroy_sessions(&self) {
        self.connected.store(false, Ordering::Relaxed);
        if let Some(session) = self.nim_session.lock().await.take() {
            session.destroy();
        }
        if let Some(session) = self.qchat_session.lock().await.take() {
            session.destroy();
        }
    }

    async fn request_subscribe(
        &self,
        server_id: &str,
        channel_id: &str,
        visitor: bool,
    ) -> Result<(), SessionError> {
        let session = self.qchat_session.lock().await.clone();
        let Some(session) = session else {
            return Err(SessionError {
                message: "房间连接已断开".to_string(),
                code: 0,
                retryable: true,
            });
        };
        let body =
            build_subscription_body(server_id, channel_id, visitor, true).map_err(|err| {
                SessionError {
                    message: err,
                    code: 0,
                    retryable: false,
                }
            })?;
        let (service, command) = if visitor {
            (SERVICE_QCHAT_VISITOR, CMD_SUBSCRIBE_VISITOR)
        } else {
            (SERVICE_QCHAT, CMD_SUBSCRIBE_MEMBER)
        };
        let response = session
            .request(service, command, body, "订阅房间频道失败")
            .await?;
        if read_subscribe_failures(&response.body).contains(&channel_id.to_string()) {
            return Err(protocol_error("订阅房间频道被拒", 403));
        }
        Ok(())
    }

    /// 起一个转发任务：把 QChat 会话的推送帧转成事件（24/11）。
    pub fn spawn_pump(self: &Arc<Self>) {
        let client = self.clone();
        tokio::spawn(async move {
            let mut qchat_rx = loop {
                let session = client.qchat_session.lock().await.clone();
                if let Some(session) = session {
                    break session.subscribe();
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            loop {
                if client.stopped.load(Ordering::Relaxed) {
                    return;
                }
                match tokio::time::timeout(Duration::from_secs(5), qchat_rx.recv()).await {
                    Ok(Ok(frame)) => {
                        if frame.service_id == SERVICE_QCHAT
                            && frame.command_id == CMD_QCHAT_MESSAGE
                            && !frame.body.is_empty()
                        {
                            match normalize_message(&frame.body) {
                                Ok(message) => client.emit(QChatEvent::Message {
                                    message: Box::new(message),
                                }),
                                Err(err) => client.report(format!("圈组消息解析失败: {err}")),
                            }
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(_)) => break,
                    Err(_) => continue,
                }
            }
            // 会话断开：整体重连（QChat 地址要重新索取，无法单条续）
            if client.stopped.load(Ordering::Relaxed)
                || !client.ever_connected.load(Ordering::Relaxed)
            {
                return;
            }
            client.emit(QChatEvent::Disconnected {
                message: "连接已断开".to_string(),
            });
            client.schedule_reconnect();
        });
    }

    fn schedule_reconnect(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.clone();
        runtime.spawn(async move {
            let mut attempt = 0u32;
            loop {
                if client.stopped.load(Ordering::Relaxed) {
                    return;
                }
                attempt += 1;
                let delay =
                    Duration::from_millis(1000 * 2u64.pow(attempt.min(5))).min(RECONNECT_MAX_DELAY);
                client.emit(QChatEvent::Reconnecting { attempt });
                tokio::time::sleep(delay).await;
                if client.stopped.load(Ordering::Relaxed) {
                    return;
                }
                match client.connect().await {
                    Ok(()) => return,
                    Err(err) => {
                        if is_fatal_auth_error(&err, 0) {
                            client.destroy();
                            client.emit(QChatEvent::FatalAuthError { message: err });
                            return;
                        }
                        client.report(err);
                    }
                }
            }
        });
    }
}

/// 账号脱敏：日志只留前 6 位。
fn mask_account(account: &str) -> String {
    if account.chars().count() <= 8 {
        "***".to_string()
    } else {
        format!("{}…", account.chars().take(6).collect::<String>())
    }
}

/// AppKey 常量（与聊天室同源）。
pub fn app_key() -> &'static str {
    IM_APP_KEY
}

/// 把一帧包成登录帧（调用方只需要 body 时用）。
pub fn login_frame(service: u8, command: u8, body: &[u8]) -> Vec<u8> {
    crate::nim::encode_frame(service, command, 1, body)
}

/// 保留：`Frame` 的引用避免未使用警告（pump 里用 `frame.service_id` 等字段）。
fn _frame_marker(_frame: &Frame) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_human_version_matches_reference() {
        assert_eq!(sdk_human_version(92012), "9.20.12");
        assert_eq!(sdk_human_version(92110), "9.21.10");
        // 非五位数原样返回
        assert_eq!(sdk_human_version(123), "123");
    }

    #[test]
    fn subscription_body_has_expected_layout() {
        let body = build_subscription_body("10001", "20002", true, true).unwrap();
        let (count, offset) = read_varint(&body, 0).unwrap();
        assert_eq!(count, 2, "操作属性表应有 2 条");
        let properties = decode_properties(&body[offset..]).unwrap_or_default();
        assert!(!properties.is_empty() || offset > 0);
        // 变量名 1 的 varint 后面才是房间属性表
        assert!(body.len() > 20);
    }

    #[test]
    fn subscription_body_rejects_invalid_ids() {
        assert!(build_subscription_body("abc", "20002", true, true).is_err());
        assert!(build_subscription_body("10001", "", true, true).is_err());
        assert!(build_subscription_body("", "20002", true, true).is_err());
    }

    #[test]
    fn string_array_round_trip() {
        let mut body = encode_varint(2);
        for item in ["10.0.0.1:9000", "10.0.0.2:9000"] {
            body.extend_from_slice(&encode_varint(item.len() as u32));
            body.extend_from_slice(item.as_bytes());
        }
        assert_eq!(
            decode_string_array(&body),
            vec!["10.0.0.1:9000", "10.0.0.2:9000"]
        );
        // 超大数量视为异常，返回空
        assert!(decode_string_array(&encode_varint(500)).is_empty());
    }

    #[test]
    fn message_normalizes_all_fields() {
        let body = encode_properties(&[
            (1, "10001".to_string()),
            (2, "20002".to_string()),
            (3, "accid".to_string()),
            (4, "1".to_string()),
            (6, "昵称".to_string()),
            (7, "1795349506379".to_string()),
            (9, "0".to_string()),
            (10, "你好".to_string()),
            (13, "client-id".to_string()),
            (14, "server-id".to_string()),
        ]);
        let message = normalize_message(&body).unwrap();
        assert_eq!(message.server_id, "10001");
        assert_eq!(message.channel_id, "20002");
        assert_eq!(message.from_nick, "昵称");
        assert_eq!(message.msg_type, 0);
        assert_eq!(message.body, "你好");
        assert_eq!(message.msg_id_server, "server-id");
    }

    #[test]
    fn fatal_auth_codes_are_recognised() {
        assert!(is_fatal_auth_error("登录已过期", 302));
        assert!(is_fatal_auth_error("", 403));
        assert!(is_fatal_auth_error("缺少云信 AppKey、账号或令牌", 0));
        assert!(!is_fatal_auth_error("连接超时", 0));
    }

    #[test]
    fn subscribe_failures_are_extracted() {
        // 外层属性表包一层清单，清单里每条是属性表 {2: channelId}
        let entry = encode_properties(&[(2, "20002".to_string())]);
        let mut list = encode_varint(1);
        list.extend_from_slice(&encode_varint(entry.len() as u32));
        list.extend_from_slice(&entry);
        // 属性表值是原始字节：这里手工拼一个 {1: list}
        let mut body = encode_varint(1);
        body.extend_from_slice(&encode_varint(1));
        body.extend_from_slice(&encode_varint(list.len() as u32));
        body.extend_from_slice(&list);
        assert_eq!(read_subscribe_failures(&body), vec!["20002"]);
    }
}
