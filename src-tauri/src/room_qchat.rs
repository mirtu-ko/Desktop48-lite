//! 房间 QChat 会话管理（对照 Electron `pocket-room-qchat.ts`）。
//!
//! 与弹幕同一「会话 + 订阅者」模式，但按**账号**持有：同一账号复用一条 QChat 连接，
//! 按频道订阅 / 退订（引用计数）；切房时**先订新房再退旧房**——旧频道若是最后一条订阅，
//! 退了会整条连接销毁，切房体验断裂。
//!
//! **只读约定**：不实现发送 / 删除 / 问答应答。
//! 进房只为「看」：订阅频道拿实时推送 + HTTP 拉历史。
//!
//! 建连会顶掉同账号的其他在线设备（手机端），故默认不连——由房间页确认后才 `enter`。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::mpsc;

use crate::app_log::log_line;
use crate::danmaku::nim_identity;
use crate::nim::IM_APP_KEY;
use crate::nim_qchat::{NimQChatClient, QChatEvent, QChatOptions, QChatRawMessage};

/// 房间批次（与 `ipc-contract.d.ts` 的 `PocketRoomBatch` 同形）。
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PocketRoomBatch {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<QChatRawMessage>>,
}

struct ChannelState {
    server_id: String,
    channel_id: String,
    /// 订阅该频道的窗口 label
    subscribers: Vec<String>,
}

struct RoomSession {
    client: std::sync::Arc<NimQChatClient>,
    channels: HashMap<String, ChannelState>,
}

#[derive(Default)]
pub struct RoomQChatSessions {
    sessions: Mutex<HashMap<String, RoomSession>>,
}

/// 进房结果（与 `PocketRoomQChatEnterResult` 同形）。
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EnterResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn key_of(server_id: &str, channel_id: &str) -> String {
    format!("{server_id}:{channel_id}")
}

