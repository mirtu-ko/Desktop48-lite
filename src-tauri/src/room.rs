//! 房间域只读 HTTP 通道（对照 Electron `pocket-auth.ts` 的房间部分）。
//!
//! 只做：频道解析 → 频道列表 → 发言权限 → 历史 / 精华 / 末条消息。不实现发送、删除、问答应答。
//!
//! 两条缓存：发言权限 30s，server 频道列表 5min。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::pocket::PocketAccount;

const ORIGIN: &str = "https://pocketapi.48.cn";
const SERVER_JUMP_URL: &str = "https://pocketapi.48.cn/im/api/v1/im/server/jump";
const ROOM_INFO_URL: &str = "https://pocketapi.48.cn/im/api/v1/im/team/room/info";
const ROOM_ANSWER_VERIFY_URL: &str =
    "https://pocketapi.48.cn/im/api/v1/team/channel/question/answer/verify";
const SEINE_SERVER_DETAIL_URL: &str = "https://pocketapi.48.cn/im/api/seine/server/detail";
const ROOM_MESSAGE_MEMBER_URL: &str =
    "https://pocketapi.48.cn/im/api/v1/team/message/list/homeowner";
const ROOM_MESSAGE_ALL_URL: &str = "https://pocketapi.48.cn/im/api/v1/team/message/list/all";
const ROOM_ESSENCE_MESSAGE_URL: &str =
    "https://pocketapi.48.cn/im/api/v1/team/essence/message/list";
const ROOM_LAST_MESSAGE_URL: &str =
    "https://pocketapi.48.cn/im/api/v1/team/classic/last/message/get";

/// 消息分页条数（与 Electron `ROOM_MESSAGE_LIMIT` 同值）
const ROOM_MESSAGE_LIMIT: i64 = 30;
/// 发言权限缓存
const ROOM_PERMISSION_TTL: Duration = Duration::from_secs(30);
/// server 频道列表缓存
const SERVER_CHANNEL_TTL: Duration = Duration::from_secs(5 * 60);

/// 账号接口地址越界校验用（与 `pocket.rs` 的 ORIGIN 一致）。
#[allow(dead_code)]
const _ORIGIN_CHECK: &str = ORIGIN;

/// 空频道占位多久后重试解析（成员开通 / 关闭房间不频繁，30 分钟足够；
/// 与 Electron `EMPTY_CHANNEL_TTL` 同值）
const EMPTY_CHANNEL_TTL: Duration = Duration::from_secs(30 * 60);
// 关注人数通常在个位数，逐个解析已足够快；不再手写并发池
// （Electron 用 RESOLVE_CONCURRENCY = 4 限制并发，这里顺序执行同样不会打爆出口限流）

#[derive(Clone)]
struct ChannelEntry {
    server_id: String,
    channel_id: String,
    resolved_at: Instant,
}

