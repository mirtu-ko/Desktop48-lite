//! 聊天室弹幕会话管理（对照 Electron `pocket-danmaku.ts`）。
//!
//! 同一聊天室多窗口共用一条连接：会话按 `chatroomId` 持有，窗口只是订阅者。
//! 渲染层按 `chatroomId` 过滤事件批次；凭据（accid + imToken）与 appKey 全部留在 Rust。
//!
//! 进房身份随登录档位（`nim_identity`）：会话建立时取一次并写入客户端，该连接此后不再变；
//! 档位切换必须经「登出 → 重新登录」，所以登录态变更时应调 `reset` 断开旧会话。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app_log::log_line;
use crate::nim::IM_APP_KEY;
use crate::nim_chatroom::{ChatroomEvent, ChatroomOptions, NimChatroomClient};

/// 云信身份：进房时以哪个客户端自居（登录属性 6 = SDK 版本，25 = 包名）。
///
/// 身份已收敛为口袋48 单档（见 `identity` 模块说明），形参保留只为不改调用点。
pub fn nim_identity(_profile: &str) -> (u32, &'static str) {
    (92012, "com.pocket.snh48.activity")
}

/// 事件批次（与 `ipc-contract.d.ts` 的 `PocketDanmakuBatch` 同形）。
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PocketDanmakuBatch {
    pub chatroom_id: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<crate::nim::ChatMessage>>,
}

struct Session {
    /// 订阅窗口 label；窗口销毁时由上层调 `stop`
    subscribers: Vec<String>,
    client: std::sync::Arc<NimChatroomClient>,
    /// 事件转发任务：最后一个订阅者离开时 abort
    pump: JoinHandle<()>,
}

#[derive(Default)]
pub struct DanmakuSessions {
    sessions: Mutex<HashMap<String, Session>>,
}

