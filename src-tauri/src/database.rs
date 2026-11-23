//! `database.json` 读写门面：成员数据、屏蔽/关注名单、配置与成员树。
//!
//! 对应 Electron `main/database.ts`。读路径不写盘；写路径为原子写（tmp + fsync + rename），
//! 写前把当前主文件备份为 `.bak`，从 `.bak` 恢复后的首次写入不得备份损坏文件。
//! 读改写都在同一把 RwLock 内完成，写盘失败时内存保持旧值。

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::{Map, Value, json};

/// 配置键与默认值，与 `common/app-config.ts` 同源。
pub const CONFIG_KEYS: &[&str] = &[
    "downloadDirectory",
    "ffmpegDirectory",
    "danmakuPlacement",
    "floatTopmost",
];

/// 团体别名：19（明星殿堂）归到 SNH48(10)。见 Electron `domain/member-tree.ts`。
const GROUP_ALIASES: &[(&str, i64)] = &[("19", 10)];

/// `saveMemberData` 允许写入的分节，对应 Electron `MemberDataContent` 的 10 个字段。
const MEMBER_SECTIONS: &[&str] = &[
    "officialInfo",
    "domainInfo",
    "starInfo",
    "starAdjunctInfo",
    "starRepeatInfo",
    "groupInfo",
    "periodInfo",
    "teamInfo",
    "starOfficialRelationInfo",
    "allmembers",
];

pub struct Database {
    path: PathBuf,
    data: RwLock<Value>,
    /// 主文件损坏、本次从 .bak 恢复：首次写入前不能备份损坏的主文件。
    restored_from_bak: AtomicBool,
}

