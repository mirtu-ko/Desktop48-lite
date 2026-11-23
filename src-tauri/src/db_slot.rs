//! 成员库句柄：让「启动时装载」与「读路径」解耦。
//!
//! `database.json` 约 1.6 MB、近千成员，解析几十毫秒——放在启动路径上会拖慢首屏，
//! 但读接口遍布 `bridge.rs`，逐个改成 `await` 代价太大。折中：
//! 启动时先用一个**空库**顶上（`has_members() == false`，渲染层据此走「还没同步」的
//! 空态），后台线程解析真实库，完成后整体换入。
//!
//! 语义约束：
//! - 装载期间对空库的写入会被拒绝（见 [`DatabaseSlot::store`] 之后的 `Database::path` 为空）；
//! - 换入用 `RwLock` 的写锁，与正在跑的读请求互斥，不会读到半个库。

use std::sync::RwLock;

use crate::database::Database;

pub struct DatabaseSlot {
    /// `None` = 后台装载尚未完成。读路径落到空库。
    inner: RwLock<Option<Database>>,
}

impl DatabaseSlot {
    /// 空槽：读接口返回空库数据，写入会被 `Database` 的内存模式拒绝。
    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// 装好真实库的槽（测试 / 探针用）。
    pub fn ready(db: Database) -> Self {
        Self {
            inner: RwLock::new(Some(db)),
        }
    }

    /// 后台装载完成后换入真实库。
    pub fn store(&self, db: Database) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(db);
    }

    /// 装载是否完成。
    pub fn is_ready(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// 读路径：装载完成前把调用落到一个**空库**上。
    ///
    /// 空库每次现造（`Database::from_value` 很轻），所以不能像 `Deref` 那样返回引用；
    /// 调用点写成 `self.db.with(|db| db.has_members())`。
    pub fn with<T>(&self, f: impl FnOnce(&Database) -> T) -> T {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(db) => f(db),
            None => f(&Database::from_value(serde_json::Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_slot_reads_as_empty_database() {
        let slot = DatabaseSlot::empty();
        assert!(!slot.is_ready());
        assert!(!slot.with(|db| db.has_members()));
        assert_eq!(slot.with(|db| db.member_count()), 0);
    }

    #[test]
    fn store_swaps_in_real_database() {
        let slot = DatabaseSlot::empty();
        let mut value = json!({ "starInfo": [{ "userId": 1, "realName": "测试" }] });
        value["config"] = json!({});
        slot.store(Database::from_value(value));
        assert!(slot.is_ready());
        assert!(slot.with(|db| db.has_members()));
        assert_eq!(
            slot.with(|db| db.member_info(1)).unwrap()["realName"],
            json!("测试")
        );
    }
}