/// 频道缓存的落盘形态：`server/jump` 的结果几乎不变，冷启动不该重跑一遍。
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StoredChannels {
    #[serde(default)]
    version: u32,
    /// userId → { serverId, channelId, resolvedAt（unix ms） }
    #[serde(default)]
    channels: HashMap<String, StoredChannelEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredChannelEntry {
    server_id: String,
    channel_id: String,
    resolved_at: i64,
}

const CHANNEL_CACHE_VERSION: u32 = 1;
const CHANNEL_CACHE_FILE: &str = "pocket48-room-channels.json";
/// 落盘条目的最长信任期：超过就重解析（房间可能换过频道）
const STORED_CHANNEL_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub struct RoomCache {
    permissions: Mutex<HashMap<String, (Instant, Value)>>,
    server_channels: Mutex<HashMap<String, (Instant, Value)>>,
    /// `userId → 房间频道` 缓存（批量解析用）；空频道按 TTL 重试
    channels: Mutex<HashMap<i64, ChannelEntry>>,
    /// 落盘路径；`None` = 不持久化（单测 / 探针隔离目录）
    path: Option<PathBuf>,
}

impl Default for RoomCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomCache {
    pub fn new() -> Self {
        Self {
            permissions: Mutex::new(HashMap::new()),
            server_channels: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
            path: None,
        }
    }

    /// 带落盘：启动时把上次的频道映射读回内存，之后每次写入都原子落盘。
    ///
    /// 只缓存**有频道**的条目；空占位（没开过播的成员）不落盘，
    /// 那种信息随时会变，存下来只会让下次启动多猜一次。
    pub fn load(dir: &Path) -> Self {
        let cache = Self {
            path: Some(dir.join(CHANNEL_CACHE_FILE)),
            ..Self::new()
        };
        cache.restore();
        cache
    }

    fn restore(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(stored) = serde_json::from_str::<StoredChannels>(&text) else {
            // 结构升级 / 文件损坏：当作没有缓存，下次写入会覆盖
            return;
        };
        if stored.version != CHANNEL_CACHE_VERSION {
            return;
        }
        let now_ms = now_millis();
        let mut restored = 0usize;
        if let Ok(mut guard) = self.channels.lock() {
            for (user_id, entry) in stored.channels {
                let Ok(user_id) = user_id.parse::<i64>() else {
                    continue;
                };
                if entry.channel_id.is_empty() {
                    continue;
                }
                let age_ms = now_ms.saturating_sub(entry.resolved_at);
                if age_ms < 0 || age_ms as u64 > STORED_CHANNEL_TTL.as_millis() as u64 {
                    continue;
                }
                // `resolved_at` 记的是「解析时刻」。把落盘的年龄折回去，
                // 让它在 `persist` 里算出的年龄与真实一致，寿命不因重启而重置。
                let restored_at = Instant::now()
                    .checked_sub(Duration::from_millis(age_ms as u64))
                    .unwrap_or_else(Instant::now);
                guard.insert(
                    user_id,
                    ChannelEntry {
                        server_id: entry.server_id,
                        channel_id: entry.channel_id,
                        resolved_at: restored_at,
                    },
                );
                restored += 1;
            }
        }
        if restored > 0 {
            crate::app_log::log_line(&format!(
                "[room] 频道缓存已恢复 {restored} 条（冷启动不再重解析）"
            ));
        }
    }

    /// 把当前有频道的条目原子写盘（tmp + rename）。失败只记日志，不影响功能。
    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let stored = {
            let Ok(guard) = self.channels.lock() else {
                return;
            };
            let now = Instant::now();
            let mut channels = HashMap::with_capacity(guard.len());
            for (user_id, entry) in guard.iter() {
                // 空占位不落盘；过期的也不落
                if entry.channel_id.is_empty() {
                    continue;
                }
                let age = now.saturating_duration_since(entry.resolved_at);
                if age > STORED_CHANNEL_TTL {
                    continue;
                }
                channels.insert(
                    user_id.to_string(),
                    StoredChannelEntry {
                        server_id: entry.server_id.clone(),
                        channel_id: entry.channel_id.clone(),
                        resolved_at: now_millis().saturating_sub(age.as_millis() as i64),
                    },
                );
            }
            StoredChannels {
                version: CHANNEL_CACHE_VERSION,
                channels,
            }
        };
        let Ok(text) = serde_json::to_string(&stored) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            crate::app_log::log_line(&format!("[room] 频道缓存落盘失败: {err}"));
        }
    }

    fn cached_channel(&self, user_id: i64) -> Option<ChannelEntry> {
        let guard = self.channels.lock().ok()?;
        let entry = guard.get(&user_id)?;
        // 有频道的条目长期有效；空占位过期后重试解析
        if !entry.channel_id.is_empty() || entry.resolved_at.elapsed() < EMPTY_CHANNEL_TTL {
            Some(entry.clone())
        } else {
            None
        }
    }

    fn store_channel(&self, user_id: i64, server_id: String, channel_id: String) {
        if let Ok(mut guard) = self.channels.lock() {
            guard.insert(
                user_id,
                ChannelEntry {
                    server_id,
                    channel_id,
                    resolved_at: Instant::now(),
                },
            );
        }
        self.persist();
    }

    fn cached(
        map: &Mutex<HashMap<String, (Instant, Value)>>,
        key: &str,
        ttl: Duration,
    ) -> Option<Value> {
        let guard = map.lock().ok()?;
        let (at, value) = guard.get(key)?;
        (at.elapsed() < ttl).then(|| value.clone())
    }

    fn store(map: &Mutex<HashMap<String, (Instant, Value)>>, key: String, value: Value) {
        if let Ok(mut guard) = map.lock() {
            guard.insert(key, (Instant::now(), value));
        }
    }

    fn store_permission(&self, key: String, value: Value) {
        Self::store(&self.permissions, key, value);
    }

    fn store_server_channels(&self, key: String, value: Value) {
        Self::store(&self.server_channels, key, value);
    }
}

