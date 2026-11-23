//! 口袋48 直播聊天室（弹幕）客户端（对照 Electron `nim-chatroom.ts`）。
//!
//! 一层薄客户端：LBS 取地址 → 用 `nim_session` 建连登录 → 收 13/7 消息 → 按需回 13/35 回执，
//! 断线按 1s→2s→…→30s 退避重连。
//!
//! 协议原语（帧 / 属性表 / RC4 / 握手）全部来自 `nim.rs`，会话骨架来自 `nim_session.rs`，
//! 本模块只负责聊天室自己的服务号语义与消息归一。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use crate::app_log::log_line;
use crate::nim::{
    ChatMessage, IM_APP_KEY, build_chatroom_login_body, encode_frame, encode_properties,
    fetch_chatroom_address, normalize_message,
};
use crate::nim_session::{CommonlinkSession, LoginSpec, RECONNECT_MAX_DELAY, SessionError};

const SERVICE_CHATROOM: u8 = 13;
const CMD_CHATROOM_LOGIN: u8 = 2;
const CMD_CHATROOM_MESSAGE: u8 = 7;
const CMD_CHATROOM_ACK: u8 = 35;

/// 消息去重容量：环形丢弃最老的 uuid，防长直播内存增长。
const SEEN_LIMIT: usize = 2048;

/// 客户端生命周期事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatroomEvent {
    Connected,
    Reconnecting { attempt: u32 },
    Disconnected { message: String },
    Message { message: ChatMessage },
    Error { message: String },
}

#[derive(Clone)]
pub struct ChatroomOptions {
    pub app_key: String,
    pub account: String,
    pub token: String,
    pub room_id: String,
    pub device_id: String,
    pub sdk_version: u32,
    pub package_name: String,
}

pub struct NimChatroomClient {
    options: ChatroomOptions,
    stopped: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ChatroomEvent>,
}

