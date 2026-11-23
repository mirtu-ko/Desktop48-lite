//! 旧 Electron 数据的一次性迁移。
//!
//! 只搬「不需要重新登录就能复用」的数据：
//! - `database.json`（成员库 + 配置，配置就在这个文件的 `config` 字段里）
//! - `database.json.bak`（上一版已知良好数据，目标库损坏时 Electron 版的回退副本）
//! - `pocket48-device.json`（设备 ID：复用后服务端不会把用户当成新安装）
//!
//! **不搬** `pocket48-auth.json`：token 由 Electron `safeStorage` 加密，
//! 跨实现无法解密（见迁移计划 §2.3 的凭证方案），首版引导重新登录即可。
//! 也不搬 `pocket48-unread.json` 等与登录态绑定的运行态文件。
//!
//! 迁移幂等、只读旧目录、目标已存在则绝不覆盖，并留一份 `.legacy-migration.json` 记录。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const LEGACY_APP_DIR: &str = "Desktop48";
const MARKER_FILE: &str = ".legacy-migration.json";

const DEVICE_FILE: &str = "pocket48-device.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    /// 本次是否真的复制了文件（复查 / 无源目录时为 false）。
    pub migrated: bool,
    pub source_dir: String,
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 旧 Electron 版的数据目录（`app.getPath('userData')` 对应位置）。
pub fn legacy_data_dir() -> Option<PathBuf> {
    crate::platform::legacy_data_dir(LEGACY_APP_DIR)
}

/// 启动时调用：把旧目录数据搬进 Tauri 的 app data 目录（`data_dir`）。
pub fn migrate_legacy_data(data_dir: &Path) -> MigrationReport {
    let Some(source) = legacy_data_dir() else {
        return skip("", "无法定位旧版数据目录");
    };
    migrate_from(data_dir, &source)
}

/// 可注入源目录的迁移实现（单测用）。
pub fn migrate_from(data_dir: &Path, source: &Path) -> MigrationReport {
    let source_label = source.display().to_string();
    if !source.is_dir() {
        return MigrationReport {
            migrated: false,
            source_dir: source_label,
            files: Vec::new(),
            note: Some("未发现旧版数据目录（全新安装）".to_string()),
        };
    }

    let marker = data_dir.join(MARKER_FILE);
    if marker.exists() {
        return MigrationReport {
            migrated: false,
            source_dir: source_label,
            files: Vec::new(),
            note: Some("此前已完成迁移".to_string()),
        };
    }

    // 目标已有数据库：不覆盖（可能是用户已在新版里更新过数据）。
    if data_dir.join("database.json").exists() {
        return MigrationReport {
            migrated: false,
            source_dir: source_label,
            files: Vec::new(),
            note: Some("目标目录已有数据库，跳过迁移".to_string()),
        };
    }

    // 主库损坏时用 .bak（与 Electron 的 SafeJSONFileSync 回退顺序一致）。
    let database = match read_valid_object(&source.join("database.json")) {
        Some(bytes) => Some(("database.json", bytes)),
        None => read_valid_object(&source.join("database.json.bak"))
            .map(|bytes| ("database.json.bak", bytes)),
    };

    let Some((from, database_bytes)) = database else {
        return MigrationReport {
            migrated: false,
            source_dir: source_label,
            files: Vec::new(),
            note: Some("旧版数据库不存在或无法解析，未迁移".to_string()),
        };
    };

    if let Err(err) = fs::create_dir_all(data_dir) {
        return MigrationReport {
            migrated: false,
            source_dir: source_label,
            files: Vec::new(),
            note: Some(format!("创建目标目录失败: {err}")),
        };
    }

    let mut files = Vec::new();
    match write_atomic(&data_dir.join("database.json"), &database_bytes) {
        Ok(()) => {
            files.push("database.json".to_string());
            if from != "database.json" {
                eprintln!("[migrate] 旧版主库不可解析，已从 .bak 恢复");
            }
        }
        Err(err) => {
            return MigrationReport {
                migrated: false,
                source_dir: source_label,
                files: Vec::new(),
                note: Some(format!("写入 database.json 失败: {err}")),
            };
        }
    }

    // .bak 只在自身可解析时复制，避免把损坏副本带进新目录。
    if !files.is_empty()
        && from == "database.json"
        && let Some(bytes) = read_valid_object(&source.join("database.json.bak"))
        && write_atomic(&data_dir.join("database.json.bak"), &bytes).is_ok()
    {
        files.push("database.json.bak".to_string());
    }

    // 设备 ID 单独判断：目标已存在则不覆盖（理论上前面已拦截，但保持幂等）。
    let device_target = data_dir.join(DEVICE_FILE);
    if !device_target.exists()
        && let Some(bytes) = read_valid_object(&source.join(DEVICE_FILE))
        && write_atomic(&device_target, &bytes).is_ok()
    {
        files.push(DEVICE_FILE.to_string());
    }

    let marker_body = serde_json::json!({
        "sourceDir": source_label,
        "migratedAt": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "files": files,
        "skipped": ["pocket48-auth.json（Electron safeStorage 加密，需重新登录）"],
    });
    let _ = fs::write(&marker, marker_body.to_string());

    eprintln!(
        "[migrate] 已从 {} 迁移 {} 个文件: {}",
        source_label,
        files.len(),
        files.join(", ")
    );

    MigrationReport {
        migrated: true,
        source_dir: source_label,
        files,
        note: None,
    }
}