impl DanmakuSessions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 起会话（已存在则只登记订阅）。
    ///
    /// 返回 `Ok(false)` = 无凭据（未登录），渲染层据此报终态；
    /// `Err` = 参数无效，属于调用方的 bug。
    pub fn start<F>(
        &self,
        chatroom_id: &str,
        subscriber: &str,
        credentials: Option<(String, String)>,
        profile: &str,
        device_id: &str,
        deliver: F,
    ) -> Result<bool, String>
    where
        F: Fn(PocketDanmakuBatch) + Send + Sync + 'static,
    {
        let room_key = chatroom_id.trim().to_string();
        if room_key.is_empty()
            || room_key.len() > 32
            || !room_key.chars().all(|ch| ch.is_ascii_digit())
        {
            return Err(format!("无效的聊天室 ID: {room_key}"));
        }

        // 已有会话：只追加订阅者（两个窗口同时打开同一直播时不该建两条连接）
        {
            let mut guard = self.lock();
            if let Some(session) = guard.get_mut(&room_key) {
                if !session.subscribers.iter().any(|label| label == subscriber) {
                    session.subscribers.push(subscriber.to_string());
                }
                return Ok(true);
            }
        }

        let Some((account, token)) = credentials else {
            return Ok(false);
        };

        let (sdk_version, package_name) = nim_identity(profile);
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let client = std::sync::Arc::new(NimChatroomClient::new(
            ChatroomOptions {
                app_key: IM_APP_KEY.to_string(),
                account,
                token,
                room_id: room_key.clone(),
                device_id: device_id.to_string(),
                sdk_version,
                package_name: package_name.to_string(),
            },
            events_tx,
        ));

        let room_for_pump = room_key.clone();
        let deliver = std::sync::Arc::new(deliver);
        let deliver_in_pump = deliver.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                let batch = match event {
                    ChatroomEvent::Connected => PocketDanmakuBatch {
                        chatroom_id: room_for_pump.clone(),
                        kind: "status",
                        status: Some("connected"),
                        message: None,
                        items: None,
                    },
                    ChatroomEvent::Reconnecting { attempt } => PocketDanmakuBatch {
                        chatroom_id: room_for_pump.clone(),
                        kind: "status",
                        status: Some("reconnecting"),
                        message: Some(format!("第 {attempt} 次重连")),
                        items: None,
                    },
                    ChatroomEvent::Disconnected { message } => PocketDanmakuBatch {
                        chatroom_id: room_for_pump.clone(),
                        kind: "status",
                        status: Some("disconnected"),
                        message: Some(message),
                        items: None,
                    },
                    ChatroomEvent::Message { message } => PocketDanmakuBatch {
                        chatroom_id: room_for_pump.clone(),
                        kind: "messages",
                        status: None,
                        message: None,
                        items: Some(vec![message]),
                    },
                    // 内部异常只记日志：弹幕拿不到不能影响播放
                    ChatroomEvent::Error { message } => {
                        log_line(&format!("[pocket-danmaku] {room_for_pump} 异常: {message}"));
                        continue;
                    }
                };
                deliver_in_pump(batch);
            }
        });

        deliver(PocketDanmakuBatch {
            chatroom_id: room_key.clone(),
            kind: "status",
            status: Some("connecting"),
            message: None,
            items: None,
        });

        {
            let mut guard = self.lock();
            if let Some(existing) = guard.get_mut(&room_key) {
                // 竞争：本轮等待期间另一个窗口已经建好了会话
                if !existing.subscribers.iter().any(|label| label == subscriber) {
                    existing.subscribers.push(subscriber.to_string());
                }
                pump.abort();
                return Ok(true);
            }
            guard.insert(
                room_key.clone(),
                Session {
                    subscribers: vec![subscriber.to_string()],
                    client: client.clone(),
                    pump,
                },
            );
        }

        // 初连失败不自动重连（断线重连在客户端内部）；渲染层重开浮窗会再起新会话
        tokio::spawn(async move {
            if let Err(err) = client.connect().await {
                log_line(&format!(
                    "[pocket-danmaku] 聊天室 {room_key} 连接失败: {err}"
                ));
            }
        });
        Ok(true)
    }

    /// 退订某聊天室；最后一个订阅者离开时断开连接。
    pub fn stop(&self, chatroom_id: &str, subscriber: &str) {
        let room_key = chatroom_id.trim().to_string();
        let removed = {
            let mut guard = self.lock();
            let Some(session) = guard.get_mut(&room_key) else {
                return;
            };
            session.subscribers.retain(|label| label != subscriber);
            if !session.subscribers.is_empty() {
                return;
            }
            guard.remove(&room_key)
        };
        if let Some(session) = removed {
            session.client.destroy();
            session.pump.abort();
            log_line(&format!("[pocket-danmaku] 聊天室 {room_key} 会话已停止"));
        }
    }

    /// 窗口销毁兜底：该窗口的所有订阅一并退掉（播放窗关闭不触发渲染层退订）。
    pub fn stop_window(&self, subscriber: &str) {
        let rooms: Vec<String> = {
            let guard = self.lock();
            guard
                .iter()
                .filter(|(_, session)| session.subscribers.iter().any(|label| label == subscriber))
                .map(|(room, _)| room.clone())
                .collect()
        };
        for room in rooms {
            self.stop(&room, subscriber);
        }
    }

    /// 登录态变更（登出 / 切档）时断开全部会话：旧连接用的是旧档进房身份与已作废 token。
    pub fn reset(&self) {
        let drained: Vec<Session> = {
            let mut guard = self.lock();
            guard.drain().map(|(_, session)| session).collect()
        };
        for session in drained {
            session.client.destroy();
            session.pump.abort();
        }
    }

    /// 应用退出清理。窗口即将销毁，不再发任何事件。
    pub fn cleanup(&self) {
        self.reset();
    }

    pub fn active_rooms(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nim_identity_is_always_pocket48() {
        // 身份收敛后，任何入参（含旧的 seine48）都取口袋48 身份
        for profile in ["pocket48", "seine48", "bogus", ""] {
            assert_eq!(
                nim_identity(profile),
                (92012, "com.pocket.snh48.activity"),
                "{profile} 应使用口袋48 身份"
            );
        }
    }

    #[test]
    fn invalid_room_id_is_rejected() {
        let sessions = DanmakuSessions::new();
        let err = sessions
            .start("abc", "main", None, "pocket48", "dev", |_| {})
            .unwrap_err();
        assert!(err.contains("无效的聊天室 ID"));
        assert!(sessions.active_rooms().is_empty());
    }

    #[test]
    fn missing_credentials_reports_false_without_session() {
        let sessions = DanmakuSessions::new();
        let ok = sessions
            .start("12345", "main", None, "pocket48", "dev", |_| {})
            .unwrap();
        assert!(!ok, "未登录应返回 false 且不建会话");
        assert!(sessions.active_rooms().is_empty());
    }
}
