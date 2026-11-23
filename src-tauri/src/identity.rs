//! 口袋48 客户端身份的 Rust 实现（对照 Electron 版 `src/main/pocket48/client-profile.ts`）。
//!
//! 只保留 Android 档「口袋48」身份：服务端按**客户端身份**而非出口 IP
//! 判定（站点 UA 会被 403），因此所有业务接口必须带同一套 UA + appInfo。
//! 设备 ID 落盘复用，避免每次启动都被当成一次重装。

use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 伪装机型，与 Electron 版 `src/common/android-device.ts` 保持同源。
const DEVICE_MODEL: &str = "23013RK75C";
const DEVICE_BRAND: &str = "Xiaomi";
const DEVICE_OS_VERSION: &str = "14";
const DEVICE_BUILD_ID: &str = "UKQ1.231003.002";
const DEVICE_BUILD_TAGS: &str = "release-keys";

/// 客户端身份（与 `src/common/pocket-client-profile.ts` 同源）。
///
/// **本项目只保留「口袋48」这一档**：原版还有 `seine48`（塞纳河48）可选，
/// 两套身份共用账号体系与接口，只影响请求头伪装，维护双档只增加回归面积，
/// 故收敛为单一身份。详见 README 的「刻意不做」。
pub const PROFILE_POCKET48: &str = "pocket48";

/// 身份恒为口袋48：旧凭证里写着 `seine48` 也一律按本档处理。
pub fn resolve_client_profile(_stored: Option<&str>) -> &'static str {
    PROFILE_POCKET48
}

/// 口袋48 7.1.44 APK 的客户端特征。
const POCKET_APP_NAME: &str = "pocket48";
const POCKET_APP_VERSION: &str = "7.1.44";
const POCKET_APP_BUILD: &str = "26092001";

/// App 档 UA：`PocketFans201807/<版本>_<build> (<model>:Android <os>;<brand> <buildId> <tags>)`。
/// 设备段与 appInfo 同源（同一组常量），不能各写一份。
fn user_agent() -> String {
    format!(
        "PocketFans201807/{POCKET_APP_VERSION}_{POCKET_APP_BUILD} ({DEVICE_MODEL}:Android {DEVICE_OS_VERSION};{DEVICE_BRAND} {DEVICE_BUILD_ID} {DEVICE_BUILD_TAGS})"
    )
}

/// 站点身份 UA（对照 Electron `SITE_USER_AGENT`）：诊断的「站点身份对照」行用。
pub fn site_user_agent() -> String {
    format!(
        "Mozilla/5.0 (Linux; Android {DEVICE_OS_VERSION}; {DEVICE_MODEL}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Mobile Safari/537.36"
    )
}

const DEVICE_FILE_NAME: &str = "pocket48-device.json";

#[derive(Debug, Serialize, Deserialize)]
struct StoredDevice {
    #[serde(rename = "deviceId")]
    device_id: String,
}

/// Android 档 IMEI 形状：16 位小写 hex。与 Electron 版同口径。
fn create_device_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let state = std::collections::hash_map::RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

fn is_valid_device_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn device_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DEVICE_FILE_NAME)
}

/// 读取已有设备 ID；缺失、损坏、形状不合法（手改/旧格式）都重建并写回。
/// 落盘失败不阻断请求：本次会话仍复用同一 ID，下次启动再试。
pub fn load_or_create_device_id(data_dir: &Path) -> String {
    let file = device_file_path(data_dir);

    if let Ok(raw) = fs::read_to_string(&file)
        && let Ok(stored) = serde_json::from_str::<StoredDevice>(&raw)
        && is_valid_device_id(&stored.device_id)
    {
        return stored.device_id;
    }

    let device_id = create_device_id();
    if let Err(err) = fs::create_dir_all(data_dir) {
        eprintln!("[identity] 设备目录创建失败: {err}");
        return device_id;
    }
    if let Err(err) = fs::write(
        &file,
        serde_json::to_vec(&StoredDevice {
            device_id: device_id.clone(),
        })
        .unwrap_or_default(),
    ) {
        eprintln!("[identity] 设备 ID 落盘失败: {err}");
    }
    device_id
}