fn skip(source_dir: &str, note: &str) -> MigrationReport {
    MigrationReport {
        migrated: false,
        source_dir: source_dir.to_string(),
        files: Vec::new(),
        note: Some(note.to_string()),
    }
}

/// 读文件并要求是 JSON 对象：宁可不迁移，也不把坏数据放进新目录。
fn read_valid_object(path: &Path) -> Option<Vec<u8>> {
    let bytes = fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.is_object().then_some(bytes)
}

/// 原子写：临时文件 + rename，避免迁移中断留下半截数据。
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("非法目标路径: {}", target.display()))?;
    let tmp = target.with_file_name(format!("{file_name}.migrating"));
    fs::write(&tmp, bytes).map_err(|err| format!("写入临时文件失败: {err}"))?;
    fs::rename(&tmp, target).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("替换目标文件失败: {err}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("d48-migrate-{tag}-{unique}"))
    }

    fn write_json(path: &Path, value: serde_json::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
    }

    #[test]
    fn copies_database_and_device_id() {
        let source = temp_dir("src");
        let target = temp_dir("dst");
        write_json(
            &source.join("database.json"),
            json!({ "config": { "danmakuPlacement": "video" }, "starInfo": [] }),
        );
        write_json(&source.join("database.json.bak"), json!({ "config": {} }));
        write_json(
            &source.join("pocket48-device.json"),
            json!({ "deviceId": "0123456789abcdef" }),
        );

        let report = migrate_from(&target, &source);
        assert!(report.migrated);
        assert!(report.files.contains(&"database.json".to_string()));
        assert!(report.files.contains(&"pocket48-device.json".to_string()));
        assert!(target.join(".legacy-migration.json").exists());
        assert_eq!(
            fs::read_to_string(target.join("pocket48-device.json")).unwrap(),
            "{\"deviceId\":\"0123456789abcdef\"}"
        );

        // 幂等：第二次不再复制。
        let second = migrate_from(&target, &source);
        assert!(!second.migrated);
        assert!(second.note.is_some());

        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn falls_back_to_bak_when_main_is_corrupt() {
        let source = temp_dir("src-bak");
        let target = temp_dir("dst-bak");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("database.json"), "{ broken").unwrap();
        write_json(&source.join("database.json.bak"), json!({ "config": {} }));

        let report = migrate_from(&target, &source);
        assert!(report.migrated);
        assert!(read_valid_object(&target.join("database.json")).is_some());
    }

    #[test]
    fn does_not_overwrite_existing_target_database() {
        let source = temp_dir("src-exist");
        let target = temp_dir("dst-exist");
        write_json(&source.join("database.json"), json!({ "starInfo": [1] }));
        write_json(&target.join("database.json"), json!({ "starInfo": [] }));

        let report = migrate_from(&target, &source);
        assert!(!report.migrated);
        assert_eq!(
            fs::read_to_string(target.join("database.json")).unwrap(),
            "{\"starInfo\":[]}"
        );
    }

    #[test]
    fn legacy_dir_points_at_electron_app_folder() {
        let dir = legacy_data_dir();
        if let Some(dir) = dir {
            assert!(dir.ends_with(LEGACY_APP_DIR));
        }
    }

    #[test]
    fn missing_source_is_not_an_error() {
        let target = temp_dir("dst-missing");
        let source = temp_dir("src-missing");
        let report = migrate_from(&target, &source);
        assert!(!report.migrated);
        assert!(!target.exists());
    }
}