/// 房间频道（`{ serverId, channelId }`）。
pub async fn room_channel(pocket: &PocketAccount, user_id: i64) -> Result<Value, String> {
    let Some(envelope) = pocket
        .post_room(
            SERVER_JUMP_URL,
            &json!({ "starId": user_id, "targetType": 1 }),
        )
        .await?
    else {
        return Ok(json!({ "serverId": "", "channelId": "" }));
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取房间频道失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    Ok(json!({
        "serverId": loose_text(content.get("serverId")),
        "channelId": loose_text(content.get("channelId")),
    }))
}

/// 当前毫秒时刻（落盘时间戳用）。
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 频道解析的并发度：与 `net.rs` 的单域名在途上限一致，再高只会排队。
const CHANNEL_FETCH_CONCURRENCY: usize = 5;

/// 批量解析关注成员的房间频道（`pocketGetRoomChannels`）。
///
/// 只解析未缓存或空占位过期的成员；单个失败记空占位按 TTL 重试，
/// 只有整批全失败且一个有效频道都没有才报错，避免一轮网络抖动清掉已缓存结果。
pub async fn room_channels_batch(
    cache: &RoomCache,
    pocket: &PocketAccount,
    user_ids: &[i64],
) -> Result<Value, String> {
    let mut ids: Vec<i64> = user_ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let missing: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| cache.cached_channel(*id).is_none())
        .collect();
    ids.sort_unstable();

    if !missing.is_empty() {
        // 并发解析，但**不无限并发**：`net.rs` 对单域名限制 5 req/s、在途 4，
        // 一次放几十个只会全挤在令牌桶前排队，还容易撞 429。并发度取在途上限。
        let results: Vec<(i64, Result<Value, String>)> = {
            use futures_util::StreamExt;
            futures_util::stream::iter(missing.iter().copied())
                .map(|user_id| async move { (user_id, room_channel(pocket, user_id).await) })
                .buffer_unordered(CHANNEL_FETCH_CONCURRENCY)
                .collect()
                .await
        };

        let mut failed = 0usize;
        let mut first_error: Option<String> = None;
        for (user_id, result) in results {
            match result {
                Ok(value) => {
                    let server_id = loose_text(value.get("serverId"));
                    let channel_id = loose_text(value.get("channelId"));
                    cache.store_channel(user_id, server_id, channel_id);
                }
                Err(err) => {
                    // 登录失效直接上抛，让上层整轮放弃
                    if err.contains("登录已过期") {
                        return Err(err);
                    }
                    cache.store_channel(user_id, String::new(), String::new());
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        let has_valid = ids.iter().any(|id| {
            cache
                .cached_channel(*id)
                .map(|entry| !entry.channel_id.is_empty())
                .unwrap_or(false)
        });
        if failed == missing.len() && !has_valid {
            return Err(first_error.unwrap_or_else(|| "房间频道解析失败".to_string()));
        }
    }

    let items: Vec<Value> = ids
        .iter()
        .map(|id| {
            let entry = cache.cached_channel(*id);
            json!({
                "userId": id,
                "channel": {
                    "serverId": entry.as_ref().map(|entry| entry.server_id.clone()).unwrap_or_default(),
                    "channelId": entry.as_ref().map(|entry| entry.channel_id.clone()).unwrap_or_default(),
                },
            })
        })
        .collect();
    Ok(Value::Array(items))
}

/// 频道归属反查（`room/info` → `channelInfo.ownerId`）。
///
/// 小房间发现靠它区分「普通频道」与「暗号小房间」：拿到正常回包 = 普通频道；
/// 回包 message 里带 `questionId` 信封 = 被暗号拦下（见 `parse_question_envelope`）。
pub async fn room_channel_owner(pocket: &PocketAccount, channel_id: &str) -> Result<Value, String> {
    let Some(envelope) = pocket
        .post_room(ROOM_INFO_URL, &json!({ "channelId": channel_id }))
        .await?
    else {
        return Ok(json!({ "channelId": channel_id, "serverId": "", "ownerId": 0 }));
    };
    if envelope.success == Some(false) {
        // 业务失败原样上抛：调用方靠 message 判断是「暗号门禁」还是「频道不存在」
        return Err(envelope.message_or("获取房间归属失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    let channel_info = content
        .get("channelInfo")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let resolved_channel = loose_text(channel_info.get("channelId"));
    Ok(json!({
        "channelId": if resolved_channel.is_empty() { channel_id.to_string() } else { resolved_channel },
        "serverId": loose_text(channel_info.get("serverId")),
        "ownerId": number_of(channel_info.get("ownerId")) as i64,
    }))
}

/// 小房间的暗号问题（`room/info` 被暗号拦下时，回包 message 内嵌 JSON 信封）。
///
/// 正常回包 = 无门禁（或已通过），返回 `null`；被拦下返回问题；其余业务错误照常上抛。
pub async fn room_question(pocket: &PocketAccount, channel_id: &str) -> Result<Value, String> {
    if channel_id.is_empty() {
        return Ok(Value::Null);
    }
    let Some(envelope) = pocket
        .post_room(ROOM_INFO_URL, &json!({ "channelId": channel_id }))
        .await?
    else {
        return Ok(Value::Null);
    };
    if envelope.success != Some(false) {
        return Ok(Value::Null);
    }
    let message = envelope.message.unwrap_or_default();
    match parse_question_envelope(&message) {
        Some(question) => Ok(question),
        None => Err(if message.is_empty() {
            "获取暗号问题失败".to_string()
        } else {
            message
        }),
    }
}

/// 解析 `room/info` 的拦截信封：message 是内嵌 JSON（如 `{"questionId":"596","question":"..."}`）。
pub fn parse_question_envelope(message: &str) -> Option<Value> {
    let trimmed = message.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let question_id = parsed
        .get("questionId")
        .map(|value| loose_text(Some(value)))?;
    if question_id.is_empty() {
        return None;
    }
    Some(json!({
        "questionId": question_id,
        "question": parsed.get("question").map(|value| loose_text(Some(value))).unwrap_or_default(),
    }))
}

/// 万能暗号：客户端直通，不发起任何请求。
///
/// 自用通道——真实暗号由服务端校验，但并非每个房间的暗号都已知。
/// **界面不公开它的存在**，只在渲染层的输入框里当普通口令处理。
pub const MASTER_PASSCODE: &str = "18951002";

/// 暗号提交结果（结构对齐 `ipc-contract.d.ts` 的 `PocketRoomAnswerResult`）。
pub fn answer_result_ok(via: &str) -> Value {
    json!({ "ok": true, "via": via })
}

pub fn answer_result_fail(reason: &str, message: &str) -> Value {
    json!({ "ok": false, "reason": reason, "message": message })
}

/// 提交小房间暗号：服务端校验通过才算解锁。
///
/// 端点 `im/api/v1/team/channel/question/answer/verify`，body `{ answer, questionId }`；
/// `questionId` 服务端声明为 long，纯数字串转数字。接口不可达时返回 `unverified`，
/// 不冒充「回答错误」。
pub async fn check_room_answer(
    pocket: &PocketAccount,
    question_id: &str,
    answer: &str,
) -> Result<Value, String> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Ok(answer_result_fail("wrong", "请输入暗号"));
    }
    // 万能密码：客户端直通，不打接口
    if trimmed == MASTER_PASSCODE {
        return Ok(answer_result_ok("master"));
    }
    if question_id.trim().is_empty() {
        return Ok(answer_result_fail("wrong", "未取到暗号问题，请稍后重试"));
    }
    let Some(envelope) = pocket
        .post_room(
            ROOM_ANSWER_VERIFY_URL,
            &json!({
                "answer": trimmed,
                "questionId": api_id(question_id),
            }),
        )
        .await?
    else {
        return Ok(answer_result_fail("wrong", "未登录口袋48账号"));
    };
    if envelope.success == Some(true) {
        return Ok(answer_result_ok("server"));
    }
    if envelope.success == Some(false) {
        return Ok(answer_result_fail(
            "wrong",
            &envelope.message_or("回答错误"),
        ));
    }
    // success 字段缺失：按未通过处理，但不冒充「回答错误」
    Ok(answer_result_fail(
        "unverified",
        "暗号校验接口不可用，请稍后重试",
    ))
}

/// 房间发言权限（`room/info` 的 `userFunction`）。
///
/// 判据只取 `sendText`：账号等级、身份组、房间设置、个人禁言都已被服务端合并进这个布尔值。
/// `hasMsgBoard` 是「房间是否已关闭」的唯一判据（关闭后 jump 会退到该 server 的直播频道）。
pub async fn room_permissions(
    cache: &RoomCache,
    pocket: &PocketAccount,
    channel_id: &str,
) -> Result<Value, String> {
    if channel_id.is_empty() {
        return Ok(Value::Null);
    }
    if let Some(cached) = RoomCache::cached(&cache.permissions, channel_id, ROOM_PERMISSION_TTL) {
        return Ok(cached);
    }
    let Some(envelope) = pocket
        .post_room(ROOM_INFO_URL, &json!({ "channelId": channel_id }))
        .await?
    else {
        return Ok(Value::Null);
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取房间发言权限失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    let user_function = content
        .get("userFunction")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let channel_info = content
        .get("channelInfo")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let room_role = {
        let role = loose_text(content.get("roomRole"));
        if role.is_empty() {
            "0".to_string()
        } else {
            role
        }
    };
    let value = json!({
        // 字段缺失（协议变动）时按可发言处理：宁可让服务端拒绝，也不要误判成禁言把用户锁在外面
        "sendText": user_function.get("sendText").and_then(Value::as_bool).unwrap_or(true),
        "cdTime": number_of(user_function.get("cdTime")),
        "roomRole": room_role,
        "followFlag": content.get("followFlag").and_then(Value::as_bool).unwrap_or(false),
        "hasMsgBoard": channel_info.get("hasMsgBoard").and_then(Value::as_f64).map(|n| n > 0.0),
    });
    cache.store_permission(channel_id.to_string(), value.clone());
    Ok(value)
}

/// server 下的频道列表（channelId + 频道显示名）。
pub async fn server_channels(
    cache: &RoomCache,
    pocket: &PocketAccount,
    server_id: &str,
    force: bool,
) -> Result<Value, String> {
    if server_id.is_empty() {
        return Ok(json!([]));
    }
    if !force
        && let Some(cached) =
            RoomCache::cached(&cache.server_channels, server_id, SERVER_CHANNEL_TTL)
    {
        return Ok(cached);
    }
    let Some(envelope) = pocket
        .post_room(
            SEINE_SERVER_DETAIL_URL,
            &json!({ "serverId": api_id(server_id) }),
        )
        .await?
    else {
        return Ok(json!([]));
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取频道列表失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    let list = content
        .get("channelInfoList")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let items: Vec<Value> = list
        .iter()
        .filter_map(|item| {
            let channel_id = loose_text(item.get("channelId"));
            if channel_id.is_empty() {
                return None;
            }
            Some(json!({
                "channelId": channel_id,
                "channelName": loose_text(item.get("channelName")),
            }))
        })
        .collect();
    let value = Value::Array(items);
    cache.store_server_channels(server_id.to_string(), value.clone());
    Ok(value)
}

/// 房间消息（`scope` = member（仅房主）/ all（全员））。
pub async fn room_messages(
    pocket: &PocketAccount,
    channel_id: &str,
    server_id: &str,
    next_time: f64,
    scope: &str,
) -> Result<Value, String> {
    let url = if scope == "all" {
        ROOM_MESSAGE_ALL_URL
    } else {
        ROOM_MESSAGE_MEMBER_URL
    };
    let Some(envelope) = pocket
        .post_room(
            url,
            &json!({
                "channelId": api_id(channel_id),
                "serverId": api_id(server_id),
                "nextTime": next_time,
                "limit": ROOM_MESSAGE_LIMIT,
            }),
        )
        .await?
    else {
        return Err("未登录口袋48账号".to_string());
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取房间消息失败"));
    }
    Ok(room_messages_payload(
        envelope.content.unwrap_or_else(|| json!({})),
    ))
}

/// 房间精华消息（房主 / 管理员挑选的房间级内容）。
pub async fn room_essence_messages(
    pocket: &PocketAccount,
    channel_id: &str,
    server_id: &str,
    next_time: f64,
) -> Result<Value, String> {
    let Some(envelope) = pocket
        .post_room(
            ROOM_ESSENCE_MESSAGE_URL,
            &json!({
                "channelId": api_id(channel_id),
                "serverId": api_id(server_id),
                "nextTime": next_time,
                "limit": ROOM_MESSAGE_LIMIT,
            }),
        )
        .await?
    else {
        return Err("未登录口袋48账号".to_string());
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取精华消息失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    let list = content
        .get("essenceMessageList")
        .or_else(|| content.get("message"))
        .or_else(|| content.get("messageList"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(json!({
        "messages": list,
        "nextTime": number_of(content.get("nextTime")),
    }))
}

/// 批量取各 server 下频道的最后一条消息（左侧列表预览）。
pub async fn room_last_messages(
    pocket: &PocketAccount,
    server_ids: &[String],
) -> Result<Value, String> {
    let ids: Vec<i64> = server_ids
        .iter()
        .filter_map(|id| id.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect();
    if ids.is_empty() {
        return Ok(json!([]));
    }
    let Some(envelope) = pocket
        // 参数名是 `serverIdList`（实测；写成 serverIds 服务端回「查询消息参数为空」）
        .post_room(ROOM_LAST_MESSAGE_URL, &json!({ "serverIdList": ids }))
        .await?
    else {
        return Err("未登录口袋48账号".to_string());
    };
    if envelope.success == Some(false) {
        return Err(envelope.message_or("获取房间预览失败"));
    }
    let content = envelope.content.unwrap_or_else(|| json!({}));
    let list = content
        .get("lastMsgList")
        .or_else(|| content.get("messageList"))
        .or_else(|| content.get("list"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(normalize_last_messages(&list))
}

fn room_messages_payload(content: Value) -> Value {
    let list = content
        .get("message")
        .or_else(|| content.get("messageList"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    json!({
        "messages": list,
        "nextTime": number_of(content.get("nextTime")),
    })
}

/// 末条消息归一：字段实测只回 4 个（msgidClient / msgTime 都拿不到），
/// 消息身份键实际恒退到 `0:正文`——未读检测只用它判断「是否来了新消息」。
fn normalize_last_messages(list: &Value) -> Value {
    let items: Vec<Value> = list
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|item| {
            let channel_id = loose_text(item.get("channelId"));
            if channel_id.is_empty() {
                return None;
            }
            // msgContent 也可能是对象：与 Electron 一样整体 JSON 化，保持身份键可比
            let msg_content = match item.get("msgContent") {
                Some(Value::String(text)) => text.clone(),
                Some(value) if !value.is_null() => value.to_string(),
                _ => String::new(),
            };
            let msg_time = number_of(item.get("msgTime"));
            // 身份键候选顺序与 Electron `toMessageId` 链一致
            let msg_id = [
                "msgidClient",
                "msgIdClient",
                "msgIdServer",
                "msgId",
                "messageId",
            ]
            .iter()
            .map(|key| loose_text(item.get(*key)))
            .find(|text| !text.is_empty())
            .unwrap_or_default();
            let msg_key = if msg_id.is_empty() {
                format!("{}:{msg_content}", msg_time as i64)
            } else {
                msg_id
            };
            Some(json!({
                "channelId": channel_id,
                "serverId": loose_text(item.get("serverId")),
                "starName": loose_text(item.get("starName")),
                "msgContent": msg_content,
                "msgKey": msg_key,
                "msgTime": msg_time,
                "unreadCount": item.get("unreadCount").and_then(Value::as_f64),
            }))
        })
        .collect();
    Value::Array(items)
}

/// `String(x)` 同口径：数字直出、字符串 trim、其余空。
pub fn loose_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn number_of(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 服务端声明的 ID 是 long：纯数字串转数字，其余原样透传（宁可让服务端报参数错误）。
fn api_id(raw: &str) -> Value {
    let trimmed = raw.trim();
    match trimmed.parse::<i64>() {
        Ok(number) => json!(number),
        Err(_) => json!(trimmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "d48-room-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn channel_cache_survives_restart() {
        let dir = tmp_dir("cache");
        let cache = RoomCache::load(&dir);
        cache.store_channel(101, "s1".into(), "c1".into());
        cache.store_channel(202, "s2".into(), "c2".into());
        // 空占位（没开过播的成员）不落盘
        cache.store_channel(303, String::new(), String::new());

        let reloaded = RoomCache::load(&dir);
        assert_eq!(reloaded.cached_channel(101).unwrap().channel_id, "c1");
        assert_eq!(reloaded.cached_channel(202).unwrap().server_id, "s2");
        // 空通道不恢复，下次仍会重新解析
        assert!(reloaded.cached_channel(303).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_cache_ignores_corrupt_or_stale_file() {
        let dir = tmp_dir("corrupt");
        let path = dir.join(CHANNEL_CACHE_FILE);

        // 损坏的 JSON：当作没有缓存，不 panic
        std::fs::write(&path, "{not json").unwrap();
        assert!(RoomCache::load(&dir).cached_channel(101).is_none());

        // 版本不符：直接丢弃
        std::fs::write(
            &path,
            json!({
                "version": CHANNEL_CACHE_VERSION + 1,
                "channels": { "101": { "serverId": "s", "channelId": "c", "resolvedAt": now_millis() } }
            })
            .to_string(),
        )
        .unwrap();
        assert!(RoomCache::load(&dir).cached_channel(101).is_none());

        // 超过 TTL：丢弃（房间可能换过频道）
        let stale = now_millis() - (STORED_CHANNEL_TTL.as_millis() as i64) - 1000;
        std::fs::write(
            &path,
            json!({
                "version": CHANNEL_CACHE_VERSION,
                "channels": { "101": { "serverId": "s", "channelId": "c", "resolvedAt": stale } }
            })
            .to_string(),
        )
        .unwrap();
        assert!(RoomCache::load(&dir).cached_channel(101).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_id_converts_numeric_strings_only() {
        assert_eq!(api_id("16285946319"), json!(16285946319i64));
        assert_eq!(api_id(" 42 "), json!(42));
        // 非数字原样透传，不在本地编造
        assert_eq!(api_id("abc"), json!("abc"));
    }

    #[test]
    fn loose_text_and_number_follow_js() {
        assert_eq!(loose_text(Some(&json!("  x  "))), "x");
        assert_eq!(loose_text(Some(&json!(12))), "12");
        assert_eq!(loose_text(None), "");
        assert_eq!(number_of(Some(&json!("1795349506379"))), 1795349506379.0);
        assert_eq!(number_of(Some(&json!(0))), 0.0);
        assert_eq!(number_of(None), 0.0);
    }

    #[test]
    fn question_envelope_is_parsed_from_message() {
        // 被暗号拦下时 message 是内嵌 JSON 信封
        let envelope = r#"{"questionId":"596","question":"今天吃什么？"}"#;
        let parsed = parse_question_envelope(envelope).unwrap();
        assert_eq!(parsed["questionId"], "596");
        assert_eq!(parsed["question"], "今天吃什么？");
        // 普通业务错误不是信封
        assert!(parse_question_envelope("频道不存在").is_none());
        assert!(parse_question_envelope("").is_none());
        // 缺 questionId 也不算
        assert!(parse_question_envelope(r#"{"question":"x"}"#).is_none());
        // 数字型 questionId 也要接受（服务端声明为 long）
        let numeric = parse_question_envelope(r#"{"questionId":596,"question":"?"}"#).unwrap();
        assert_eq!(numeric["questionId"], "596");
    }

    #[test]
    fn last_messages_fall_back_to_content_key() {
        let list = json!([
            { "channelId": "20002", "serverId": "10001", "starName": "成员", "msgContent": "你好" },
            { "channelId": "", "msgContent": "无频道应丢弃" },
        ]);
        let normalized = normalize_last_messages(&list);
        let items = normalized.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["channelId"], "20002");
        // 拿不到 msgidClient 时键退到 0:正文（与 Electron 同口径）
        assert_eq!(items[0]["msgKey"], "0:你好");
        assert!(items[0]["unreadCount"].is_null());
    }

    #[test]
    fn room_messages_payload_supports_both_shapes() {
        let a = room_messages_payload(json!({ "message": [1], "nextTime": "123" }));
        assert_eq!(a["nextTime"], 123.0);
        let b = room_messages_payload(json!({ "messageList": [2] }));
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
        assert_eq!(b["nextTime"], 0.0);
    }
}