/// appInfo 头的内容。键顺序与上游实测回包一致，不要改成 hash map。
fn app_info_json_for(device_id: &str) -> String {
    serde_json::json!({
        "IMEI": device_id,
        "appBuild": POCKET_APP_BUILD,
        "appName": POCKET_APP_NAME,
        "appVersion": POCKET_APP_VERSION,
        "deviceId": device_id,
        "deviceName": DEVICE_MODEL,
        "osType": "android",
        "osVersion": DEVICE_OS_VERSION,
        "phoneName": DEVICE_MODEL,
        "phoneSystemVersion": DEVICE_OS_VERSION,
        "vendor": DEVICE_BRAND,
    })
    .to_string()
}

/// 官方 App 的静态 `pa` 签名（对照 Electron 版 `POCKET_PA`）。
///
/// 正式版要像 Electron 那样由 `resources/pocket48/2.wasm` 现算（含当前时间戳），
/// 这里先落静态值保证头集合完整；动态签名在账号域接入。
pub const POCKET_PA: &str = "MTYzNjAxNzMxNTAwMCw3NjkyNzVkNTliZWU0MmQ0OGVjOWIyYTMwY2FkOWQzOSw3NzFhY2QwY2JiZDhkZDUzZDdjZGY3YTcxNGYzNmVlOCwyMDIxMDYwOTAx";

/// 请求头集合：(header, value)。调用方（reqwest）逐条设置。
///
/// 键名统一小写：`HeaderName::from_static` 只接受小写形态；HTTP 头大小写不敏感，
/// 服务端收到的键名不因此改变。
pub fn identity_headers(device_id: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "content-type",
            "application/json; charset=UTF-8".to_string(),
        ),
        ("user-agent", user_agent()),
        ("appinfo", app_info_json_for(device_id)),
        ("accept-language", "zh-Hans-CN;q=1".to_string()),
        ("pa", POCKET_PA.to_string()),
    ]
}

/// 机型串，`get_app_info` 展示用。
pub fn device_label() -> String {
    format!("{DEVICE_BRAND} {DEVICE_MODEL} · Android {DEVICE_OS_VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_matches_electron_version() {
        assert_eq!(
            user_agent(),
            "PocketFans201807/7.1.44_26092001 (23013RK75C:Android 14;Xiaomi UKQ1.231003.002 release-keys)"
        );
    }

    #[test]
    fn any_stored_profile_resolves_to_pocket48() {
        // 旧凭证写着 seine48 也按口袋48 处理，避免被旧档位锁住
        assert_eq!(resolve_client_profile(Some("seine48")), PROFILE_POCKET48);
        assert_eq!(resolve_client_profile(Some("pocket48")), PROFILE_POCKET48);
        assert_eq!(resolve_client_profile(None), PROFILE_POCKET48);
    }

    #[test]
    fn headers_no_longer_carry_seine_specific_field() {
        let headers = identity_headers("0123456789abcdef");
        assert!(
            !headers
                .iter()
                .any(|(name, _)| *name == "x-custom-device-type"),
            "seine48 专属头不应再发送"
        );
        assert!(headers.iter().any(|(name, _)| *name == "pa"));
    }

    #[test]
    fn device_id_shape_matches_android_imei() {
        for _ in 0..20 {
            let id = create_device_id();
            assert!(is_valid_device_id(&id), "{id} 不是 16 位小写 hex");
        }
    }

    #[test]
    fn rejects_invalid_stored_ids() {
        assert!(!is_valid_device_id(""));
        assert!(!is_valid_device_id("0123456789ABCDE")); // 15 位
        assert!(!is_valid_device_id("0123456789ABCDEF")); // 大写
        assert!(!is_valid_device_id("0123456789abcdeg"));
        assert!(is_valid_device_id("0123456789abcdef"));
    }

    #[test]
    fn device_id_is_persisted_and_reused() {
        let dir = std::env::temp_dir().join(format!("d48-identity-{}", create_device_id()));
        let first = load_or_create_device_id(&dir);
        let second = load_or_create_device_id(&dir);
        assert_eq!(first, second);
        assert!(device_file_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
