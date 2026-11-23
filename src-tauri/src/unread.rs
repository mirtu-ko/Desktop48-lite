//! 房间未读检测（对照 Electron `pocket-unread.ts`）。状态归 Rust，渲染层只做镜像。
//!
//! 每 60s 一轮：先批量预览各频道最新一条（`msgKey` 变化即触发），再对触发的频道拉一页消息
//! 数出「比已读水位更新」的条数。水位只在首见锚定与 `markRead` 时推进，检测轮绝不推进。
//!
//! 持久化 `pocket48-unread.json`（原子写 + 坏档回退）：频道解析结果与水位一并落盘。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app_log::log_line;
use crate::pocket::PocketAccount;
use crate::room::{loose_text, room_channel, room_last_messages, room_messages};

/// 轮询间隔：低频检测（每轮一次批量预览），又不至于让角标迟滞超过一分钟。
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// 每轮最多拉取的房间消息列表数：一页几十 KB，关注多时按轮转摊平。
const MAX_MESSAGE_PROBES: usize = 9;
/// 有未读却超过这么久没探测过的频道强制补探一次（频道停更后触发锚会冻住）。
const STALE_UNREAD_PROBE_MS: i64 = 30 * 60 * 1000;
/// 有效频道的归属复核间隔：换房后零未读的成员只能靠它自愈
const CHANNEL_RECHECK_TTL_MS: i64 = 60 * 60 * 1000;
/// 每轮最多复核的频道数（单轮接口预算有界）
const CHANNEL_RECHECK_PER_TICK: usize = 2;
/// 频道连续探测失败达这个次数即作废重解析（成员换房 / 关房后旧频道会一直失败）
const CHANNEL_FAIL_LIMIT: u32 = 3;
/// 小房间主动发现的间隔：同一成员 server 两次「拉频道列表 + 试暗号」之间的最小间隔
const SMALL_DISCOVER_TTL_MS: i64 = 5 * 60 * 1000;
/// 单频道未读上限：列表接口一页封顶，渲染层据此显示 50+。
const ROOM_MESSAGE_LIMIT: i64 = 50;
/// 官方系统账号：不计入未读（与 Electron `OFFICIAL_POCKET_USER_ID` 同值）。
const OFFICIAL_POCKET_USER_ID: &str = "121569667";
const UNREAD_FILE: &str = "pocket48-unread.json";

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnreadRoomState {
    pub count: i64,
    pub big_count: i64,
    pub small_count: i64,
    pub latest_time: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PocketUnreadRoomState {
    pub count: i64,
    pub big_count: i64,
    pub small_count: i64,
    pub latest_time: i64,
}

/// 全量快照（与 `ipc-contract.d.ts` 的 `PocketUnreadBatch` 同形）。
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct PocketUnreadBatch {
    /// userId → 未读快照（只有 count > 0 的成员才出现）
    pub rooms: BTreeMap<String, UnreadRoomState>,
    pub closed_channels: Vec<Value>,
    /// 各频道最新一条消息（房间页左列预览）
    pub previews: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct StoredChannel {
    server_id: String,
    channel_id: String,
    resolved_at: i64,
    /// 连续探测失败次数（成功即清零）；达上限作废重解析
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fail_count: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ReadMark {
    msg_time: i64,
    msg_id_server: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ChannelUnread {
    count: i64,
    latest_time: i64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StoredUnread {
    #[serde(default)]
    channels: BTreeMap<String, StoredChannel>,
    #[serde(default)]
    read_marks: BTreeMap<String, ReadMark>,
    /// 触发锚：channelId → 上一轮预览的身份键
    #[serde(default)]
    seen_keys: BTreeMap<String, String>,
    #[serde(default)]
    unread_channels: BTreeMap<String, ChannelUnread>,
    #[serde(default)]
    last_probe_at: BTreeMap<String, i64>,
    /// 上一轮预览回包（左列预览用；不参与未读判定）
    #[serde(default)]
    previews: Vec<Value>,
    /// 小房间（暗号频道）归属：userId → 频道
    #[serde(default)]
    small_channels: BTreeMap<String, StoredChannel>,
    /// 频道归属反查缓存：channelId → { ownerId, resolvedAt }
    /// 正向归属终身复用；`ownerId = 0` 是负缓存（非成员房间），按 TTL 重试
    #[serde(default)]
    room_owners: BTreeMap<String, Value>,
    /// 房间关闭中的历史频道：channelId → { serverId, rank, resolvedAt }
    #[serde(default)]
    closed_channel_map: BTreeMap<String, Value>,
}

/// 未读检测状态。
pub struct UnreadTracker {
    path: PathBuf,
    state: Mutex<StoredUnread>,
    /// 本进程内的探测时刻（不落盘：重启后补探一次反而更稳）
    probe_at: Mutex<HashMap<String, Instant>>,
    /// `serverId → 上次小房间枚举时刻`（不落盘：重启后重发现一次更稳）
    small_discover_at: Mutex<HashMap<String, Instant>>,
    ticking: Mutex<bool>,
}

impl UnreadTracker {
    /// 从磁盘加载；坏档按空状态处理（与 Electron `SafeJSONFileSync` 的回退同口径）。
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(UNREAD_FILE);
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<StoredUnread>(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            state: Mutex::new(state),
            probe_at: Mutex::new(HashMap::new()),
            small_discover_at: Mutex::new(HashMap::new()),
            ticking: Mutex::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoredUnread> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 快照：只有 count > 0 的成员出现。
    pub fn snapshot(&self) -> PocketUnreadBatch {
        let guard = self.lock();
        PocketUnreadBatch {
            rooms: aggregate(&guard),
            closed_channels: closed_channel_list(&guard),
            previews: guard.previews.clone(),
        }
    }

    /// 频道归属自愈：把 `lastMsgList` 里出现、但归属表里没有的频道补上。
    ///
    /// 正常回包补齐大房间；带 `questionId` 信封的归为小房间；报「关闭」记进关闭集；
    /// 报「频道不存在」进负缓存（按 TTL 重试）。已关闭频道到期也要复核——
    /// 房间重开后旧频道会从 `lastMsgList` 消失，只等未知列表的话「已关闭」标记会永久残留。
    pub async fn heal_channel_owners(
        &self,
        pocket: &PocketAccount,
        last_messages: &[Value],
        followed_ids: &[i64],
    ) -> Result<bool, String> {
        const EMPTY_TTL: i64 = 30 * 60 * 1000;
        let now = now_millis();

        // 归属表：已登记的大房间与小房间频道
        let attributed: HashSet<String> = {
            let guard = self.lock();
            guard
                .channels
                .values()
                .chain(guard.small_channels.values())
                .filter(|channel| !channel.channel_id.is_empty())
                .map(|channel| channel.channel_id.clone())
                .collect()
        };

        // 候选：未知频道 + 到期的关闭记录
        let mut to_resolve: Vec<String> = Vec::new();
        {
            let guard = self.lock();
            for message in last_messages {
                let channel_id = loose_text(message.get("channelId"));
                if channel_id.is_empty() || attributed.contains(&channel_id) {
                    continue;
                }
                if to_resolve.contains(&channel_id) {
                    continue;
                }
                let closed_recently = guard
                    .closed_channel_map
                    .get(&channel_id)
                    .and_then(|info| info.get("resolvedAt").and_then(Value::as_i64))
                    .map(|at| now - at <= EMPTY_TTL)
                    .unwrap_or(false);
                if closed_recently {
                    continue;
                }
                let cached = guard.room_owners.get(&channel_id);
                match cached {
                    None => to_resolve.push(channel_id),
                    Some(info) => {
                        // 正向归属终身复用；负缓存按 TTL 重试
                        let owner = info.get("ownerId").and_then(Value::as_i64).unwrap_or(0);
                        if owner == 0 {
                            let at = info.get("resolvedAt").and_then(Value::as_i64).unwrap_or(0);
                            if now - at > EMPTY_TTL {
                                to_resolve.push(channel_id);
                            }
                        }
                    }
                }
            }
            // 到期的关闭记录（房间可能已重开）
            for channel_id in guard.closed_channel_map.keys() {
                if to_resolve.contains(channel_id) {
                    continue;
                }
                let at = guard
                    .closed_channel_map
                    .get(channel_id)
                    .and_then(|info| info.get("resolvedAt").and_then(Value::as_i64))
                    .unwrap_or(0);
                if now - at > EMPTY_TTL {
                    to_resolve.push(channel_id.clone());
                }
            }
        }

        if to_resolve.is_empty() {
            return Ok(false);
        }
        log_line(&format!(
            "[pocket-unread] 未归属频道自愈：{}",
            to_resolve.join(",")
        ));

        let mut dirty = false;
        for channel_id in to_resolve {
            let from_response = last_messages
                .iter()
                .find(|item| loose_text(item.get("channelId")) == channel_id)
                .map(|item| loose_text(item.get("serverId")))
                .unwrap_or_default();
            match crate::room::room_channel_owner(pocket, &channel_id).await {
                Ok(info) => {
                    let owner_id = info.get("ownerId").and_then(Value::as_i64).unwrap_or(0);
                    let server_id = loose_text(info.get("serverId"));
                    let mut guard = self.lock();
                    // 复核时房间已重开：关闭集合里的旧记录作废
                    guard.closed_channel_map.remove(&channel_id);
                    guard.room_owners.insert(
                        channel_id.clone(),
                        json!({ "ownerId": owner_id, "resolvedAt": now_millis() }),
                    );
                    dirty = true;
                    if owner_id > 0 && followed_ids.contains(&owner_id) {
                        let owner_key = owner_id.to_string();
                        // 必须也允许补空占位：owner 解析失败时 channels 里已存在
                        // channelId = ''，只补「不存在」的 key 会让自愈被旧占位永久挡住
                        let needs = guard
                            .channels
                            .get(&owner_key)
                            .map(|channel| channel.channel_id.is_empty())
                            .unwrap_or(true);
                        if needs {
                            guard.channels.insert(
                                owner_key,
                                StoredChannel {
                                    server_id: if server_id.is_empty() {
                                        from_response.clone()
                                    } else {
                                        server_id
                                    },
                                    channel_id: channel_id.clone(),
                                    resolved_at: now_millis(),
                                    fail_count: None,
                                },
                            );
                            log_line(&format!(
                                "[pocket-unread] 频道归属补全 owner={owner_id} channel={channel_id}"
                            ));
                        }
                    }
                }
                Err(err) => {
                    if err.contains("登录已过期") {
                        return Err(err);
                    }
                    if crate::room::parse_question_envelope(&err).is_some() {
                        // 暗号频道 = 成员的小房间（拿不到 ownerId）：
                        // 按「同 server 的唯一关注成员」归属，多成员时不猜
                        let mut guard = self.lock();
                        let owners: Vec<String> = guard
                            .channels
                            .iter()
                            .filter(|(_, channel)| {
                                !channel.channel_id.is_empty() && channel.server_id == from_response
                            })
                            .map(|(user_id, _)| user_id.clone())
                            .collect();
                        if owners.len() == 1 && !guard.small_channels.contains_key(&owners[0]) {
                            let owner = owners[0].clone();
                            guard.small_channels.insert(
                                owner.clone(),
                                StoredChannel {
                                    server_id: from_response.clone(),
                                    channel_id: channel_id.clone(),
                                    resolved_at: now_millis(),
                                    fail_count: None,
                                },
                            );
                            log_line(&format!(
                                "[pocket-unread] 小房间归属补全 user={owner} channel={channel_id}"
                            ));
                        } else {
                            // 归属不唯一或已登记：记负缓存，避免每轮重打
                            guard.room_owners.insert(
                                channel_id.clone(),
                                json!({ "ownerId": 0, "resolvedAt": now_millis() }),
                            );
                        }
                        dirty = true;
                    } else if err.contains("关闭") {
                        // 房间关闭中：登记后供渲染层恢复按日期查看
                        let mut guard = self.lock();
                        if !from_response.is_empty() {
                            guard.closed_channel_map.insert(
                                channel_id.clone(),
                                json!({
                                    "serverId": from_response,
                                    "resolvedAt": now_millis(),
                                }),
                            );
                            dirty = true;
                        }
                        log_line(&format!(
                            "[pocket-unread] 房间已关闭，登记历史频道 {channel_id}"
                        ));
                    } else if err.contains("频道不存在") {
                        // 翻牌 / 礼物回复等非成员房间：负缓存按 TTL 重试
                        let mut guard = self.lock();
                        guard.closed_channel_map.remove(&channel_id);
                        guard.room_owners.insert(
                            channel_id.clone(),
                            json!({ "ownerId": 0, "resolvedAt": now_millis() }),
                        );
                        dirty = true;
                    } else {
                        // 网络类失败不记缓存，下轮重试
                        log_line(&format!(
                            "[pocket-unread] 频道归属反查失败（下轮重试）{channel_id}: {err}"
                        ));
                    }
                }
            }
        }
        if dirty {
            self.persist();
        }
        Ok(dirty)
    }

    /// 用实时解析结果校正大房间频道缓存（`server/jump` 才是权威，缓存只为省重解析）。
    ///
    /// 成员换房后 `channel_id` 会变，旧频道仍能读到历史消息、探测不失败，未读就一直算在它头上。
    /// 解析回空 = 房间已关闭，同样按失效处理。返回是否有变化（决定落盘与广播）。
    pub fn reconcile_channel(&self, user_id: i64, server_id: &str, channel_id: &str) -> bool {
        let server_id = server_id.trim();
        let channel_id = channel_id.trim();
        let mut guard = self.lock();
        let key = user_id.to_string();
        let Some(stored) = guard.channels.get(&key).cloned() else {
            // 未登记（未关注成员 / 临时房间）：没有缓存与未读可校正。
            // 别顺手写一条进去——下一轮 ensureChannels 会把它当「已取关」删掉
            return false;
        };
        if server_id.is_empty() || channel_id.is_empty() {
            // 解析成功但没有频道：房间已关闭 / 已不存在
            if stored.channel_id.is_empty() {
                return false;
            }
            clear_channel_runtime(&mut guard, &stored.channel_id);
            guard.channels.insert(
                key,
                StoredChannel {
                    server_id: String::new(),
                    channel_id: String::new(),
                    resolved_at: now_millis(),
                    fail_count: None,
                },
            );
            drop(guard);
            self.persist();
            log_line(&format!(
                "[pocket-unread] 房间已不存在，清掉频道缓存与未读 user={user_id} channel={}",
                stored.channel_id
            ));
            return true;
        }
        if stored.channel_id == channel_id && stored.server_id == server_id {
            return false;
        }
        if !stored.channel_id.is_empty() {
            clear_channel_runtime(&mut guard, &stored.channel_id);
        }
        guard.channels.insert(
            key,
            StoredChannel {
                server_id: server_id.to_string(),
                channel_id: channel_id.to_string(),
                resolved_at: now_millis(),
                fail_count: None,
            },
        );
        drop(guard);
        self.persist();
        log_line(&format!(
            "[pocket-unread] 房间频道已变更，未读改挂新频道 user={user_id} {} -> {channel_id}",
            stored.channel_id
        ));
        true
    }

    /// 探测失败累计：大房间连续失败达上限即作废（旧频道不再终身复用）。
    fn note_channel_failure(&self, channel_id: &str) -> bool {
        let mut guard = self.lock();
        let hit = guard
            .channels
            .iter()
            .find(|(_, channel)| channel.channel_id == channel_id)
            .map(|(user_id, channel)| (user_id.clone(), channel.clone()));
        let Some((user_id, channel)) = hit else {
            return false;
        };
        let count = channel.fail_count.unwrap_or(0) + 1;
        if count < CHANNEL_FAIL_LIMIT {
            if let Some(entry) = guard.channels.get_mut(&user_id) {
                entry.fail_count = Some(count);
            }
            return true;
        }
        // 作废重解析：置空占位且 resolved_at = 0，下一轮立即重解析，不必等占位 TTL
        clear_channel_runtime(&mut guard, &channel.channel_id);
        guard.channels.insert(
            user_id.clone(),
            StoredChannel {
                server_id: String::new(),
                channel_id: String::new(),
                resolved_at: 0,
                fail_count: None,
            },
        );
        drop(guard);
        self.persist();
        log_line(&format!(
            "[pocket-unread] 大房间频道连续探测失败，作废重解析 user={user_id} channel={}",
            channel.channel_id
        ));
        true
    }

    /// 探测成功即清零失败计数。
    fn clear_channel_failure(&self, channel_id: &str) -> bool {
        let mut guard = self.lock();
        let mut touched = false;
        // 分两次借用：同一个 guard 上不能同时可变借两个字段的迭代器
        let channels = &mut guard.channels;
        for channel in channels.values_mut() {
            if channel.channel_id == channel_id && channel.fail_count.is_some() {
                channel.fail_count = None;
                touched = true;
            }
        }
        let small = &mut guard.small_channels;
        for channel in small.values_mut() {
            if channel.channel_id == channel_id && channel.fail_count.is_some() {
                channel.fail_count = None;
                touched = true;
            }
        }
        touched
    }

    /// 成员的小房间频道（未发现 / 已作废时为 `None`）。
    pub fn small_channel(&self, user_id: i64) -> Option<(String, String)> {
        self.lock()
            .small_channels
            .get(&user_id.to_string())
            .filter(|channel| !channel.channel_id.is_empty())
            .map(|channel| (channel.server_id.clone(), channel.channel_id.clone()))
    }

    /// 作废成员的小房间归属（房主更新小房间后旧频道会失效）。
    ///
    /// 清掉该频道的未读 / 水位 / 触发锚，并放开该 server 的发现间隔让下一轮立刻重找；
    /// 大房间不动。
    pub fn invalidate_small_channel(&self, user_id: i64) -> bool {
        let removed = {
            let mut guard = self.lock();
            let removed = guard.small_channels.remove(&user_id.to_string());
            if let Some(channel) = &removed {
                clear_channel_runtime(&mut guard, &channel.channel_id);
            }
            removed
        };
        match removed {
            Some(channel) => {
                if let Ok(mut map) = self.small_discover_at.lock() {
                    map.remove(&channel.server_id);
                }
                self.persist();
                log_line(&format!(
                    "[pocket-unread] 作废小房间归属 user={user_id} channel={}",
                    channel.channel_id
                ));
                true
            }
            None => false,
        }
    }

    /// 重新发现成员的小房间并返回结果。
    ///
    /// `force = false` 走 server 级间隔（反复选中同一成员不会反复打接口）；
    /// `force = true` 用于频道刚失效——立刻枚举，不等检测轮。
    pub async fn refresh_small_channel(
        &self,
        pocket: &PocketAccount,
        cache: &crate::room::RoomCache,
        user_id: i64,
        force: bool,
    ) -> Result<Option<(String, String)>, String> {
        if !pocket.authorized() {
            return Ok(None);
        }
        self.discover_small_channels(pocket, cache, Some(user_id), force)
            .await?;
        Ok(self.small_channel(user_id))
    }

    /// 主动发现小房间（暗号频道）。
    ///
    /// 用 `server/detail` 枚举该 server 的全部频道，逐个 `room/info` 试探：正常回包是普通频道，
    /// 回包带 `questionId` 信封即被暗号拦下（未通过暗号的账号拿不到 `channelInfo`）。
    /// 只枚举「一个 server 下只有一个关注成员」的情况，多成员共用时无法归属。
    pub async fn discover_small_channels(
        &self,
        pocket: &PocketAccount,
        cache: &crate::room::RoomCache,
        targets: Option<i64>,
        force: bool,
    ) -> Result<bool, String> {
        // 候选：已解析出大房间的成员；按 server 统计关注人数
        let (candidates, owners_by_server) = {
            let guard = self.lock();
            let mut owners: HashMap<String, usize> = HashMap::new();
            for channel in guard.channels.values() {
                if !channel.server_id.is_empty() && !channel.channel_id.is_empty() {
                    *owners.entry(channel.server_id.clone()).or_insert(0) += 1;
                }
            }
            let list: Vec<(String, StoredChannel)> = guard
                .channels
                .iter()
                .filter(|(user_id, channel)| {
                    if let Some(target) = targets {
                        return user_id.as_str() == target.to_string();
                    }
                    !channel.server_id.is_empty() && !channel.channel_id.is_empty()
                })
                .map(|(user_id, channel)| (user_id.clone(), channel.clone()))
                .collect();
            (list, owners)
        };

        let mut dirty = false;
        for (user_id, channel) in candidates {
            if channel.server_id.is_empty() || channel.channel_id.is_empty() {
                continue;
            }
            // 一个 server 下有多个关注成员：无法归属，跳过
            if owners_by_server
                .get(&channel.server_id)
                .copied()
                .unwrap_or(0)
                != 1
            {
                continue;
            }
            if !force {
                let recent = self
                    .small_discover_at
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&channel.server_id).copied())
                    .map(|at| at.elapsed().as_millis() as i64 <= SMALL_DISCOVER_TTL_MS)
                    .unwrap_or(false);
                if recent {
                    continue;
                }
            }
            if let Ok(mut map) = self.small_discover_at.lock() {
                map.insert(channel.server_id.clone(), Instant::now());
            }

            let list = match crate::room::server_channels(cache, pocket, &channel.server_id, force)
                .await
            {
                Ok(list) => list,
                Err(err) => {
                    // 失败不占用间隔：下一轮重试同一 server
                    if let Ok(mut map) = self.small_discover_at.lock() {
                        map.remove(&channel.server_id);
                    }
                    log_line(&format!(
                        "[pocket-unread] 小房间枚举失败 user={user_id} server={}: {err}",
                        channel.server_id
                    ));
                    continue;
                }
            };
            let mut found: Option<String> = None;
            for candidate in list.as_array().cloned().unwrap_or_default() {
                let candidate_id = loose_text(candidate.get("channelId"));
                if candidate_id.is_empty() || candidate_id == channel.channel_id {
                    continue;
                }
                match crate::room::room_channel_owner(pocket, &candidate_id).await {
                    // 正常回包：普通频道（公告 / 互动）
                    Ok(_) => {}
                    Err(err) => {
                        if err.contains("登录已过期") {
                            return Err(err);
                        }
                        if crate::room::parse_question_envelope(&err).is_some() {
                            found = Some(candidate_id);
                            break;
                        }
                        // 频道不存在等：列表里可能残留已删频道，继续看下一个候选
                    }
                }
            }

            let mut guard = self.lock();
            match found {
                Some(channel_id) => {
                    let previous = guard
                        .small_channels
                        .get(&user_id)
                        .map(|channel| channel.channel_id.clone());
                    if previous.as_deref() == Some(channel_id.as_str()) {
                        continue;
                    }
                    if let Some(previous) = previous {
                        clear_channel_runtime(&mut guard, &previous);
                    }
                    guard.small_channels.insert(
                        user_id.clone(),
                        StoredChannel {
                            server_id: channel.server_id.clone(),
                            channel_id: channel_id.clone(),
                            resolved_at: now_millis(),
                            fail_count: None,
                        },
                    );
                    dirty = true;
                    log_line(&format!(
                        "[pocket-unread] 小房间主动发现 user={user_id} channel={channel_id}"
                    ));
                }
                None => {
                    // 旧频道已不在列表里 = 确实被删了（房主「只删不建」）
                    let stale = guard
                        .small_channels
                        .get(&user_id)
                        .map(|channel| channel.channel_id.clone());
                    if let Some(stale) = stale {
                        let still_listed = list
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .any(|item| loose_text(item.get("channelId")) == stale)
                            })
                            .unwrap_or(false);
                        if !still_listed {
                            guard.small_channels.remove(&user_id);
                            clear_channel_runtime(&mut guard, &stale);
                            dirty = true;
                            log_line(&format!(
                                "[pocket-unread] 小房间已不存在，作废旧记录 user={user_id} channel={stale}"
                            ));
                        }
                    }
                }
            }
        }
        if dirty {
            self.persist();
        }
        Ok(dirty)
    }

    /// 标记已读：拉一页最新消息取水位（对齐到「此刻」），并清零该成员未读。
    pub async fn mark_read(
        &self,
        pocket: &PocketAccount,
        user_id: i64,
        target: &str,
    ) -> Result<Option<Value>, String> {
        // 目标频道：小房间走 small_channels，大房间走 channels（与 Electron 同口径）
        let channel = {
            let guard = self.lock();
            let key = user_id.to_string();
            let entry = if target == "small" {
                guard.small_channels.get(&key)
            } else {
                guard.channels.get(&key)
            };
            entry
                .filter(|channel| !channel.channel_id.is_empty())
                .cloned()
        };
        // 频道不存在（如小房间还没被归属自愈发现）：无从对齐，无事可做
        let Some(channel) = channel else {
            return Ok(None);
        };

        // **先取旧水位**：渲染层要用它裁「分割线」——分割线画在旧水位那条之后。
        // 拿新水位返回的话分割线永远落在列表末尾（等于画不出来）。
        let previous = {
            let guard = self.lock();
            guard.read_marks.get(&channel.channel_id).cloned()
        };
        let prev_value = previous.map(|mark| {
            json!({
                "msgTime": mark.msg_time,
                // 落盘的占位 id "0" 归一成空串
                "msgIdServer": if mark.msg_id_server == "0" { String::new() } else { mark.msg_id_server },
            })
        });

        // 清零该频道计数（另一路保留），立即重建快照
        let had_unread = {
            let mut guard = self.lock();
            let had = guard
                .unread_channels
                .get(&channel.channel_id)
                .map(|entry| entry.count > 0)
                .unwrap_or(false);
            guard.unread_channels.remove(&channel.channel_id);
            had
        };
        self.persist();
        if !had_unread {
            // 本就没未读：不必再拉一页列表，水位还在原处，新消息照常由检测轮数出来
            return Ok(prev_value);
        }

        // 把水位推到「当下」：拉一页最新消息，取最新一条
        match room_messages(
            pocket,
            &channel.channel_id,
            &channel.server_id,
            0.0,
            "member",
        )
        .await
        {
            Ok(messages) => {
                let newest = messages
                    .get("messages")
                    .and_then(Value::as_array)
                    .and_then(|list| list.first())
                    .cloned();
                let mark = match &newest {
                    Some(message) => ReadMark {
                        msg_time: message
                            .get("msgTime")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0) as i64,
                        msg_id_server: loose_text(message.get("msgIdServer")),
                    },
                    None => ReadMark {
                        msg_time: now_millis(),
                        msg_id_server: String::new(),
                    },
                };
                {
                    let mut guard = self.lock();
                    guard.read_marks.insert(channel.channel_id.clone(), mark);
                }
                self.persist();
                log_line(&format!(
                    "[pocket-unread] 已读：user={user_id} channel={}",
                    channel.channel_id
                ));
            }
            Err(err) => {
                // 对齐失败挂起：下一轮检测先对齐水位再计数，免得刚读过的消息被当成未读复活
                log_line(&format!(
                    "[pocket-unread] 已读水位对齐失败（挂起，下一轮重试）channel={}: {err}",
                    channel.channel_id
                ));
            }
        }
        Ok(prev_value)
    }

    /// 清空账号相关未读（登出 / token 失效）：清未读计数、水位、触发锚与预览；
    /// 频道归属与账号无关，保留。
    pub fn clear_account_unread(&self) -> bool {
        let changed = {
            let mut guard = self.lock();
            let had = !guard.unread_channels.is_empty()
                || !guard.read_marks.is_empty()
                || !guard.seen_keys.is_empty()
                || !guard.previews.is_empty();
            guard.unread_channels.clear();
            guard.read_marks.clear();
            guard.seen_keys.clear();
            // 预览是上一个账号的房间进度，留着会在登出后继续显示。
            guard.previews.clear();
            had
        };
        if changed {
            self.persist();
        }
        changed
    }

    fn persist(&self) {
        let serialized = {
            let guard = self.lock();
            match serde_json::to_string(&*guard) {
                Ok(text) => text,
                Err(err) => {
                    log_line(&format!("[pocket-unread] 序列化失败: {err}"));
                    return;
                }
            }
        };
        // 原子写：tmp + rename
        let tmp = self.path.with_extension("json.tmp");
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&tmp, serialized).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, &self.path);
    }

    /// 一轮检测；返回快照是否变化（决定要不要广播）。
    pub async fn tick(
        &self,
        pocket: &PocketAccount,
        followed_ids: &[i64],
        blocked_ids: &[i64],
    ) -> Result<bool, String> {
        {
            let mut ticking = self
                .ticking
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *ticking {
                return Ok(false);
            }
            *ticking = true;
        }
        let result = self.tick_inner(pocket, followed_ids, blocked_ids).await;
        {
            let mut ticking = self
                .ticking
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *ticking = false;
        }
        result
    }

    async fn tick_inner(
        &self,
        pocket: &PocketAccount,
        followed_ids: &[i64],
        blocked_ids: &[i64],
    ) -> Result<bool, String> {
        // 凭证还在后台装载时既不算「已登录」也不能清空：等它读完再决定。
        if !pocket.is_loaded() {
            return Ok(false);
        }
        if !pocket.authorized() {
            return Ok(self.clear_account_unread());
        }
        let blocked: HashSet<i64> = blocked_ids.iter().copied().collect();
        let targets: Vec<i64> = followed_ids
            .iter()
            .copied()
            .filter(|id| !blocked.contains(id))
            .collect();

        // 1) 确保关注成员的频道已解析（缓存命中不打接口）
        for user_id in &targets {
            let cached = {
                let guard = self.lock();
                guard
                    .channels
                    .get(&user_id.to_string())
                    .map(|channel| !channel.channel_id.is_empty())
                    .unwrap_or(false)
            };
            if cached {
                continue;
            }
            let channel = room_channel(pocket, *user_id).await?;
            let server_id = channel
                .get("serverId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let channel_id = channel
                .get("channelId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut guard = self.lock();
            guard.channels.insert(
                user_id.to_string(),
                StoredChannel {
                    server_id,
                    channel_id,
                    resolved_at: now_millis(),
                    fail_count: None,
                },
            );
        }

        // 2) 归属表：channelId → (userId, serverId)
        let (server_ids, owner, server_of) = {
            let guard = self.lock();
            let mut servers: Vec<String> = Vec::new();
            let mut owner = HashMap::new();
            let mut server_of = HashMap::new();
            for (user_id, channel) in &guard.channels {
                if channel.channel_id.is_empty() {
                    continue;
                }
                if !servers.contains(&channel.server_id) {
                    servers.push(channel.server_id.clone());
                }
                owner.insert(channel.channel_id.clone(), user_id.clone());
                server_of.insert(channel.channel_id.clone(), channel.server_id.clone());
            }
            (servers, owner, server_of)
        };
        if server_ids.is_empty() {
            return Ok(false);
        }

        // 3) 批量预览：谁有新消息（只作触发，不参与计数）
        let previews = room_last_messages(pocket, &server_ids).await?;
        let previews = previews.as_array().cloned().unwrap_or_default();
        let mut pending: Vec<(String, String)> = Vec::new();
        {
            let mut guard = self.lock();
            guard.previews = previews.clone();
        }
        {
            let guard = self.lock();
            for item in &previews {
                let channel_id = item
                    .get("channelId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !owner.contains_key(&channel_id) {
                    continue;
                }
                let msg_key = item
                    .get("msgKey")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let server_id = item
                    .get("serverId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|text| !text.is_empty())
                    .or_else(|| server_of.get(&channel_id).cloned())
                    .unwrap_or_default();
                if server_id.is_empty() {
                    continue;
                }
                match guard.seen_keys.get(&channel_id) {
                    Some(seen) if seen == &msg_key => {}
                    _ => pending.push((channel_id, server_id)),
                }
            }
        }

        // 4) 补探：有未读又久未探测的频道重算一次（停更后触发锚冻住，未读会永久挂旧值）
        {
            let guard = self.lock();
            let mut extra = Vec::new();
            for (channel_id, entry) in &guard.unread_channels {
                if entry.count <= 0 || pending.iter().any(|(id, _)| id == channel_id) {
                    continue;
                }
                let stale = self
                    .probe_at
                    .lock()
                    .map(|map| {
                        map.get(channel_id)
                            .map(|at| at.elapsed().as_millis() as i64 > STALE_UNREAD_PROBE_MS)
                            .unwrap_or(true)
                    })
                    .unwrap_or(false);
                if stale && let Some(server_id) = server_of.get(channel_id) {
                    extra.push((channel_id.clone(), server_id.clone()));
                }
            }
            pending.extend(extra);
        }

        let mut changed = false;

        // 4.5) 归属自愈：把预览里出现但归属表没有的频道补上
        //      （大房间 / 小房间 / 关闭中的历史频道三种结果）
        let mut healed = false;
        match self
            .heal_channel_owners(pocket, &previews, followed_ids)
            .await
        {
            Ok(changed) => healed = changed,
            Err(err) => {
                if err.contains("登录已过期") {
                    return Err(err);
                }
                log_line(&format!(
                    "[pocket-unread] 频道归属自愈失败（下一轮再试）: {err}"
                ));
            }
        }
        if healed {
            changed = true;
        }

        // 4.6) 定期复核：成员换房后旧频道若没有未读，补探永远轮不到它，缓存终生不校正。
        //      每轮只复核最久未复核的几个，失败保留缓存并把 resolvedAt 顺延，不形成重试风暴。
        {
            let recheck: Vec<(String, StoredChannel)> = {
                let guard = self.lock();
                let cutoff = now_millis() - CHANNEL_RECHECK_TTL_MS;
                let mut list: Vec<(String, StoredChannel)> = guard
                    .channels
                    .iter()
                    .filter(|(_, channel)| {
                        !channel.channel_id.is_empty() && channel.resolved_at <= cutoff
                    })
                    .map(|(user_id, channel)| (user_id.clone(), channel.clone()))
                    .collect();
                list.sort_by_key(|(_, channel)| channel.resolved_at);
                list.truncate(CHANNEL_RECHECK_PER_TICK);
                list
            };
            for (user_id, channel) in recheck {
                let user_id_num: i64 = user_id.parse().unwrap_or(0);
                if user_id_num <= 0 {
                    continue;
                }
                match room_channel(pocket, user_id_num).await {
                    Ok(fresh) => {
                        let server_id = loose_text(fresh.get("serverId"));
                        let channel_id = loose_text(fresh.get("channelId"));
                        if self.reconcile_channel(user_id_num, &server_id, &channel_id) {
                            changed = true;
                        }
                    }
                    Err(err) => {
                        if err.contains("登录已过期") {
                            return Err(err);
                        }
                        log_line(&format!(
                            "[pocket-unread] 频道归属定期复核失败（按 TTL 再试）user={user_id}: {err}"
                        ));
                    }
                }
                // 复核结果无变化时也推后 resolved_at，避免同一批成员每轮重试
                let mut guard = self.lock();
                if let Some(entry) = guard.channels.get_mut(&user_id)
                    && entry.channel_id == channel.channel_id
                {
                    entry.resolved_at = now_millis();
                }
            }
        }

        // 5) 计数：每轮最多 MAX_MESSAGE_PROBES 个频道
        pending.truncate(MAX_MESSAGE_PROBES);
        for (channel_id, server_id) in &pending {
            match tokio::time::timeout(
                Duration::from_secs(30),
                room_messages(pocket, channel_id, server_id, 0.0, "member"),
            )
            .await
            {
                Ok(Ok(messages)) => {
                    changed |= self.apply_probe(channel_id, &messages);
                    // 探测成功即清零失败计数（否则跨场累计会误判成换房）
                    self.clear_channel_failure(channel_id);
                }
                Ok(Err(err)) => {
                    if err.contains("登录已过期") {
                        return Err(err);
                    }
                    // 连续失败达上限 → 作废该成员的大房间缓存，下一轮重解析
                    if err.contains("频道不存在") || err.contains("已关闭") {
                        self.note_channel_failure(channel_id);
                    }
                    log_line(&format!(
                        "[pocket-unread] 频道 {channel_id} 探测失败（下一轮再试）: {err}"
                    ));
                }
                Err(_) => {
                    log_line(&format!("[pocket-unread] 频道 {channel_id} 探测超时"));
                }
            }
            if let Ok(mut map) = self.probe_at.lock() {
                map.insert(channel_id.clone(), Instant::now());
            }
        }
        if changed {
            self.persist();
        }
        Ok(changed)
    }

    /// 应用一次探测结果：数出比水位更新的条数并写入未读。
    fn apply_probe(&self, channel_id: &str, payload: &Value) -> bool {
        let messages: Vec<Value> = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // 官方系统账号不计入未读
        let countable: Vec<&Value> = messages
            .iter()
            .filter(|message| {
                message
                    .get("userId")
                    .map(|value| match value {
                        Value::String(text) => text != OFFICIAL_POCKET_USER_ID,
                        Value::Number(number) => number.to_string() != OFFICIAL_POCKET_USER_ID,
                        _ => true,
                    })
                    .unwrap_or(true)
            })
            .collect();
        let mut guard = self.lock();
        // 首见锚定：没有水位就不算历史未读，直接把水位锚到最新一条
        if !guard.read_marks.contains_key(channel_id)
            && let Some(newest) = countable.first()
        {
            guard.read_marks.insert(
                channel_id.to_string(),
                ReadMark {
                    msg_time: newest.get("msgTime").and_then(Value::as_f64).unwrap_or(0.0) as i64,
                    msg_id_server: text_of(newest.get("msgIdServer")),
                },
            );
        }
        let mark = guard
            .read_marks
            .get(channel_id)
            .cloned()
            .unwrap_or_default();
        let (count, latest_time) = count_newer(&countable, &mark);
        let before = guard.unread_channels.get(channel_id).cloned();
        let after = if count > 0 {
            Some(ChannelUnread { count, latest_time })
        } else {
            None
        };
        let changed = before != after;
        match after {
            Some(entry) => {
                guard.unread_channels.insert(channel_id.to_string(), entry);
            }
            None => {
                guard.unread_channels.remove(channel_id);
            }
        }
        changed
    }
}

/// 关闭频道的快照形态（渲染层据此标记左列并恢复按日期查看）。
fn closed_channel_list(state: &StoredUnread) -> Vec<Value> {
    state
        .closed_channel_map
        .iter()
        .map(|(channel_id, info)| {
            json!({
                "channelId": channel_id,
                "serverId": info.get("serverId").cloned().unwrap_or(json!("")),
            })
        })
        .collect()
}

/// 清掉某频道的未读 / 水位 / 触发锚：频道失效或换成新频道时调用，大房间不走这里。
fn clear_channel_runtime(state: &mut StoredUnread, channel_id: &str) {
    state.unread_channels.remove(channel_id);
    state.read_marks.remove(channel_id);
    state.seen_keys.remove(channel_id);
    state.probe_at_placeholder(channel_id);
}

impl StoredUnread {
    /// `last_probe_at` 是落盘字段，探测时刻另有一份内存表；这里只清落盘那份。
    fn probe_at_placeholder(&mut self, channel_id: &str) {
        self.last_probe_at.remove(channel_id);
    }
}

/// 数出比已读水位更新的条数（列表按 msgTime 倒序，最新在前）。
///
/// 优先按消息 id 定位水位那条：找到了，它前面的就是未读；找不到说明水位那条已沉到
/// 本页之外，整页都比它新 —— 条数封顶 `ROOM_MESSAGE_LIMIT`（渲染层据此显示 50+）。
fn count_newer(messages: &[&Value], mark: &ReadMark) -> (i64, i64) {
    if messages.is_empty() {
        return (0, 0);
    }
    let latest = messages
        .first()
        .map(|message| {
            message
                .get("msgTime")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as i64
        })
        .unwrap_or(0);
    let mark_id = mark.msg_id_server.trim();
    let use_id = !mark_id.is_empty()
        && mark_id != "0"
        && !text_of(messages[0].get("msgIdServer")).is_empty();
    if use_id {
        let index = messages
            .iter()
            .position(|message| text_of(message.get("msgIdServer")) == mark_id);
        let count = match index {
            Some(index) => index as i64,
            None => messages.len().min(ROOM_MESSAGE_LIMIT as usize) as i64,
        };
        return (count, latest);
    }
    let newer = messages
        .iter()
        .filter(|message| {
            (message
                .get("msgTime")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as i64)
                > mark.msg_time
        })
        .count() as i64;
    (newer.min(ROOM_MESSAGE_LIMIT), latest)
}

/// 按频道归属聚合为「成员 → 未读快照」。
fn aggregate(state: &StoredUnread) -> BTreeMap<String, UnreadRoomState> {
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for (user_id, channel) in &state.channels {
        if !channel.channel_id.is_empty() {
            owner.insert(channel.channel_id.as_str(), user_id.as_str());
        }
    }
    let mut out: BTreeMap<String, UnreadRoomState> = BTreeMap::new();
    for (channel_id, unread) in &state.unread_channels {
        if unread.count <= 0 {
            continue;
        }
        let Some(user_id) = owner.get(channel_id.as_str()) else {
            continue;
        };
        let entry = out.entry(user_id.to_string()).or_default();
        entry.count += unread.count;
        // 本版本没有小房间，全部计大房间
        entry.big_count += unread.count;
        entry.latest_time = entry.latest_time.max(unread.latest_time);
    }
    out
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, time: i64, user: &str) -> Value {
        json!({ "msgIdServer": id, "msgTime": time, "userId": user })
    }

    #[test]
    fn count_newer_uses_id_when_available() {
        let list = [
            message("m3", 300, "u1"),
            message("m2", 200, "u1"),
            message("m1", 100, "u1"),
        ];
        let refs: Vec<&Value> = list.iter().collect();
        let mark = ReadMark {
            msg_time: 200,
            msg_id_server: "m2".to_string(),
        };
        assert_eq!(count_newer(&refs, &mark).0, 1, "m3 比 m2 新");
    }

    #[test]
    fn count_newer_falls_back_to_time_and_caps() {
        let list: Vec<Value> = (0..60)
            .map(|index| message("", 1000 - index, "u1"))
            .collect();
        let refs: Vec<&Value> = list.iter().collect();
        let mark = ReadMark {
            msg_time: 0,
            msg_id_server: String::new(),
        };
        // 无 id 时按时刻比较，并封顶在 ROOM_MESSAGE_LIMIT
        assert_eq!(count_newer(&refs, &mark).0, ROOM_MESSAGE_LIMIT);
    }

    #[test]
    fn count_newer_caps_when_mark_is_off_page() {
        let list = [message("m3", 300, "u1"), message("m2", 200, "u1")];
        let refs: Vec<&Value> = list.iter().collect();
        let mark = ReadMark {
            msg_time: 1,
            msg_id_server: "m1".to_string(),
        };
        // 水位那条沉到本页之外：整页都算未读
        assert_eq!(count_newer(&refs, &mark).0, 2);
    }

    #[test]
    fn aggregate_only_reports_positive_counts() {
        let mut state = StoredUnread::default();
        state.channels.insert(
            "63".to_string(),
            StoredChannel {
                server_id: "1213763".to_string(),
                channel_id: "1361347".to_string(),
                resolved_at: 0,
                fail_count: None,
            },
        );
        state.unread_channels.insert(
            "1361347".to_string(),
            ChannelUnread {
                count: 3,
                latest_time: 500,
            },
        );
        state.unread_channels.insert(
            "unowned".to_string(),
            ChannelUnread {
                count: 9,
                latest_time: 600,
            },
        );
        let rooms = aggregate(&state);
        assert_eq!(rooms.len(), 1, "无归属频道不进快照");
        assert_eq!(rooms["63"].count, 3);
        assert_eq!(rooms["63"].big_count, 3);
        assert_eq!(rooms["63"].small_count, 0);
        assert_eq!(rooms["63"].latest_time, 500);
    }

    #[test]
    fn reconcile_moves_unread_to_new_channel() {
        let dir = std::env::temp_dir().join(format!(
            "d48-reconcile-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        {
            let mut guard = tracker.lock();
            guard.channels.insert(
                "63".to_string(),
                StoredChannel {
                    server_id: "1".to_string(),
                    channel_id: "old".to_string(),
                    resolved_at: 0,
                    fail_count: None,
                },
            );
            guard.unread_channels.insert(
                "old".to_string(),
                ChannelUnread {
                    count: 5,
                    latest_time: 1,
                },
            );
        }
        // 换成新频道：未读要从旧频道摘掉
        assert!(tracker.reconcile_channel(63, "1", "new"));
        {
            let guard = tracker.lock();
            assert_eq!(guard.channels["63"].channel_id, "new");
            assert!(!guard.unread_channels.contains_key("old"));
        }
        // 同一频道再校正：无变化
        assert!(!tracker.reconcile_channel(63, "1", "new"));
        // 未登记成员：不写缓存（否则会被 ensureChannels 当「已取关」删掉）
        assert!(!tracker.reconcile_channel(999, "1", "x"));
        assert!(!tracker.lock().channels.contains_key("999"));
        // 解析回空 = 房间关闭：清空占位并清未读
        tracker.lock().unread_channels.insert(
            "new".to_string(),
            ChannelUnread {
                count: 2,
                latest_time: 2,
            },
        );
        assert!(tracker.reconcile_channel(63, "", ""));
        {
            let guard = tracker.lock();
            assert!(guard.channels["63"].channel_id.is_empty());
            assert!(guard.unread_channels.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_failure_invalidates_after_limit() {
        let dir = std::env::temp_dir().join(format!(
            "d48-fail-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        tracker.lock().channels.insert(
            "63".to_string(),
            StoredChannel {
                server_id: "1".to_string(),
                channel_id: "ch".to_string(),
                resolved_at: 0,
                fail_count: None,
            },
        );
        // 前两次只累计，不清空
        for _ in 0..CHANNEL_FAIL_LIMIT - 1 {
            assert!(tracker.note_channel_failure("ch"));
            assert_eq!(tracker.lock().channels["63"].channel_id, "ch");
        }
        // 达上限即作废：置空占位 + resolved_at = 0
        assert!(tracker.note_channel_failure("ch"));
        {
            let guard = tracker.lock();
            assert!(guard.channels["63"].channel_id.is_empty());
            assert_eq!(guard.channels["63"].resolved_at, 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_read_returns_previous_watermark_for_divider() {
        // 分割线要画在「旧水位那条之后」：mark_read 必须返回**旧**水位，不是新的
        let dir = std::env::temp_dir().join(format!(
            "d48-mark-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        {
            let mut guard = tracker.lock();
            guard.channels.insert(
                "63".to_string(),
                StoredChannel {
                    server_id: "1".to_string(),
                    channel_id: "ch".to_string(),
                    resolved_at: 0,
                    fail_count: None,
                },
            );
            guard.read_marks.insert(
                "ch".to_string(),
                ReadMark {
                    msg_time: 1000,
                    msg_id_server: "m-old".to_string(),
                },
            );
        }
        // 没有未读时直接返回旧水位，不发网络请求
        let pocket = crate::pocket::PocketAccount::load(
            dir.clone(),
            "0123456789abcdef".to_string(),
            reqwest::Client::new(),
            Box::new(crate::pocket::MemoryVault::default()),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = runtime
            .block_on(tracker.mark_read(&pocket, 63, "room"))
            .unwrap()
            .unwrap();
        assert_eq!(value["msgIdServer"], "m-old");
        assert_eq!(value["msgTime"], 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_read_ignores_placeholder_id() {
        let dir = std::env::temp_dir().join(format!(
            "d48-mark0-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        {
            let mut guard = tracker.lock();
            guard.channels.insert(
                "63".to_string(),
                StoredChannel {
                    server_id: "1".to_string(),
                    channel_id: "ch".to_string(),
                    resolved_at: 0,
                    fail_count: None,
                },
            );
            guard.read_marks.insert(
                "ch".to_string(),
                ReadMark {
                    msg_time: 5,
                    msg_id_server: "0".to_string(),
                },
            );
        }
        let pocket = crate::pocket::PocketAccount::load(
            dir.clone(),
            "0123456789abcdef".to_string(),
            reqwest::Client::new(),
            Box::new(crate::pocket::MemoryVault::default()),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = runtime
            .block_on(tracker.mark_read(&pocket, 63, "room"))
            .unwrap()
            .unwrap();
        // 占位 id '0' 归一成空串（渲染层据此退回按时刻定位）
        assert_eq!(value["msgIdServer"], "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tracker_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!(
            "d48-unread-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        {
            let mut guard = tracker.lock();
            guard.channels.insert(
                "63".to_string(),
                StoredChannel {
                    server_id: "1".to_string(),
                    channel_id: "2".to_string(),
                    resolved_at: 0,
                    fail_count: None,
                },
            );
            guard.unread_channels.insert(
                "2".to_string(),
                ChannelUnread {
                    count: 5,
                    latest_time: 1,
                },
            );
        }
        tracker.persist();
        let reloaded = UnreadTracker::load(&dir);
        assert_eq!(reloaded.snapshot().rooms["63"].count, 5);
        // 清空账号未读：频道缓存保留
        assert!(reloaded.clear_account_unread());
        assert!(reloaded.snapshot().rooms.is_empty());
        let guard = reloaded.lock();
        assert!(guard.channels.contains_key("63"));
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_clears_previews_but_keeps_channel_cache() {
        // 登出后房间页左列不能还显示上一个账号的预览
        let dir = std::env::temp_dir().join(format!(
            "d48-unread-logout-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = UnreadTracker::load(&dir);
        {
            let mut guard = tracker.lock();
            guard.channels.insert(
                "63".to_string(),
                StoredChannel {
                    server_id: "1".to_string(),
                    channel_id: "2".to_string(),
                    resolved_at: 0,
                    fail_count: None,
                },
            );
            guard.unread_channels.insert(
                "2".to_string(),
                ChannelUnread {
                    count: 5,
                    latest_time: 1,
                },
            );
            guard.previews = vec![serde_json::json!({ "channelId": "2", "text": "hi" })];
            guard
                .read_marks
                .insert("2".to_string(), ReadMark::default());
        }
        tracker.persist();

        let reloaded = UnreadTracker::load(&dir);
        assert!(reloaded.clear_account_unread());
        let snapshot = reloaded.snapshot();
        assert!(snapshot.rooms.is_empty(), "登出后不该还有未读");
        assert!(snapshot.previews.is_empty(), "登出后不该还有房间预览");
        // 频道归属与账号无关，保留下来省一次反查
        assert!(reloaded.lock().channels.contains_key("63"));
        // 重复调用是幂等的：没东西可清就返回 false
        assert!(!reloaded.clear_account_unread());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_file_falls_back_to_empty() {
        let dir = std::env::temp_dir().join(format!(
            "d48-unread-bad-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(UNREAD_FILE), "{ broken").unwrap();
        let tracker = UnreadTracker::load(&dir);
        assert!(tracker.snapshot().rooms.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