impl Database {
    /// 读主库；损坏/缺失时回退 `database.json.bak`，再不行用空库。
    /// 全程只读，不会创建任何文件。
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let main_exists = path.exists();
        let main = read_json(&path);
        let (data, restored_from_bak) = match main {
            Some(value) => (value, false),
            None if main_exists => (
                read_json(&backup_path(&path)).unwrap_or_else(default_data),
                true,
            ),
            None => (
                read_json(&backup_path(&path)).unwrap_or_else(default_data),
                false,
            ),
        };
        Self {
            path,
            data: RwLock::new(normalize_root(data)),
            restored_from_bak: AtomicBool::new(restored_from_bak),
        }
    }

    /// 测试/探针注入内存数据，避免触碰真实库。
    pub fn from_value(value: Value) -> Self {
        Self {
            path: PathBuf::new(),
            data: RwLock::new(normalize_root(value)),
            restored_from_bak: AtomicBool::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> RwLockReadGuard<'_, Value> {
        self.data
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_guard(&self) -> RwLockWriteGuard<'_, Value> {
        self.data
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 读改写事务：先在副本上变更，落盘成功后才替换内存；写盘失败时内存保持旧值。
    /// 闭包返回 `false` 表示无变化，不写盘（名单增删幂等）。
    fn mutate<F>(&self, change: F) -> Result<bool, String>
    where
        F: FnOnce(&mut Value) -> Result<bool, String>,
    {
        let mut guard = self.write_guard();
        let mut next = guard.clone();
        if !change(&mut next)? {
            return Ok(false);
        }
        self.persist(&next)?;
        *guard = next;
        Ok(true)
    }

    /// 原子写：备份当前主文件 → tmp 写入 + fsync → rename。
    /// 与 Electron `SafeJSONFileSync` 同语义，但加载阶段不隔离 `.corrupt`（保持只读）。
    fn persist(&self, data: &Value) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("内存数据库不支持写入".to_string());
        }
        let serialized = serde_json::to_string(data)
            .map_err(|err| format!("database.json 序列化失败: {err}"))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("创建数据目录失败: {err}"))?;
        }
        let bak = backup_path(&self.path);
        let restored = self.restored_from_bak.load(AtomicOrdering::Relaxed);
        if !restored && self.path.exists() {
            fs::copy(&self.path, &bak).map_err(|err| format!("备份 database.json 失败: {err}"))?;
        }
        let tmp = PathBuf::from(format!(
            "{}.{}.tmp",
            self.path.display(),
            std::process::id()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if let Err(err) = result {
            let _ = fs::remove_file(&tmp);
            return Err(format!("写入 database.json 失败: {err}"));
        }
        self.restored_from_bak.store(false, AtomicOrdering::Relaxed);
        Ok(())
    }

    pub fn has_members(&self) -> bool {
        self.member_count() > 0
    }

    pub fn member_count(&self) -> usize {
        star_info(&self.read()).len()
    }

    /// `getAllMembers`：allmembers + 兼职档案 + 官方账号（缺字段一律空数组）。
    pub fn all_members(&self) -> Value {
        let data = self.read();
        json!({
            "allmembers": section(&data, "allmembers").to_vec(),
            "adjuncts": section(&data, "starAdjunctInfo").to_vec(),
            "officials": section(&data, "officialInfo").to_vec(),
        })
    }

    /// `starInfo` 段（成员主表，**带 `userId`**）。
    ///
    /// 与 `all_members()` 的 `allmembers` 段不同：那一段是首字母 / 生日之类的展示资料，
    /// 没有 `userId`，按 id 查名字必须走这里。
    pub fn star_members(&self) -> Vec<Value> {
        let data = self.read();
        section(&data, "starInfo").to_vec()
    }

    /// `getMemberInfo`：成员原始字段 + 派生的 `teamColor`（teamInfo 优先，成员自带兜底）。
    pub fn member_info(&self, user_id: i64) -> Option<Value> {
        let data = self.read();
        let member = star_info(&data)
            .iter()
            .find(|member| member_user_id(member).is_some_and(|id| id == user_id))?;
        let mut value = member.as_object()?.clone();
        value.insert("teamColor".into(), json!(member_team_color(&data, member)));
        Some(Value::Object(value))
    }

    /// `getMemberFlags`：按 id 解析成员，过滤已不存在的成员；字段与 Electron 版一致。
    pub fn member_flags(&self, kind: &str) -> Result<Value, String> {
        let key = flag_key(kind)?;
        let data = self.read();
        // 旧库只有 hiddenMemberIds 时按 Electron init() 的迁移规则只读映射；
        // 字段存在但为空数组表示「确实没有屏蔽」，不能误用旧字段。
        let ids = if key == "blockedMemberIds" && data.get(key).is_none() {
            section(&data, "hiddenMemberIds")
        } else {
            section(&data, key)
        };
        let mut resolved = Vec::new();
        for id in ids {
            let Some(member) = star_info(&data)
                .iter()
                .find(|member| id_eq(member.get("userId").unwrap_or(&Value::Null), id))
            else {
                continue;
            };
            let Some(mut value) = member.as_object().cloned() else {
                continue;
            };
            if let Some(user_id) = member_user_id(member) {
                value.insert("userId".into(), json!(user_id));
            }
            let real_name = member
                .get("realName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            value.insert("realName".into(), json!(real_name));
            let team_color = team_color_of(&data, member.get("teamId"));
            let color = if team_color.is_empty() {
                member
                    .get("teamColor")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            } else {
                team_color
            };
            value.insert("teamColor".into(), json!(color));
            resolved.push(Value::Object(value));
        }
        Ok(Value::Array(resolved))
    }

    /// 名单 id 轻量读法（未读检测等只需要数字 id，不必解析成员详情）。
    pub fn member_flag_ids(&self, kind: &str) -> Result<Vec<i64>, String> {
        let key = flag_key(kind)?;
        let data = self.read();
        let ids = if key == "blockedMemberIds" && data.get(key).is_none() {
            section(&data, "hiddenMemberIds")
        } else {
            section(&data, key)
        };
        Ok(ids
            .iter()
            .filter_map(id_num)
            .filter(|id| id.is_finite())
            .map(|id| id as i64)
            .collect())
    }

    /// `getConfig`：缺失 / 空串 / 非字符串 / 枚举脏值一律回落默认值，纯读不写盘。
    pub fn config(&self, key: &str) -> Result<Value, String> {
        let default = config_default(key).ok_or_else(|| format!("Invalid config key: {key}"))?;
        let data = self.read();
        let stored = data
            .get("config")
            .and_then(|config| config.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        if let Some(value) = stored {
            let allowed = config_enum(key);
            let valid = match allowed {
                Some(allowed) => allowed.contains(&value),
                None => true,
            };
            if valid {
                return Ok(json!(value));
            }
        }
        Ok(json!(default))
    }

    /// `setConfig`：键与枚举值都校验；脏值不落盘。
    pub fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        config_default(key).ok_or_else(|| format!("Invalid config key: {key}"))?;
        if let Some(allowed) = config_enum(key)
            && !allowed.contains(&value)
        {
            return Err(format!("Invalid config value for {key}: {value}"));
        }
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            let config = object
                .entry("config")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| "database.json config 不是对象".to_string())?;
            config.insert(key.to_string(), json!(value));
            Ok(true)
        })?;
        Ok(())
    }

    /// `setMemberFlags`：整组替换（不做去重，与 Electron 一致）。
    pub fn set_member_flags(&self, kind: &str, ids: &[Value]) -> Result<(), String> {
        let key = flag_key(kind)?;
        for id in ids {
            if !id.is_number() && !id.is_string() {
                return Err(format!("Invalid member id: {id}"));
            }
        }
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            object.insert(key.to_string(), Value::Array(ids.to_vec()));
            Ok(true)
        })?;
        Ok(())
    }

    /// `addMemberFlag` / `addMemberFlags`：数字归一后幂等合并，无变化不写盘。
    pub fn add_member_flags(&self, kind: &str, user_ids: &[i64]) -> Result<bool, String> {
        let key = flag_key(kind)?;
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            let list = object
                .entry(key)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| format!("{key} 不是数组"))?;
            let mut changed = false;
            for user_id in user_ids {
                if !list.iter().any(|id| id_eq(id, &json!(user_id))) {
                    list.push(json!(user_id));
                    changed = true;
                }
            }
            Ok(changed)
        })
    }

    pub fn add_member_flag(&self, kind: &str, user_id: i64) -> Result<(), String> {
        self.add_member_flags(kind, &[user_id]).map(|_| ())
    }

    /// `removeMemberFlag`：数字归一后过滤；记录不存在时不写盘。
    pub fn remove_member_flag(&self, kind: &str, user_id: i64) -> Result<(), String> {
        let key = flag_key(kind)?;
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            let Some(list) = object.get_mut(key).and_then(Value::as_array_mut) else {
                return Ok(false);
            };
            let before = list.len();
            list.retain(|id| !id_eq(id, &json!(user_id)));
            Ok(list.len() != before)
        })?;
        Ok(())
    }

    /// `saveMemberData`：只覆盖入参里出现的分节（空数组也算有效覆盖）。
    pub fn save_member_data(&self, content: &Value) -> Result<(), String> {
        let content = content
            .as_object()
            .ok_or_else(|| "成员数据不是对象".to_string())?;
        for (key, value) in content {
            if !MEMBER_SECTIONS.contains(&key.as_str()) {
                return Err(format!("Invalid member data section: {key}"));
            }
            if !value.is_array() {
                return Err(format!("成员数据分节 {key} 不是数组"));
            }
        }
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            for (key, value) in content {
                object.insert(key.clone(), value.clone());
            }
            Ok(true)
        })?;
        Ok(())
    }

    /// `getMemberTree`：starInfo + teamInfo + groupInfo → 团体/队伍/成员三层派生树。
    /// 与 Electron `buildMemberTree` 同规则：团体别名归一、按 groupSort/teamSort 排序、
    /// 叶子保留成员全量字段并覆盖归一后的 groupId/groupName/teamColor。
    pub fn member_tree(&self) -> Value {
        let data = self.read();
        let mut groups: Vec<GroupAcc> = Vec::new();

        for member in star_info(&data) {
            let raw_group_id = member.get("groupId").filter(|value| !value.is_null());
            let alias = raw_group_id.and_then(id_string).and_then(|id| {
                GROUP_ALIASES
                    .iter()
                    .find(|(from, _)| *from == id)
                    .map(|(_, to)| *to)
            });
            let group_id = alias.map(|id| json!(id)).or_else(|| raw_group_id.cloned());
            let group_name = alias
                .and_then(|id| group_name_of(&data, &json!(id)))
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    member
                        .get("groupName")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "未分组".to_string());
            let team_name = member
                .get("teamName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("未分队");

            let group_index = match groups
                .iter()
                .position(|group| group.group_name == group_name)
            {
                Some(index) => index,
                None => {
                    groups.push(GroupAcc {
                        group_id: None,
                        group_name: group_name.clone(),
                        teams: Vec::new(),
                    });
                    groups.len() - 1
                }
            };
            let group = &mut groups[group_index];
            if group.group_id.is_none() {
                group.group_id = group_id;
            }
            let team_index = match group
                .teams
                .iter()
                .position(|team| team.team_name == team_name)
            {
                Some(index) => index,
                None => {
                    group.teams.push(TeamAcc {
                        team_id: None,
                        team_name: team_name.to_string(),
                        members: Vec::new(),
                    });
                    group.teams.len() - 1
                }
            };
            let team = &mut group.teams[team_index];
            if team.team_id.is_none() {
                team.team_id = member
                    .get("teamId")
                    .filter(|value| !value.is_null())
                    .cloned();
            }
            team.members.push(member.clone());
        }

        groups.sort_by(|a, b| {
            cmp_f64(
                group_sort_of(&data, a.group_id.as_ref()),
                group_sort_of(&data, b.group_id.as_ref()),
            )
        });

        let mut output = Vec::new();
        for mut group in groups {
            let mut team_values = Vec::new();
            group.teams.sort_by(|a, b| {
                cmp_f64(
                    team_sort_of(&data, group.group_id.as_ref(), a.team_id.as_ref()),
                    team_sort_of(&data, group.group_id.as_ref(), b.team_id.as_ref()),
                )
            });
            for team in &group.teams {
                let mut leaves = Vec::new();
                for member in &team.members {
                    let Some(source) = member.as_object() else {
                        continue;
                    };
                    let mut leaf = source.clone();
                    leaf.insert(
                        "label".into(),
                        json!(
                            member
                                .get("realName")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                        ),
                    );
                    leaf.insert("value".into(), json!(js_string_opt(member.get("userId"))));
                    if let Some(group_id) = &group.group_id {
                        leaf.insert("groupId".into(), group_id.clone());
                    }
                    leaf.insert("groupName".into(), json!(group.group_name));
                    let team_color = team_color_of(&data, team.team_id.as_ref());
                    let color = if team_color.is_empty() {
                        member
                            .get("teamColor")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        team_color
                    };
                    leaf.insert("teamColor".into(), json!(color));
                    leaves.push(Value::Object(leaf));
                }
                let mut team_value = Map::new();
                team_value.insert("teamName".into(), json!(team.team_name));
                team_value.insert("label".into(), json!(team.team_name));
                team_value.insert("value".into(), json!(js_string_opt(team.team_id.as_ref())));
                team_value.insert(
                    "teamBadge".into(),
                    json!(team_badge_of(&data, team.team_id.as_ref())),
                );
                team_value.insert(
                    "teamColor".into(),
                    json!(team_color_of(&data, team.team_id.as_ref())),
                );
                team_value.insert("children".into(), Value::Array(leaves));
                team_values.push(Value::Object(team_value));
            }

            let mut group_value = Map::new();
            group_value.insert("groupName".into(), json!(group.group_name));
            if let Some(group_id) = &group.group_id {
                group_value.insert("groupId".into(), group_id.clone());
            }
            group_value.insert("label".into(), json!(group.group_name));
            group_value.insert(
                "value".into(),
                json!(js_string_opt(group.group_id.as_ref())),
            );
            group_value.insert("children".into(), Value::Array(team_values));
            output.push(Value::Object(group_value));
        }
        Value::Array(output)
    }

    /// `getMemberTeamPromotions`：只回该账号的升格，且不带 `ownerId`（渲染层不接触账号身份）。
    pub fn member_team_promotions(&self, owner_id: &str) -> Value {
        let data = self.read();
        let empty = Map::new();
        let list = promotions_of(data.as_object().unwrap_or(&empty));
        Value::Array(
            list.iter()
                .filter(|item| owner_of(item) == owner_id)
                .map(|item| {
                    json!({
                        "userId": item.get("userId").cloned().unwrap_or(Value::Null),
                        "teamName": item.get("teamName").cloned().unwrap_or(Value::Null),
                        "teamColor": item.get("teamColor").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect(),
        )
    }

    /// `setMemberTeamPromotion`：同一账号对同一成员写入 / 覆盖，只保留白名单字段。
    pub fn set_member_team_promotion(
        &self,
        promotion: &Value,
        owner_id: &str,
    ) -> Result<(), String> {
        let owner = owner_id.trim();
        if owner.is_empty() {
            return Err("Invalid member team promotion ownerId".to_string());
        }
        let user_id = js_num(promotion.get("userId"))
            .ok_or_else(|| "Invalid member team promotion: userId".to_string())?;
        let team_name = promotion
            .get("teamName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| "Invalid member team promotion: teamName".to_string())?;
        let team_color = promotion
            .get("teamColor")
            .and_then(Value::as_str)
            .ok_or_else(|| "Invalid member team promotion: teamColor".to_string())?
            .to_string();
        let record = json!({
            "ownerId": owner,
            "userId": user_id as i64,
            "teamName": team_name,
            "teamColor": team_color,
        });
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            let mut next: Vec<Value> = promotions_of(object)
                .iter()
                .filter(|item| {
                    !(owner_of(item) == owner && js_num(item.get("userId")) == Some(user_id))
                })
                .cloned()
                .collect();
            next.push(record.clone());
            object.insert("memberTeamPromotions".into(), Value::Array(next));
            Ok(true)
        })?;
        Ok(())
    }

    /// `removeMemberTeamPromotion`：只删当前账号的记录；记录不存在时不写盘。
    pub fn remove_member_team_promotion(&self, owner_id: &str, user_id: i64) -> Result<(), String> {
        let owner = owner_id.trim();
        if owner.is_empty() {
            return Err("Invalid member team promotion ownerId".to_string());
        }
        self.mutate(|data| {
            let object = data
                .as_object_mut()
                .ok_or_else(|| "database.json 根节点不是对象".to_string())?;
            let current = promotions_of(object);
            let next: Vec<Value> = current
                .iter()
                .filter(|item| {
                    !(owner_of(item) == owner && js_num(item.get("userId")) == Some(user_id as f64))
                })
                .cloned()
                .collect();
            if next.len() == current.len() {
                return Ok(false);
            }
            object.insert("memberTeamPromotions".into(), Value::Array(next));
            Ok(true)
        })?;
        Ok(())
    }
}

/// 升格表的原始数组（缺失 / 脏值一律空）。
fn promotions_of(data: &Map<String, Value>) -> &[Value] {
    data.get("memberTeamPromotions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn owner_of(item: &Value) -> &str {
    item.get("ownerId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
}

/// JS `Number(...)` 口径：数字直取，字符串（含空白）解析，其余 null。
fn js_num(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Some(0.0)
            } else {
                text.parse::<f64>().ok()
            }
        }
        Some(Value::Null) | None => Some(0.0),
        Some(_) => None,
    }
    .filter(|number| number.is_finite())
}

struct GroupAcc {
    group_id: Option<Value>,
    group_name: String,
    teams: Vec<TeamAcc>,
}

struct TeamAcc {
    team_id: Option<Value>,
    team_name: String,
    members: Vec<Value>,
}

fn normalize_root(value: Value) -> Value {
    if value.is_object() {
        value
    } else {
        default_data()
    }
}

fn default_data() -> Value {
    json!({
        "officialInfo": [],
        "domainInfo": [],
        "starInfo": [],
        "starAdjunctInfo": [],
        "starRepeatInfo": [],
        "groupInfo": [],
        "periodInfo": [],
        "teamInfo": [],
        "starOfficialRelationInfo": [],
        "allmembers": [],
    })
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

fn read_json(path: &Path) -> Option<Value> {
    let raw = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.is_object().then_some(value)
}

fn section<'a>(data: &'a Value, key: &str) -> &'a [Value] {
    data.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn star_info(data: &Value) -> &[Value] {
    section(data, "starInfo")
}

fn flag_key(kind: &str) -> Result<&'static str, String> {
    match kind {
        "blocked" => {
            // 旧库兼容：hiddenMemberIds → blockedMemberIds（只读映射，不写回）
            Ok("blockedMemberIds")
        }
        "followed" => Ok("followedMemberIds"),
        other => Err(format!("Invalid member flag kind: {other}")),
    }
}