impl NimChatroomClient {
    /// 建客户端但不连接；事件通过 `take_events()` 取。
    pub fn new(options: ChatroomOptions, events: mpsc::UnboundedSender<ChatroomEvent>) -> Self {
        Self {
            options,
            stopped: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            events,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// 首次连接；失败即终态（不自动重连），由调用方决定是否再起会话。
    pub async fn connect(&self) -> Result<(), String> {
        let room = self.options.room_id.trim();
        if self.options.app_key.is_empty()
            || self.options.account.is_empty()
            || self.options.token.is_empty()
            || room.is_empty()
            || room.len() > 32
            || !room.chars().all(|ch| ch.is_ascii_digit())
        {
            return Err("聊天室连接参数无效（appKey/账号/凭据/房间 ID）".to_string());
        }
        self.stopped.store(false, Ordering::Relaxed);
        self.open().await
    }

    pub fn destroy(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.connected.store(false, Ordering::Relaxed);
    }

    fn emit(&self, event: ChatroomEvent) {
        let _ = self.events.send(event);
    }

    fn report(&self, message: impl Into<String>) {
        self.emit(ChatroomEvent::Error {
            message: message.into(),
        });
    }

    async fn open(&self) -> Result<(), String> {
        let address = fetch_chatroom_address(
            &self.options.app_key,
            &self.options.account,
            &self.options.room_id,
            self.options.sdk_version,
        )
        .await?;

        let login_body = build_chatroom_login_body(
            &self.options.app_key,
            &self.options.account,
            &self.options.token,
            &self.options.room_id,
            &self.options.device_id,
            self.options.sdk_version,
            &self.options.package_name,
        );
        // 登录包 serial 硬编码 1（与 Electron 一致）：会话 serial 从 2 起
        let spec = LoginSpec {
            address,
            login_packet: encode_frame(SERVICE_CHATROOM, CMD_CHATROOM_LOGIN, 1, &login_body),
            login_service: SERVICE_CHATROOM,
            login_command: CMD_CHATROOM_LOGIN,
            login_label: "聊天室".to_string(),
        };
        let session = Arc::new(
            CommonlinkSession::connect(&spec)
                .await
                .map_err(|err: SessionError| err.to_string())?,
        );
        self.connected.store(true, Ordering::Relaxed);
        self.emit(ChatroomEvent::Connected);
        log_line(&format!(
            "[nim-chatroom] 聊天室 {} 已进入（账号 {}）",
            self.options.room_id,
            mask_account(&self.options.account)
        ));

        let mut frames = session.subscribe();
        let mut seen: HashSet<String> = HashSet::new();
        let room_id = self.options.room_id.clone();
        loop {
            if self.stopped.load(Ordering::Relaxed) || !session.is_alive() {
                break;
            }
            let frame = match tokio::time::timeout(Duration::from_secs(5), frames.recv()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => break,
                Err(_) => continue,
            };
            if frame.service_id != SERVICE_CHATROOM || frame.command_id != CMD_CHATROOM_MESSAGE {
                continue;
            }
            let Ok(message) = normalize_message(&frame.body) else {
                // 属性表解析失败不能静默：表现为「已进入但永远收不到消息」
                self.report(format!("消息解析失败（{}B）", frame.body.len()));
                continue;
            };
            if !message.uuid.is_empty() {
                if !seen.insert(message.uuid.clone()) {
                    continue;
                }
                if seen.len() > SEEN_LIMIT
                    && let Some(oldest) = seen.iter().next().cloned()
                {
                    seen.remove(&oldest);
                }
            }
            if message.needs_ack && !message.uuid.is_empty() {
                session.send(
                    SERVICE_CHATROOM,
                    CMD_CHATROOM_ACK,
                    encode_properties(&[(1, message.uuid.clone()), (2, room_id.clone())]),
                );
            }
            self.emit(ChatroomEvent::Message { message });
        }

        session.destroy();
        self.connected.store(false, Ordering::Relaxed);
        if !self.stopped.load(Ordering::Relaxed) {
            self.emit(ChatroomEvent::Disconnected {
                message: "连接已断开".to_string(),
            });
            self.schedule_reconnect();
        }
        Ok(())
    }

    /// 断线重连：1s→2s→4s…上限 30s；`destroy()` 之后不再重连。
    fn schedule_reconnect(&self) {
        let Some(pool) = current_runtime() else {
            return;
        };
        // 用一个独立任务持有重试链，避免递归 async
        let stopped = self.stopped.clone();
        let connected = self.connected.clone();
        let events = self.events.clone();
        let options = ChatroomOptions {
            app_key: self.options.app_key.clone(),
            account: self.options.account.clone(),
            token: self.options.token.clone(),
            room_id: self.options.room_id.clone(),
            device_id: self.options.device_id.clone(),
            sdk_version: self.options.sdk_version,
            package_name: self.options.package_name.clone(),
        };
        pool.spawn(async move {
            let mut attempt = 0u32;
            loop {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                attempt += 1;
                let delay = Duration::from_millis(1000 * 2u64.pow(attempt.min(5)));
                let delay = delay.min(RECONNECT_MAX_DELAY);
                let _ = events.send(ChatroomEvent::Reconnecting { attempt });
                tokio::time::sleep(delay).await;
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                let client = NimChatroomClient {
                    options: options.clone(),
                    stopped: stopped.clone(),
                    connected: connected.clone(),
                    events: events.clone(),
                };
                match client.open().await {
                    Ok(()) => return,
                    Err(err) => {
                        let _ = events.send(ChatroomEvent::Error { message: err });
                    }
                }
            }
        });
    }
}

fn current_runtime() -> Option<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().ok()
}

/// 账号脱敏：日志里只留前 6 位。
fn mask_account(account: &str) -> String {
    if account.chars().count() <= 8 {
        return "***".to_string();
    }
    format!("{}…", account.chars().take(6).collect::<String>())
}

/// 直播聊天室的 appKey 来自 Electron 常量（不在 im/userinfo 响应里）。
pub fn app_key() -> &'static str {
    IM_APP_KEY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_is_masked_in_logs() {
        assert_eq!(mask_account("short"), "***");
        assert_eq!(mask_account("20200602025051_gawphuof3m3xro7"), "202006…");
    }

    #[tokio::test]
    async fn invalid_room_id_is_rejected_before_network() {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let client = NimChatroomClient::new(
            ChatroomOptions {
                app_key: "key".to_string(),
                account: "acc".to_string(),
                token: "tok".to_string(),
                room_id: "not-a-room".to_string(),
                device_id: "0000000000000000".to_string(),
                sdk_version: 92012,
                package_name: "com.pocket.snh48.activity".to_string(),
            },
            sender,
        );
        let err = client.connect().await.unwrap_err();
        assert!(err.contains("聊天室连接参数无效"));
    }
}