impl RoomQChatSessions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RoomSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 进入房间频道：确保会话存在、连接就绪、频道已订阅。
    #[allow(clippy::too_many_arguments)]
    pub async fn enter<F>(
        &self,
        server_id: &str,
        channel_id: &str,
        subscriber: &str,
        credentials: Option<(String, String)>,
        profile: &str,
        device_id: &str,
        http: reqwest::Client,
        deliver: F,
    ) -> Result<EnterResult, String>
    where
        F: Fn(PocketRoomBatch) + Send + Sync + 'static,
    {
        if server_id.is_empty() || channel_id.is_empty() {
            return Ok(EnterResult {
                ok: false,
                message: Some("缺少房间频道 ID".to_string()),
            });
        }
        let Some((accid, token)) = credentials else {
            return Ok(EnterResult {
                ok: false,
                message: Some("未登录或登录已过期".to_string()),
            });
        };

        let key = key_of(server_id, channel_id);

        // 已有会话：只登记订阅者；连接与订阅状态由客户端自己维护
        {
            let mut guard = self.lock();
            if let Some(session) = guard.get_mut(&accid) {
                let channel = session
                    .channels
                    .entry(key.clone())
                    .or_insert_with(|| ChannelState {
                        server_id: server_id.to_string(),
                        channel_id: channel_id.to_string(),
                        subscribers: Vec::new(),
                    });
                if !channel.subscribers.iter().any(|label| label == subscriber) {
                    channel.subscribers.push(subscriber.to_string());
                }
            }
        }

        let client = {
            let mut guard = self.lock();
            if let Some(session) = guard.get(&accid) {
                session.client.clone()
            } else {
                let (sdk_version, package_name) = nim_identity(profile);
                let (events_tx, mut events_rx) = mpsc::unbounded_channel();
                let client = std::sync::Arc::new(NimQChatClient::new(
                    QChatOptions {
                        app_key: IM_APP_KEY.to_string(),
                        account: accid.clone(),
                        token,
                        device_id: device_id.to_string(),
                        sdk_version,
                        package_name: package_name.to_string(),
                    },
                    http,
                    events_tx,
                ));
                let deliver = std::sync::Arc::new(deliver);
                let deliver_pump = deliver.clone();
                let client_pump = client.clone();

                // 事件转发：状态 + 实时消息（消息推给所有频道订阅者）
                tokio::spawn(async move {
                    while let Some(event) = events_rx.recv().await {
                        let batch = match event {
                            QChatEvent::Connected => PocketRoomBatch {
                                kind: "status",
                                status: Some("connected"),
                                message: None,
                                items: None,
                            },
                            QChatEvent::Reconnecting { attempt } => PocketRoomBatch {
                                kind: "status",
                                status: Some("reconnecting"),
                                message: Some(format!("第 {attempt} 次重连")),
                                items: None,
                            },
                            QChatEvent::Disconnected { message } => PocketRoomBatch {
                                kind: "status",
                                status: Some("disconnected"),
                                message: Some(message),
                                items: None,
                            },
                            QChatEvent::FatalAuthError { message } => PocketRoomBatch {
                                kind: "status",
                                status: Some("disconnected"),
                                message: Some(format!("{message}（登录已过期，请重新登录）")),
                                items: None,
                            },
                            QChatEvent::Error { message } => {
                                log_line(&format!("[pocket-room-qchat] 连接错误: {message}"));
                                continue;
                            }
                            QChatEvent::Message { message } => PocketRoomBatch {
                                kind: "message",
                                status: None,
                                message: None,
                                items: Some(vec![*message]),
                            },
                        };
                        deliver_pump(batch);
                    }
                });

                // 推送泵：等 QChat 会话就绪后接管 24/11
                client_pump.spawn_pump();

                guard.insert(
                    accid.clone(),
                    RoomSession {
                        client: client.clone(),
                        channels: HashMap::new(),
                    },
                );
                if let Some(session) = guard.get_mut(&accid) {
                    session.channels.insert(
                        key.clone(),
                        ChannelState {
                            server_id: server_id.to_string(),
                            channel_id: channel_id.to_string(),
                            subscribers: vec![subscriber.to_string()],
                        },
                    );
                }
                client
            }
        };

        if !client.is_connected() {
            let result = client.connect().await;
            if let Err(err) = result {
                self.rollback(&accid, &key, subscriber);
                return Ok(EnterResult {
                    ok: false,
                    message: Some(err),
                });
            }
        }
        client
            .subscribe_channel(server_id, channel_id)
            .await
            .map_err(|err| {
                log_line(&format!("[pocket-room-qchat] 订阅失败：{key} {err}"));
                err
            })?;
        Ok(EnterResult {
            ok: true,
            message: None,
        })
    }

    /// 失败回滚：摘掉本次登记；空频道会话连同连接一并销毁
    /// （连接成功但订阅失败时登录态已建立，留着就是一条顶着手机端下线的空转连接）。
    fn rollback(&self, accid: &str, key: &str, subscriber: &str) {
        let mut guard = self.lock();
        let Some(session) = guard.get_mut(accid) else {
            return;
        };
        if let Some(channel) = session.channels.get_mut(key) {
            channel.subscribers.retain(|label| label != subscriber);
            if channel.subscribers.is_empty() {
                session.channels.remove(key);
            }
        }
        if session.channels.is_empty() {
            session.client.destroy();
            guard.remove(accid);
        }
    }

    /// 离开频道：退订；无剩余订阅者时断开连接（切房先订新房再退旧房）。
    pub async fn leave(&self, server_id: &str, channel_id: &str, subscriber: &str) {
        let key = key_of(server_id, channel_id);
        let (client, accid) = {
            let mut guard = self.lock();
            let Some((accid, session)) = guard.iter_mut().next() else {
                return;
            };
            let Some(channel) = session.channels.get_mut(&key) else {
                return;
            };
            channel.subscribers.retain(|label| label != subscriber);
            if !channel.subscribers.is_empty() {
                return;
            }
            session.channels.remove(&key);
            let empty = session.channels.is_empty();
            let client = session.client.clone();
            let accid = accid.clone();
            if empty {
                client.destroy();
                guard.remove(&accid);
                log_line(&format!(
                    "[pocket-room-qchat] {accid} 无剩余订阅，连接已断开"
                ));
            }
            (client, accid)
        };
        let _ = accid;
        client.unsubscribe_channel(server_id, channel_id).await;
    }

    /// 窗口销毁兜底：该窗口的所有频道订阅一并退掉。
    pub fn leave_window(&self, subscriber: &str) {
        let rooms: Vec<(String, String)> = {
            let guard = self.lock();
            guard
                .values()
                .flat_map(|session| session.channels.values())
                .filter(|channel| channel.subscribers.iter().any(|label| label == subscriber))
                .map(|channel| (channel.server_id.clone(), channel.channel_id.clone()))
                .collect()
        };
        for (server_id, channel_id) in rooms {
            let key = key_of(&server_id, &channel_id);
            let mut guard = self.lock();
            for session in guard.values_mut() {
                if let Some(channel) = session.channels.get_mut(&key) {
                    channel.subscribers.retain(|label| label != subscriber);
                }
            }
        }
    }

    /// 登录态变更：断掉全部会话（旧连接用的是已作废 token）。
    pub fn reset(&self) {
        let drained: Vec<RoomSession> = {
            let mut guard = self.lock();
            guard.drain().map(|(_, session)| session).collect()
        };
        for session in drained {
            session.client.destroy();
        }
    }

    pub fn cleanup(&self) {
        self.reset();
    }

    /// 测试 / 探针：当前在册的频道键。
    pub fn active_channels(&self) -> Vec<String> {
        let guard = self.lock();
        let mut keys: Vec<String> = guard
            .values()
            .flat_map(|session| session.channels.keys().cloned())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enter_without_credentials_is_rejected() {
        let sessions = RoomQChatSessions::new();
        let result = sessions
            .enter(
                "10001",
                "20002",
                "main",
                None,
                "pocket48",
                "dev",
                reqwest::Client::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.message.unwrap().contains("未登录"));
        assert!(sessions.active_channels().is_empty());
    }

    #[tokio::test]
    async fn enter_requires_channel_ids() {
        let sessions = RoomQChatSessions::new();
        let result = sessions
            .enter(
                "",
                "20002",
                "main",
                Some(("a".to_string(), "t".to_string())),
                "pocket48",
                "dev",
                reqwest::Client::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.message.unwrap().contains("频道 ID"));
    }

    #[test]
    fn channel_key_is_stable() {
        assert_eq!(key_of("10001", "20002"), "10001:20002");
    }

    #[test]
    fn reset_is_safe_when_idle() {
        let sessions = RoomQChatSessions::new();
        sessions.reset();
        assert!(sessions.active_channels().is_empty());
    }
}