fn config_default(key: &str) -> Option<&'static str> {
    match key {
        "downloadDirectory" => Some(""),
        "ffmpegDirectory" => Some(""),
        "danmakuPlacement" => Some("video"),
        "floatTopmost" => Some("playing"),
        _ => None,
    }
}

fn config_enum(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "danmakuPlacement" => Some(&["video", "left"]),
        "floatTopmost" => Some(&["always", "playing", "never"]),
        _ => None,
    }
}

fn id_num(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
}

fn id_eq(left: &Value, right: &Value) -> bool {
    match (id_num(left), id_num(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn member_user_id(member: &Value) -> Option<i64> {
    let value = id_num(member.get("userId")?)?;
    value.is_finite().then_some(value as i64)
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => "[object Object]".to_string(),
    }
}

/// 缺失字段等价于 JS `String(undefined)`；与显式 null 分开。
fn js_string_opt(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), js_string)
}

fn group_name_of(data: &Value, group_id: &Value) -> Option<String> {
    section(data, "groupInfo")
        .iter()
        .find(|group| id_eq(group.get("groupId").unwrap_or(&Value::Null), group_id))
        .and_then(|group| group.get("groupName"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn team_record<'a>(data: &'a Value, team_id: Option<&Value>) -> Option<&'a Value> {
    let team_id = team_id?;
    section(data, "teamInfo")
        .iter()
        .find(|team| id_eq(team.get("teamId").unwrap_or(&Value::Null), team_id))
}

fn team_color_of(data: &Value, team_id: Option<&Value>) -> String {
    team_record(data, team_id)
        .and_then(|team| team.get("teamColor"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn team_badge_of(data: &Value, team_id: Option<&Value>) -> String {
    team_record(data, team_id)
        .and_then(|team| team.get("seineTeamBadge"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn member_team_color(data: &Value, member: &Value) -> String {
    let color = team_color_of(data, member.get("teamId"));
    if color.is_empty() {
        member
            .get("teamColor")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        color
    }
}

fn group_sort_of(data: &Value, group_id: Option<&Value>) -> f64 {
    group_id
        .and_then(|group_id| {
            section(data, "groupInfo")
                .iter()
                .find(|group| id_eq(group.get("groupId").unwrap_or(&Value::Null), group_id))
        })
        .and_then(|group| group.get("groupSort"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX)
}

fn team_sort_of(data: &Value, group_id: Option<&Value>, team_id: Option<&Value>) -> f64 {
    let (Some(group_id), Some(team_id)) = (group_id, team_id) else {
        return f64::MAX;
    };
    section(data, "teamInfo")
        .iter()
        .find(|team| {
            id_eq(team.get("groupId").unwrap_or(&Value::Null), group_id)
                && id_eq(team.get("teamId").unwrap_or(&Value::Null), team_id)
        })
        .and_then(|team| team.get("teamSort"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX)
}

fn cmp_f64(left: f64, right: f64) -> Ordering {
    left.partial_cmp(&right).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "officialInfo": [{ "userId": 1000, "realName": "官方号" }],
            "allmembers": [{ "userId": 1, "nickname": "A" }],
            "starInfo": [
                {
                    "userId": 1, "realName": "A", "groupId": 19, "groupName": "明星殿堂",
                    "teamId": 1008, "teamName": "SNH48"
                },
                {
                    "userId": 2, "realName": "B", "groupId": 10, "groupName": "SNH48",
                    "teamId": 1008, "teamName": "SNH48"
                },
                {
                    "userId": 3, "realName": "C", "groupId": 11, "groupName": "GNZ48",
                    "teamId": 2001, "teamName": "GNZ48 Team G"
                }
            ],
            "starAdjunctInfo": [{ "adjunctId": 7, "userId": 1 }],
            "teamInfo": [
                { "teamId": 1008, "groupName": "SNH48", "teamColor": "#abcdef", "seineTeamBadge": "badge.png", "teamSort": 1 },
                { "teamId": 2001, "groupName": "GNZ48", "teamColor": "#123456", "teamSort": 2 }
            ],
            "groupInfo": [
                { "groupId": 10, "groupName": "SNH48", "groupSort": 1 },
                { "groupId": 11, "groupName": "GNZ48", "groupSort": 2 }
            ],
            "blockedMemberIds": [1, "3", 999],
            "followedMemberIds": [2],
            "hiddenMemberIds": [2],
            "config": {
                "danmakuPlacement": "left",
                "floatTopmost": "bogus",
                "downloadDirectory": ""
            }
        })
    }

    #[test]
    fn all_members_keeps_three_sections() {
        let db = Database::from_value(fixture());
        let all = db.all_members();
        assert_eq!(all["allmembers"].as_array().map(Vec::len), Some(1));
        assert_eq!(all["adjuncts"].as_array().map(Vec::len), Some(1));
        assert_eq!(all["officials"].as_array().map(Vec::len), Some(1));
        assert!(db.has_members());
    }

    #[test]
    fn member_info_adds_team_color() {
        let db = Database::from_value(fixture());
        let info = db.member_info(1).expect("成员存在");
        assert_eq!(info["userId"], json!(1));
        assert_eq!(info["teamColor"], json!("#abcdef"));
        assert!(db.member_info(999).is_none());
    }

    #[test]
    fn member_flags_filter_missing_and_normalize() {
        let db = Database::from_value(fixture());
        let blocked = db.member_flags("blocked").expect("blocked");
        assert_eq!(blocked.as_array().map(Vec::len), Some(2));
        assert_eq!(blocked[0]["userId"], json!(1));
        assert_eq!(blocked[1]["userId"], json!(3));
        assert_eq!(blocked[1]["teamColor"], json!("#123456"));
        let followed = db.member_flags("followed").expect("followed");
        assert_eq!(followed[0]["userId"], json!(2));
        assert!(db.member_flags("wat").is_err());
    }

    #[test]
    fn config_falls_back_for_empty_or_invalid_values() {
        let db = Database::from_value(fixture());
        assert_eq!(db.config("danmakuPlacement").unwrap(), json!("left"));
        // 已移除的键按「未知键」处理，不再返回旧默认值
        assert!(db.config("trayOnClose").is_err());
        assert_eq!(db.config("downloadDirectory").unwrap(), json!(""));
        assert_eq!(db.config("floatTopmost").unwrap(), json!("playing"));
        assert!(db.config("toString").is_err());
    }

    #[test]
    fn member_tree_aliases_and_sorts() {
        let db = Database::from_value(fixture());
        let tree = db.member_tree();
        let groups = tree.as_array().expect("树是数组");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["groupName"], json!("SNH48"));
        assert_eq!(groups[0]["groupId"], json!(10));
        let team = &groups[0]["children"][0];
        assert_eq!(team["teamName"], json!("SNH48"));
        assert_eq!(team["teamBadge"], json!("badge.png"));
        assert_eq!(team["teamColor"], json!("#abcdef"));
        let leaves = team["children"].as_array().expect("叶子");
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0]["groupId"], json!(10));
        assert_eq!(leaves[0]["groupName"], json!("SNH48"));
        assert_eq!(leaves[0]["teamColor"], json!("#abcdef"));
        assert_eq!(leaves[0]["value"], json!("1"));
        assert_eq!(groups[1]["groupName"], json!("GNZ48"));
    }

    fn temp_db(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "desktop48-db-write-{}-{}-{}",
            tag,
            std::process::id(),
            rand::random::<u16>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.json");
        (dir, path)
    }

    #[test]
    fn writes_are_atomic_backed_up_and_reloadable() {
        let (dir, path) = temp_db("rw");
        fs::write(&path, fixture().to_string()).unwrap();
        let db = Database::load(&path);

        db.set_config("danmakuPlacement", "left").unwrap();
        assert_eq!(db.config("danmakuPlacement").unwrap(), json!("left"));
        assert!(db.set_config("danmakuPlacement", "side").is_err());
        assert!(db.set_config("toString", "x").is_err());

        assert!(db.add_member_flags("blocked", &[1, 2]).unwrap());
        assert!(!db.add_member_flags("blocked", &[1]).unwrap());
        db.remove_member_flag("blocked", 3).unwrap();
        assert_eq!(
            db.member_flags("blocked").unwrap().as_array().map(Vec::len),
            Some(2)
        );

        db.save_member_data(&json!({ "starInfo": [{ "userId": 4, "realName": "D" }] }))
            .unwrap();
        assert_eq!(db.member_count(), 1);
        assert!(
            db.save_member_data(&json!({ "starInfo": "not-an-array" }))
                .is_err()
        );
        assert!(
            db.save_member_data(&json!({ "unknownSection": [] }))
                .is_err()
        );
        db.set_member_flags("followed", &[json!(4)]).unwrap();

        // 每次写盘前备份旧主文件；调换配置后 .bak 至少存在且可解析。
        assert!(backup_path(&path).exists());
        let reloaded = Database::load(&path);
        assert_eq!(reloaded.config("danmakuPlacement").unwrap(), json!("left"));
        assert_eq!(reloaded.member_count(), 1);
        assert_eq!(
            reloaded.member_flags("followed").unwrap()[0]["userId"],
            json!(4)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_write_after_backup_recovery_keeps_good_backup() {
        let (dir, path) = temp_db("recover");
        fs::write(&path, "{ broken").unwrap();
        fs::write(backup_path(&path), fixture().to_string()).unwrap();
        let db = Database::load(&path);
        assert!(db.has_members());
        db.set_config("floatTopmost", "never").unwrap();
        // 主文件已恢复为合法 JSON；.bak 仍是从 .bak 恢复出的好数据，而不是损坏内容。
        let main: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(main["config"]["floatTopmost"], json!("never"));
        let bak: Value =
            serde_json::from_str(&fs::read_to_string(backup_path(&path)).unwrap()).unwrap();
        assert_eq!(bak["starInfo"].as_array().map(Vec::len), Some(3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_falls_back_to_backup() {
        let dir = std::env::temp_dir().join(format!(
            "desktop48-db-{}-{}",
            std::process::id(),
            rand::random::<u16>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.json");
        fs::write(&path, "{ broken").unwrap();
        fs::write(backup_path(&path), fixture().to_string()).unwrap();
        let db = Database::load(&path);
        assert!(db.has_members());
        assert_eq!(db.path(), path);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 升格是写通道：必须落到临时目录的真实文件上（内存库不支持写入）。
    fn temp_promotion_db(tag: &str) -> (PathBuf, Database) {
        let (dir, path) = temp_db(tag);
        fs::write(&path, fixture().to_string()).unwrap();
        (dir, Database::load(&path))
    }

    #[test]
    fn promotions_are_owner_scoped() {
        let (dir, db) = temp_promotion_db("promo-owner");
        db.set_member_team_promotion(
            &json!({
                "userId": 1,
                "teamName": "SNH48 Team SII",
                "teamColor": "#6d5ae0",
                // 伪造的 ownerId 必须被丢弃
                "ownerId": "attacker",
                "groupId": 10
            }),
            "owner-a",
        )
        .unwrap();
        let mine = db.member_team_promotions("owner-a");
        assert_eq!(mine.as_array().unwrap().len(), 1);
        assert_eq!(mine[0]["teamName"], "SNH48 Team SII");
        assert!(mine[0].get("ownerId").is_none());
        assert!(mine[0].get("groupId").is_none());
        assert!(
            db.member_team_promotions("owner-b")
                .as_array()
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn promotion_set_overwrites_and_remove_is_idempotent() {
        let (dir, db) = temp_promotion_db("promo-overwrite");
        for name in ["A队", "B队"] {
            db.set_member_team_promotion(
                &json!({ "userId": 1, "teamName": name, "teamColor": "#fff" }),
                "owner-a",
            )
            .unwrap();
        }
        let list = db.member_team_promotions("owner-a");
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["teamName"], "B队");

        db.remove_member_team_promotion("owner-a", 1).unwrap();
        assert!(
            db.member_team_promotions("owner-a")
                .as_array()
                .unwrap()
                .is_empty()
        );
        // 记录不存在时不报错（与 Electron 一致）
        db.remove_member_team_promotion("owner-a", 1).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn promotion_rejects_bad_input() {
        let db = Database::from_value(fixture());
        assert!(
            db.set_member_team_promotion(&json!({ "teamName": "A", "teamColor": "" }), "owner-a")
                .is_err()
        );
        assert!(
            db.set_member_team_promotion(
                &json!({ "userId": 1, "teamName": "  ", "teamColor": "" }),
                "owner-a"
            )
            .is_err()
        );
        assert!(
            db.set_member_team_promotion(&json!({ "userId": 1, "teamName": "A" }), "owner-a")
                .is_err()
        );
        assert!(
            db.set_member_team_promotion(
                &json!({ "userId": 1, "teamName": "A", "teamColor": "" }),
                " "
            )
            .is_err()
        );
    }

    #[test]
    fn promotions_tolerate_dirty_records() {
        let mut value = fixture();
        value["memberTeamPromotions"] = json!([
            { "ownerId": "owner-a", "userId": 1, "teamName": "A队", "teamColor": "#111" },
            { "ownerId": "", "userId": 2, "teamName": "脏记录", "teamColor": "#222" },
            "not-an-object",
        ]);
        let db = Database::from_value(value);
        let list = db.member_team_promotions("owner-a");
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["userId"], 1);
    }
}
