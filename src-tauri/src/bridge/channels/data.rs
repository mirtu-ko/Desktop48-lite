//! 成员库与配置：`database.json` 的读写通道 + 升格名单。
//!
//! 读通道在成员库装载完成前落到空库（见 `db_slot`）；升格的 owner 只由登录态推导。
//! 事件与参数口径与 `src/bridge.ts` 的通道表一致；`mod.rs` 的 `dispatch_from` 只做权限收敛与转调。

use serde_json::{Value, json};

use super::super::{BridgeContext, arg};

pub(crate) async fn dispatch(
    ctx: &BridgeContext,
    channel: &str,
    args: &[Value],
    _caller: Option<&tauri::WebviewWindow>,
) -> Result<Value, String> {
    match channel {
        // 成员库已由 迁移；本段只暴露读通道，写通道随后补齐。
        "hasMembers" => Ok(json!(ctx.db.with(|db| db.has_members()))),
        "getAllMembers" => Ok(ctx.db.with(|db| db.all_members())),
        "getMemberInfo" => {
            let user_id: i64 = arg(args, 0)?;
            Ok(ctx
                .db
                .with(|db| db.member_info(user_id).unwrap_or(Value::Null)))
        }
        "getMemberFlags" => {
            let kind: String = arg(args, 0)?;
            ctx.db.with(|db| db.member_flags(&kind))
        }
        "getConfig" => {
            let key: String = arg(args, 0)?;
            ctx.db.with(|db| db.config(&key))
        }
        "getMemberTree" => Ok(ctx.db.with(|db| db.member_tree())),

        // 写通道：原子写 + 写前备份；先改副本，落盘成功才替换内存。
        "setConfig" => {
            let key: String = arg(args, 0)?;
            let value: String = arg(args, 1)?;
            ctx.db.with(|db| db.set_config(&key, &value))?;
            Ok(Value::Null)
        }
        "setMemberFlags" => {
            let kind: String = arg(args, 0)?;
            let ids: Vec<Value> = arg(args, 1)?;
            ctx.db.with(|db| db.set_member_flags(&kind, &ids))?;
            Ok(Value::Null)
        }
        "addMemberFlag" => {
            let kind: String = arg(args, 0)?;
            let user_id: i64 = arg(args, 1)?;
            ctx.db.with(|db| db.add_member_flag(&kind, user_id))?;
            Ok(Value::Null)
        }
        "addMemberFlags" => {
            let kind: String = arg(args, 0)?;
            let user_ids: Vec<i64> = arg(args, 1)?;
            ctx.db.with(|db| db.add_member_flags(&kind, &user_ids))?;
            Ok(Value::Null)
        }
        "removeMemberFlag" => {
            let kind: String = arg(args, 0)?;
            let user_id: i64 = arg(args, 1)?;
            ctx.db.with(|db| db.remove_member_flag(&kind, user_id))?;
            Ok(Value::Null)
        }
        "saveMemberData" => {
            let content: Value = arg(args, 0)?;
            ctx.db.with(|db| db.save_member_data(&content))?;
            Ok(json!({ "ok": true }))
        }

        // ===== 升格：ownerId 只由 Rust 的登录态推导（对照 register-database-ipc.ts） =====
        "getMemberTeamPromotions" => match ctx.promotion_owner_id() {
            Some(owner) => Ok(ctx.db.with(|db| db.member_team_promotions(&owner))),
            None => Ok(json!([])),
        },
        "setMemberTeamPromotion" => {
            let promotion: Value = arg(args, 0)?;
            let owner = ctx.require_promotion_owner()?;
            ctx.db
                .with(|db| db.set_member_team_promotion(&promotion, &owner))?;
            Ok(Value::Null)
        }
        "removeMemberTeamPromotion" => {
            let user_id: i64 = arg(args, 0)?;
            let owner = ctx.require_promotion_owner()?;
            ctx.db
                .with(|db| db.remove_member_team_promotion(&owner, user_id))?;
            Ok(Value::Null)
        }

        other => Err(format!("未实现的通道: {other}")),
    }
}
