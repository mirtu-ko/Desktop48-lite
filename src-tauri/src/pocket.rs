//! 口袋48 账号域（对照 Electron `main/pocket48/pocket-auth.ts`）。
//!
//! - **token 不落明文**：优先存系统钥匙串；不可用时退回 `pocket48-auth.json` 的 `tokenPlain`
//!   并置 `plainFallback=true`。
//! - 凭证文件只存展示用资料，磁盘是唯一事实来源。
//! - token 失效即清空凭证并广播 `pocketAuthExpired`。
//!
//! 旧版 `safeStorage` 密文无法跨实现解密，因此不导入旧凭证：首次启动需重新登录或粘贴 token。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tauri::{AppHandle, Emitter};

use crate::identity;

const ORIGIN: &str = "https://pocketapi.48.cn";
const SMS_SEND_URL: &str = "https://pocketapi.48.cn/user/api/v2/sms/send_sms";
const SMS_LOGIN_URL: &str = "https://pocketapi.48.cn/user/api/v2/login/app/app_login";
const TOKEN_CHECK_URL: &str = "https://pocketapi.48.cn/user/api/v1/user/info/reload";
const USER_MONEY_URL: &str = "https://pocketapi.48.cn/user/api/v1/user/money";
const CHECKIN_URL: &str = "https://pocketapi.48.cn/user/api/v1/checkin";
const FRIENDS_IDS_URL: &str = "https://pocketapi.48.cn/user/api/v1/friendships/friends/id";
const FRIEND_ADD_URL: &str = "https://pocketapi.48.cn/user/api/v2/friendships/friends/add";
const FRIEND_REMOVE_URL: &str = "https://pocketapi.48.cn/user/api/v2/friendships/friends/remove";
const IM_USER_INFO_URL: &str = "https://pocketapi.48.cn/im/api/v1/im/userinfo";
const LIVE_ONE_URL: &str = "https://pocketapi.48.cn/live/api/v1/live/getLiveOne";
const LIVE_RESULT_URL: &str = "https://pocketapi.48.cn/live/api/v1/live/result";

const AUTH_FILE_NAME: &str = "pocket48-auth.json";
/// 钥匙串条目：service 用应用标识，account 固定，一个安装只有一份登录态。
const KEYRING_SERVICE: &str = "com.desktop48.lite.pocket48";
const KEYRING_ACCOUNT: &str = "pocket48-auth";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN_INVALID_STATUSES: [i64; 3] = [401003, 401004, 401005];
/// 反爬问答题：`message` 为内嵌 JSON `{question, answer:[...]}`。
const CAPTCHA_STATUS: i64 = 2001;
const FOLLOW_TO_TYPE_MEMBER: i64 = 1;

// ===== 钥匙串抽象：生产走系统钥匙串，单测走内存，避免触摸真实钥匙串 =====

pub trait TokenVault: Send + Sync {
    fn save(&self, token: &str) -> Result<(), String>;
    fn load(&self) -> Result<Option<String>, String>;
    fn delete(&self) -> Result<(), String>;
}

/// macOS Keychain / Linux Secret Service / Windows Credential Manager。
pub struct KeyringVault;

impl TokenVault for KeyringVault {
    fn save(&self, token: &str) -> Result<(), String> {
        entry()?
            .set_password(token)
            .map_err(|err| format!("写入系统钥匙串失败: {err}"))
    }

    fn load(&self) -> Result<Option<String>, String> {
        match entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(format!("读取系统钥匙串失败: {err}")),
        }
    }

    fn delete(&self) -> Result<(), String> {
        match entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(format!("删除系统钥匙串条目失败: {err}")),
        }
    }
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|err| format!("系统钥匙串不可用: {err}"))
}

/// **开发期专用**：强制走明文降级。
///
/// macOS Keychain 的 ACL 绑定创建该条目的二进制，debug 构建每次重编译都换 cdhash，
/// 于是每次都弹密码框。开发期走已有的明文降级路径即可，正式版仍用 `KeyringVault`。
pub struct PlainOnlyVault;

impl TokenVault for PlainOnlyVault {
    fn save(&self, _token: &str) -> Result<(), String> {
        Err("开发模式不写入系统钥匙串".to_string())
    }

    fn load(&self) -> Result<Option<String>, String> {
        Err("开发模式不读取系统钥匙串".to_string())
    }

    fn delete(&self) -> Result<(), String> {
        Ok(())
    }
}

/// 单测 / 探针用：不写系统钥匙串。
#[derive(Default)]
pub struct MemoryVault {
    token: Mutex<Option<String>>,
}

impl TokenVault for MemoryVault {
    fn save(&self, token: &str) -> Result<(), String> {
        *self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.to_string());
        Ok(())
    }

    fn load(&self) -> Result<Option<String>, String> {
        Ok(self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    fn delete(&self) -> Result<(), String> {
        *self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }
}

// ===== 对外结构（字段名与 Electron `ipc-contract.d.ts` 一致） =====

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PocketAuthState {
    pub logged_in: bool,
    pub nickname: String,
    pub avatar: String,
    pub user_id: String,
    pub mobile_masked: String,
    pub client_profile: String,
    /// true = 系统钥匙串不可用，token 以明文落盘（仅提示用）
    pub plain_fallback: bool,
    pub saved_at: u64,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PocketSmsResult {
    pub need_captcha: bool,
    pub question: String,
    pub options: Vec<String>,
    pub message: String,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PocketVerifyResult {
    pub valid: bool,
    pub nickname: String,
    pub user_id: String,
    pub level: String,
    pub im_ok: bool,
    pub im_accid: String,
    pub money_ok: bool,
    pub money_pay: f64,
    pub money_free: f64,
    pub money_total: f64,
    pub support: String,
    pub fans: String,
    pub likes: String,
    pub signature: String,
    pub message: String,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PocketCheckInResult {
    pub add_exp: f64,
    pub add_money: f64,
    pub add_support: f64,
    pub days: f64,
}

// ===== 凭证文件 =====

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct StoredAuth {
    #[serde(default)]
    mobile: String,
    #[serde(default)]
    area: String,
    #[serde(default = "default_profile")]
    client_profile: String,
    /// 仅当系统钥匙串不可用时才有值。
    #[serde(default)]
    token_plain: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    avatar: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    saved_at: u64,
}

fn default_profile() -> String {
    identity::PROFILE_POCKET48.to_string()
}

#[derive(Default)]
struct Inner {
    stored: Option<StoredAuth>,
    token: String,
    /// 凭证状态是否已从钥匙串 / 磁盘装载完成。装载前 `logged_in` 恒为 false，
    /// 渲染层据此区分「真没登录」与「还没读完」。
    loaded: bool,
}

pub struct PocketAccount {
    dir: PathBuf,
    device_id: String,
    http: reqwest::Client,
    vault: Box<dyn TokenVault>,
    inner: Mutex<Inner>,
    app: OnceLock<AppHandle>,
}

impl PocketAccount {
    /// 构造并读取凭证：**同步读钥匙串**，阻塞到用户做出选择为止。
    ///
    /// 这是启动流程里的第一个交互点：系统会弹「是否允许访问钥匙串」，
    /// 用户同意就拿到 token，拒绝则降级为未登录（两种结果都返回 `Self`，启动继续）。
    /// 因此必须在**显示主窗之前**调用，用户看到的第一屏才是应用，而不是一个等待中的空窗口。
    pub fn load(
        dir: PathBuf,
        device_id: String,
        http: reqwest::Client,
        vault: Box<dyn TokenVault>,
    ) -> Self {
        let stored = read_auth(&dir);
        let account = Self {
            dir,
            device_id,
            http,
            vault,
            inner: Mutex::new(Inner {
                stored,
                token: String::new(),
                loaded: false,
            }),
            app: OnceLock::new(),
        };
        account.unlock_vault();
        account
    }

    /// 读钥匙串里的 token（或把历史遗留的明文迁回钥匙串），完成后标记已装载。
    ///
    /// 阻塞调用：钥匙串被锁 / 首次授权时会一直等到用户在系统弹窗上做出选择。
    /// 用户拒绝时返回未登录，不算启动失败。
    pub fn unlock_vault(&self) {
        let started = std::time::Instant::now();
        let mut stored = read_auth(&self.dir);
        let mut token = String::new();
        if let Some(auth) = stored.as_ref() {
            if auth.token_plain.is_empty() {
                match self.vault.load() {
                    Ok(Some(value)) => token = value,
                    Ok(None) => {}
                    Err(err) => eprintln!("[pocket] 读取钥匙串失败（按未登录处理）: {err}"),
                }
            } else {
                // 历史遗留的明文 token：先试着迁回钥匙串，成功才清掉明文。
                // 迁移失败（钥匙串不可用）就继续用明文——这正是降级路径的语义，
                // 不能因为迁移失败让用户掉登录。
                let plain = auth.token_plain.clone();
                match self.vault.save(&plain) {
                    Ok(()) => {
                        token = plain;
                        if let Some(auth) = stored.as_mut() {
                            auth.token_plain = String::new();
                        }
                        if let Some(auth) = stored.as_ref() {
                            if let Err(err) = write_auth(&self.dir.join(AUTH_FILE_NAME), auth) {
                                eprintln!("[pocket] 明文凭证迁移后回写失败: {err}");
                            } else {
                                eprintln!("[pocket] 已把明文 token 迁回系统钥匙串");
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("[pocket] 明文 token 迁回钥匙串失败，继续用明文: {err}");
                        token = plain;
                    }
                }
            }
        }
        {
            let mut inner = self.inner();
            inner.stored = stored;
            inner.token = token;
            inner.loaded = true;
        }
        crate::app_log::log_line(&format!(
            "[pocket] 凭证已就绪（{}）：{}ms",
            if self.authorized() {
                "已登录"
            } else {
                "未登录"
            },
            started.elapsed().as_millis()
        ));
    }

    /// 凭证状态是否已装载完成（false = 还在后台读钥匙串）。
    pub fn is_loaded(&self) -> bool {
        self.inner().loaded
    }

    /// 已装载且持有 token。
    ///
    /// 所有「需要登录才能做」的路径都用它，而不是 `auth_state().logged_in`：
    /// 后者在装载完成前恒为 false，直接用会闪一下未登录（比如弹幕进房）。
    pub fn authorized(&self) -> bool {
        let inner = self.inner();
        inner.loaded && !inner.token.is_empty()
    }

    pub fn set_app_handle(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn auth_path(&self) -> PathBuf {
        self.dir.join(AUTH_FILE_NAME)
    }

    pub fn auth_state(&self) -> PocketAuthState {
        let inner = self.inner();
        let stored = inner.stored.as_ref();
        PocketAuthState {
            logged_in: !inner.token.is_empty(),
            nickname: stored.map(|s| s.nickname.clone()).unwrap_or_default(),
            avatar: stored.map(|s| s.avatar.clone()).unwrap_or_default(),
            user_id: stored.map(|s| s.user_id.clone()).unwrap_or_default(),
            mobile_masked: stored
                .map(|s| mask_mobile(&s.mobile, &s.area))
                .unwrap_or_default(),
            client_profile: identity::resolve_client_profile(
                stored.map(|s| s.client_profile.as_str()),
            )
            .to_string(),
            plain_fallback: stored.map(|s| !s.token_plain.is_empty()).unwrap_or(false),
            saved_at: stored.map(|s| s.saved_at).unwrap_or(0),
        }
    }

    fn current_profile(&self) -> &'static str {
        let inner = self.inner();
        identity::resolve_client_profile(
            inner
                .stored
                .as_ref()
                .map(|stored| stored.client_profile.as_str()),
        )
    }

    /// 取 token + 档位；未登录直接报错（错误文案与渲染层 `pocketAuthErrorMessage` 对齐）。
    fn session(&self) -> Result<(String, &'static str), String> {
        let inner = self.inner();
        if inner.token.is_empty() {
            return Err("未登录口袋48账号".to_string());
        }
        Ok((
            inner.token.clone(),
            identity::resolve_client_profile(
                inner
                    .stored
                    .as_ref()
                    .map(|stored| stored.client_profile.as_str()),
            ),
        ))
    }

    fn token(&self) -> Option<String> {
        let inner = self.inner();
        if inner.token.is_empty() {
            None
        } else {
            Some(inner.token.clone())
        }
    }

    // ===== 登录态写盘 / 注销 =====

    fn save_auth(
        &self,
        mobile: &str,
        token: &str,
        info: &PocketUserInfo,
        area: &str,
        profile: &str,
    ) -> Result<(), String> {
        let (plain_fallback, token_plain) = match self.vault.save(token) {
            Ok(()) => (false, String::new()),
            Err(err) => {
                // 钥匙串写失败不能让用户登不上；降级为明文并明确标记。
                eprintln!("[pocket] 钥匙串写入失败，降级明文: {err}");
                let _ = self.vault.delete();
                (true, token.to_string())
            }
        };
        let stored = StoredAuth {
            mobile: mobile.trim().to_string(),
            area: normalize_area(area),
            client_profile: identity::resolve_client_profile(Some(profile)).to_string(),
            token_plain,
            nickname: info.nickname.clone(),
            avatar: info.avatar.clone(),
            user_id: info.user_id.clone(),
            saved_at: now_ms(),
        };
        if let Err(err) = write_auth(&self.auth_path(), &stored) {
            if !plain_fallback {
                let _ = self.vault.delete();
            }
            return Err(err);
        }
        let mut inner = self.inner();
        inner.token = token.to_string();
        inner.stored = Some(stored);
        inner.loaded = true;
        Ok(())
    }

    fn update_profile(&self, info: &PocketUserInfo) {
        let stored = {
            let mut inner = self.inner();
            let Some(stored) = inner.stored.as_mut() else {
                return;
            };
            if stored.nickname == info.nickname
                && stored.avatar == info.avatar
                && stored.user_id == info.user_id
            {
                return;
            }
            stored.nickname = info.nickname.clone();
            stored.avatar = info.avatar.clone();
            stored.user_id = info.user_id.clone();
            stored.clone()
        };
        if let Err(err) = write_auth(&self.auth_path(), &stored) {
            eprintln!("[pocket] 回写资料失败（不影响登录态）: {err}");
        }
    }

    /// 凭证失效：清内存 + 删文件 + 删钥匙串，然后广播给所有窗口。
    pub fn expire(&self) {
        {
            let mut inner = self.inner();
            inner.token.clear();
            inner.stored = None;
            inner.loaded = true;
        }
        let _ = fs::remove_file(self.auth_path());
        if let Err(err) = self.vault.delete() {
            eprintln!("[pocket] {err}");
        }
        if let Some(app) = self.app.get() {
            let _ = app.emit("pocketAuthExpired", ());
        }
    }

    pub fn logout(&self) -> Result<(), String> {
        // 先删磁盘再清内存：删除失败时仍保留登录态，避免出现「界面已退出但磁盘还有 token」。
        match fs::remove_file(self.auth_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("删除登录凭证失败: {err}")),
        }
        let mut inner = self.inner();
        inner.token.clear();
        inner.stored = None;
        inner.loaded = true;
        drop(inner);
        if let Err(err) = self.vault.delete() {
            eprintln!("[pocket] {err}");
        }
        Ok(())
    }

    // ===== HTTP =====

    async fn post(
        &self,
        url: &str,
        body: &Value,
        token: Option<&str>,
        profile: &str,
        extra: &[(&str, &str)],
    ) -> Result<Envelope, String> {
        if !url.starts_with(ORIGIN) {
            return Err("账号接口地址越界".to_string());
        }
        let mut request = self.http.post(url);
        let _ = profile; // 身份已收敛为单档，保留形参只为不改调用点
        for (name, value) in identity::identity_headers(&self.device_id) {
            request = request.header(name, value);
        }
        if let Some(token) = token {
            request = request.header("token", token);
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let payload =
            serde_json::to_string(body).map_err(|err| format!("请求序列化失败: {err}"))?;
        let response = request
            .body(payload)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    "请求超时，请检查网络".to_string()
                } else {
                    format!("网络请求失败: {err}")
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("接口异常（HTTP {}）", status.as_u16()));
        }
        let text = response
            .text()
            .await
            .map_err(|err| format!("读取响应失败: {err}"))?;
        serde_json::from_str::<Envelope>(&text).map_err(|err| format!("接口返回数据异常: {err}"))
    }

    /// 房间域用的 POST：未登录返回 `Ok(None)`；token 失效走统一 expire。
    ///
    /// 与 `post` 的唯一差别是「未登录不是错误」——房间页在未登录时应当安静地什么都不显示，
    /// 而不是弹一次「未登录口袋48账号」。
    pub async fn post_room(&self, url: &str, body: &Value) -> Result<Option<Envelope>, String> {
        let Some(token) = self.token() else {
            return Ok(None);
        };
        let profile = self.current_profile();
        let envelope = self.post(url, body, Some(&token), profile, &[]).await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        Ok(Some(envelope))
    }

    fn check_invalid(&self, envelope: &Envelope) -> bool {
        envelope
            .status
            .map(|status| TOKEN_INVALID_STATUSES.contains(&status))
            .unwrap_or(false)
    }

    // ===== 发码 / 登录 =====

    pub async fn send_sms(
        &self,
        mobile: &str,
        area: Option<String>,
        answer: Option<String>,
        profile: Option<String>,
    ) -> Result<PocketSmsResult, String> {
        let area = normalize_area(area.as_deref().unwrap_or(""));
        let mobile = mobile.trim();
        if !is_valid_mobile(mobile, &area) {
            return Err(if area == "86" {
                "请输入 11 位手机号".to_string()
            } else {
                "请输入有效的手机号".to_string()
            });
        }
        let profile = identity::resolve_client_profile(profile.as_deref());
        let body = match answer.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            Some(answer) => json!({ "mobile": mobile, "area": area, "answer": answer }),
            None => json!({
                "mobile": mobile,
                "area": area,
                "businessCode": 1,
                "intelligenceToken": "",
            }),
        };
        let envelope = self.post(SMS_SEND_URL, &body, None, profile, &[]).await?;
        if envelope.status == Some(CAPTCHA_STATUS) {
            let raw = envelope.message.unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(&raw).ok();
            let question = parsed
                .as_ref()
                .and_then(|v| v.get("question"))
                .and_then(Value::as_str)
                .unwrap_or(&raw)
                .to_string();
            let options = parsed
                .as_ref()
                .and_then(|v| v.get("answer"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            return Ok(PocketSmsResult {
                need_captcha: true,
                question,
                options,
                message: "需要完成验证".to_string(),
            });
        }
        if envelope.is_ok() {
            return Ok(PocketSmsResult {
                message: "验证码已发送".to_string(),
                ..PocketSmsResult::default()
            });
        }
        Err(envelope.message_or("验证码发送失败"))
    }

    pub async fn login_by_code(
        &self,
        mobile: &str,
        code: &str,
        area: Option<String>,
        profile: Option<String>,
    ) -> Result<PocketAuthState, String> {
        let area = normalize_area(area.as_deref().unwrap_or(""));
        let mobile = mobile.trim();
        let code = code.trim();
        if !is_valid_mobile(mobile, &area) {
            return Err(if area == "86" {
                "请输入 11 位手机号".to_string()
            } else {
                "请输入有效的手机号".to_string()
            });
        }
        if code.is_empty() {
            return Err("请输入验证码".to_string());
        }
        let profile = identity::resolve_client_profile(profile.as_deref());
        let body = json!({
            "mobileCodeLogin": { "mobile": mobile, "code": code, "area": area },
            "loginType": "MOBILE_SMS_CODE",
            "deviceToken": "",
        });
        let envelope = self.post(SMS_LOGIN_URL, &body, None, profile, &[]).await?;
        let token = envelope
            .content
            .as_ref()
            .and_then(|content| content.get("token"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !envelope.is_ok() || token.is_empty() {
            return Err(envelope.message_or("验证码错误或已过期"));
        }
        // 登录响应未必带资料：再打一次 reload 取昵称/头像，失败不阻断登录。
        let info = match self
            .post(
                TOKEN_CHECK_URL,
                &json!({ "from": "appstart" }),
                Some(&token),
                profile,
                &[],
            )
            .await
        {
            Ok(check) if check.success == Some(true) => pick_user_info(check.content.as_ref()),
            Ok(_) => PocketUserInfo::default(),
            Err(err) => {
                eprintln!("[pocket] 登录后拉取资料失败（不阻断登录）: {err}");
                PocketUserInfo::default()
            }
        };
        self.save_auth(mobile, &token, &info, &area, profile)?;
        Ok(self.auth_state())
    }

    pub async fn login_by_token(
        &self,
        token: &str,
        mobile: Option<String>,
        profile: Option<String>,
    ) -> Result<PocketAuthState, String> {
        let token = token.trim();
        if token.is_empty() {
            return Err("请输入 token".to_string());
        }
        let profile = identity::resolve_client_profile(profile.as_deref());
        let envelope = self
            .post(
                TOKEN_CHECK_URL,
                &json!({ "from": "appstart" }),
                Some(token),
                profile,
                &[],
            )
            .await?;
        if !envelope.is_ok() {
            return Err(if self.check_invalid(&envelope) {
                "token 无效或已过期".to_string()
            } else {
                envelope.message_or("token 校验失败")
            });
        }
        let info = pick_user_info(envelope.content.as_ref());
        let mobile = mobile.unwrap_or_default();
        self.save_auth(mobile.trim(), token, &info, "86", profile)?;
        Ok(self.auth_state())
    }

    // ===== 校验 / 资料 / 签到 =====

    /// 校验登录并拉取展示资料；`imOk`/`moneyOk` 失败只降级对应条目，不影响登录态。
    pub async fn verify_login(&self) -> Result<PocketVerifyResult, String> {
        let (token, profile) = match self.session() {
            Ok(session) => session,
            Err(_) => {
                return Ok(PocketVerifyResult {
                    message: "未登录口袋48账号".to_string(),
                    ..PocketVerifyResult::default()
                });
            }
        };
        let envelope = self
            .post(
                TOKEN_CHECK_URL,
                &json!({ "from": "appstart" }),
                Some(&token),
                profile,
                &[],
            )
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Ok(PocketVerifyResult {
                message: "登录已过期，请重新登录".to_string(),
                ..PocketVerifyResult::default()
            });
        }
        if !envelope.is_ok() {
            return Ok(PocketVerifyResult {
                message: envelope.message_or("token 校验失败"),
                ..PocketVerifyResult::default()
            });
        }
        let content = envelope.content.clone().unwrap_or_else(|| json!({}));
        let info = pick_user_info(Some(&content));
        let user = user_object(&content);
        let social = pick_social_stats(&user);
        let support = pick_number(&user, &["support"]);
        // 校验通过即回写服务端最新资料（头像/昵称可能已改），token 不动。
        self.update_profile(&info);

        let mut im_ok = false;
        let mut im_accid = String::new();
        let mut im_message = String::new();
        match self.im_credentials().await {
            Ok((accid, _)) => {
                im_ok = true;
                im_accid = accid;
            }
            Err(err) => im_message = err,
        }

        let mut money_ok = false;
        let mut money = Money::default();
        match self
            .post(
                USER_MONEY_URL,
                &json!({ "token": token }),
                Some(&token),
                profile,
                &[],
            )
            .await
        {
            Ok(money_envelope) if money_envelope.is_ok() => {
                money = pick_money(money_envelope.content.as_ref());
                money_ok = true;
            }
            Ok(_) => {}
            Err(err) => eprintln!("[pocket] 钱包接口失败（不影响登录态）: {err}"),
        }

        Ok(PocketVerifyResult {
            valid: true,
            nickname: info.nickname,
            user_id: info.user_id,
            level: pick_number(&user, &["level"]),
            im_ok,
            im_accid,
            money_ok,
            money_pay: money.money_pay,
            money_free: money.money_free,
            money_total: money.money_total,
            support,
            fans: social.fans,
            likes: social.likes,
            signature: social.signature,
            message: if im_ok {
                "登录有效".to_string()
            } else {
                format!("登录有效，但互动消息凭据获取失败：{im_message}")
            },
        })
    }

    pub async fn check_in(&self) -> Result<PocketCheckInResult, String> {
        let (token, profile) = self.session()?;
        let envelope = self
            .post(CHECKIN_URL, &json!({}), Some(&token), profile, &[])
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        if !envelope.is_ok() {
            return Err(envelope.message_or("签到失败"));
        }
        let content = envelope.content.unwrap_or_else(|| json!({}));
        Ok(PocketCheckInResult {
            add_exp: non_negative(&content, "addExp"),
            add_money: non_negative(&content, "addMoney"),
            add_support: non_negative(&content, "addSupport"),
            days: non_negative(&content, "days"),
        })
    }

    /// 直播 ID → 聊天室 ID（`getLiveOne`）。未登录返回 `Ok(None)`。
    ///
    /// `chatroomId` 为空时兜底 `roomId`：**0 表示无聊天室，绝不能当真值**
    /// （`'0'` 拿去连聊天室必然失败）。
    pub async fn live_chatroom_id(&self, live_id: &str) -> Result<Option<String>, String> {
        let Some(token) = self.token() else {
            return Ok(None);
        };
        let profile = self.current_profile();
        let envelope = self
            .post(
                LIVE_ONE_URL,
                &json!({ "type": 1, "userId": "0", "liveId": live_id }),
                Some(&token),
                profile,
                &[],
            )
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        if envelope.success == Some(false) {
            return Err(envelope.message_or("获取直播聊天室 ID 失败"));
        }
        let content = envelope.content.clone().unwrap_or_else(|| json!({}));
        // 字段类型以报文为准：实测 `roomId` 是**字符串**（"16285946319"），
        // 直接 `as_i64()` 会拿到 None。与 Electron `Number(content.roomId)` 同口径做宽松解析。
        let chatroom = content
            .get("chatroomId")
            .map(loose_number_text)
            .unwrap_or_default();
        if !chatroom.is_empty() && chatroom != "0" {
            return Ok(Some(chatroom));
        }
        // 兜底 roomId（yaya 同款）：**0 表示无聊天室，绝不能当真值**
        let room = content
            .get("roomId")
            .map(loose_number_text)
            .unwrap_or_default();
        let room_number = room.parse::<f64>().unwrap_or(0.0);
        Ok((room_number > 0.0).then(|| format!("{}", room_number as i64)))
    }

    /// 直播结束复盘（`live/result`）：必须带 token；进行中也可查。
    pub async fn live_result(&self, live_id: &str) -> Result<Value, String> {
        let Some(token) = self.token() else {
            return Err("未登录口袋48账号".to_string());
        };
        let profile = self.current_profile();
        let envelope = self
            .post(
                LIVE_RESULT_URL,
                &json!({ "liveId": live_id }),
                Some(&token),
                profile,
                &[],
            )
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        if envelope.success == Some(false) {
            return Err(envelope.message_or("获取直播复盘失败"));
        }
        Ok(envelope.content.unwrap_or_else(|| json!({})))
    }

    /// 网易云信登录凭据；token 失效时清凭证并报「登录已过期」。
    /// 供 `pocketDanmakuStart` 起聊天室会话用：凭据不出 Rust。
    pub async fn im_credentials(&self) -> Result<(String, String), String> {
        let (token, profile) = self.session()?;
        let envelope = self
            .post(
                IM_USER_INFO_URL,
                &json!({}),
                Some(&token),
                profile,
                &[("P-Sign-Type", "V0")],
            )
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        let content = envelope.content.clone().unwrap_or_else(|| json!({}));
        let object = content.as_object().cloned().unwrap_or_default();
        let accid = first_string(&object, &["accid", "accId", "imUserId"]);
        let im_token = first_string(&object, &["pwd", "token", "imPwd"]);
        if !envelope.is_ok() || accid.is_empty() || im_token.is_empty() {
            return Err(envelope.message_or("获取直播聊天室凭据失败"));
        }
        Ok((accid, im_token))
    }

    // ===== 关注列表（账号维度） =====

    pub async fn followed_ids(&self) -> Result<Vec<i64>, String> {
        let Some(token) = self.token() else {
            return Ok(Vec::new());
        };
        let profile = self.current_profile();
        let envelope = self
            .post(FRIENDS_IDS_URL, &json!({}), Some(&token), profile, &[])
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        if envelope.success == Some(false) {
            return Err(envelope.message_or("获取关注列表失败"));
        }
        let ids = envelope
            .content
            .as_ref()
            .and_then(|content| content.get("data"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Number(number) => number.as_i64(),
                        Value::String(text) => text.parse::<i64>().ok(),
                        _ => None,
                    })
                    .filter(|id| *id > 0)
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    pub async fn follow_member(&self, user_id: i64) -> Result<(), String> {
        self.post_follow(FRIEND_ADD_URL, user_id, "关注").await
    }

    pub async fn unfollow_member(&self, user_id: i64) -> Result<(), String> {
        self.post_follow(FRIEND_REMOVE_URL, user_id, "取消关注")
            .await
    }

    async fn post_follow(&self, url: &str, user_id: i64, action: &str) -> Result<(), String> {
        let (token, profile) = self.session()?;
        let envelope = self
            .post(
                url,
                &json!({ "toSourceId": user_id, "toType": FOLLOW_TO_TYPE_MEMBER }),
                Some(&token),
                profile,
                &[],
            )
            .await?;
        if self.check_invalid(&envelope) {
            self.expire();
            return Err("登录已过期，请重新登录".to_string());
        }
        if !envelope.is_ok() {
            return Err(envelope.message_or(&format!("{action}失败")));
        }
        Ok(())
    }
}

// ===== 回包与字段解析 =====

#[derive(Deserialize, Default, Clone)]
pub struct Envelope {
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub content: Option<Value>,
}

impl Envelope {
    /// v2 信封只有 status，旧接口才有 success；`status=200` 且显式 `success=false` 才算失败。
    fn is_ok(&self) -> bool {
        self.status == Some(200) && self.success != Some(false)
    }

    pub fn message_or(&self, fallback: &str) -> String {
        match self.message.as_deref() {
            Some(message) if !message.trim().is_empty() => message.to_string(),
            _ => fallback.to_string(),
        }
    }
}

#[derive(Default, Clone)]
struct PocketUserInfo {
    nickname: String,
    avatar: String,
    user_id: String,
}

#[derive(Default)]
struct Money {
    money_pay: f64,
    money_free: f64,
    money_total: f64,
}

fn pick_user_info(content: Option<&Value>) -> PocketUserInfo {
    let content = content.cloned().unwrap_or_else(|| json!({}));
    let user = user_object(&content);
    PocketUserInfo {
        nickname: first_string(&user, &["nickname", "nickName", "userName"]),
        avatar: first_string(&user, &["avatar", "userAvatar"]),
        user_id: first_string(&user, &["userId", "id"]),
    }
    .normalized()
}

impl PocketUserInfo {
    fn normalized(mut self) -> Self {
        self.nickname = self.nickname.trim().to_string();
        self.avatar = self.avatar.trim().to_string();
        self.user_id = self.user_id.trim().to_string();
        self
    }
}

fn user_object(content: &Value) -> Map<String, Value> {
    for key in ["userInfo", "user"] {
        if let Some(object) = content.get(key).and_then(Value::as_object) {
            return object.clone();
        }
    }
    content.as_object().cloned().unwrap_or_default()
}

/// JS `Number(...)` 同口径的宽松数字取值：数字直取、字符串（含空白）trim 后原样返回，
/// 其余（对象 / 数组 / 布尔 / null）当空。
fn loose_number_text(value: &Value) -> String {
    match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_string(),
        _ => String::new(),
    }
}

fn first_string(object: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        match object.get(*key) {
            Some(Value::String(text)) if !text.trim().is_empty() => return text.trim().to_string(),
            Some(Value::Number(number)) => return number.to_string(),
            _ => {}
        }
    }
    String::new()
}

fn pick_number(object: &Map<String, Value>, keys: &[&str]) -> String {
    first_string(object, keys)
}

struct SocialStats {
    fans: String,
    likes: String,
    signature: String,
}

fn pick_social_stats(object: &Map<String, Value>) -> SocialStats {
    SocialStats {
        fans: first_number(object, &["fansNum", "fansTimes", "fans", "follower"]),
        likes: first_number(
            object,
            &["likeCount", "bePraisedTimes", "praisedTimes", "likesNum"],
        ),
        signature: first_string(
            object,
            &["signature", "autograph", "introduce", "description"],
        ),
    }
}

fn first_number(object: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        let parsed = match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(number) = parsed
            && number.is_finite()
        {
            return trim_number(number);
        }
    }
    String::new()
}

fn trim_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn pick_money(content: Option<&Value>) -> Money {
    let content = content.cloned().unwrap_or_else(|| json!({}));
    let money_pay = non_negative(&content, "moneyPay");
    let money_free = non_negative(&content, "moneyFree");
    let total = non_negative(&content, "moneyTotal");
    Money {
        money_pay,
        money_free,
        money_total: if total > 0.0 {
            total
        } else {
            money_pay + money_free
        },
    }
}

fn non_negative(content: &Value, key: &str) -> f64 {
    let parsed = match content.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(number) if number.is_finite() && number >= 0.0 => number,
        _ => 0.0,
    }
}

// ===== 凭证文件读写 =====

fn read_auth(dir: &Path) -> Option<StoredAuth> {
    let path = dir.join(AUTH_FILE_NAME);
    let text = fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<StoredAuth>(&text) {
        Ok(stored) => Some(stored),
        Err(err) => {
            eprintln!("[pocket] 凭证文件解析失败（按未登录处理）: {err}");
            None
        }
    }
}

/// 原子写：tmp + fsync + rename；Unix 上文件权限固定 0600（token 可能明文）。
fn write_auth(path: &Path, stored: &StoredAuth) -> Result<(), String> {
    let serialized =
        serde_json::to_string(stored).map_err(|err| format!("凭证序列化失败: {err}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("创建数据目录失败: {err}"))?;
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写入登录凭证失败: {err}"));
    }
    Ok(())
}

// ===== 小工具 =====

fn normalize_area(area: &str) -> String {
    let trimmed = area.trim().trim_start_matches('+');
    if trimmed.is_empty() {
        "86".to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_valid_mobile(mobile: &str, area: &str) -> bool {
    if area == "86" {
        mobile.len() == 11 && mobile.chars().all(|c| c.is_ascii_digit())
    } else {
        let len = mobile.chars().count();
        (4..=15).contains(&len) && mobile.chars().all(|c| c.is_ascii_digit())
    }
}

fn mask_mobile(mobile: &str, area: &str) -> String {
    let area = normalize_area(area);
    if area != "86" {
        if mobile.chars().count() < 5 {
            return String::new();
        }
        let prefix: String = mobile.chars().take(2).collect();
        let suffix: String = mobile
            .chars()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return format!("+{area} {prefix}****{suffix}");
    }
    if mobile.chars().count() < 7 {
        return String::new();
    }
    let prefix: String = mobile.chars().take(3).collect();
    let suffix: String = mobile
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}****{suffix}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct FailingVault;

    impl TokenVault for FailingVault {
        fn save(&self, _: &str) -> Result<(), String> {
            Err("no keyring".to_string())
        }
        fn load(&self) -> Result<Option<String>, String> {
            Err("no keyring".to_string())
        }
        fn delete(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("d48-pocket-{tag}-{unique}"))
    }

    /// 构造并**同步**装载钥匙串（生产里由后台线程调用）。
    fn account(dir: &Path, vault: Box<dyn TokenVault>) -> Arc<PocketAccount> {
        let account = Arc::new(PocketAccount::load(
            dir.to_path_buf(),
            "0123456789abcdef".to_string(),
            reqwest::Client::new(),
            vault,
        ));
        account.unlock_vault();
        account
    }

    fn info() -> PocketUserInfo {
        PocketUserInfo {
            nickname: "测试".to_string(),
            avatar: "https://example.com/a.png".to_string(),
            user_id: "42".to_string(),
        }
    }

    #[test]
    fn auth_roundtrips_and_masks_mobile() {
        let dir = temp_dir("roundtrip");
        let pocket = account(&dir, Box::new(MemoryVault::default()));
        assert!(!pocket.auth_state().logged_in);
        pocket
            .save_auth("13912345678", "token-1", &info(), "86", "pocket48")
            .unwrap();
        let state = pocket.auth_state();
        assert!(state.logged_in);
        assert_eq!(state.mobile_masked, "139****5678");
        assert_eq!(state.nickname, "测试");
        assert_eq!(state.user_id, "42");
        assert!(!state.plain_fallback);

        // 重新加载：token 来自（内存）钥匙串，而不是凭证文件。
        let reloaded = account(&dir, Box::new(MemoryVault::default()));
        assert!(
            !reloaded.auth_state().logged_in,
            "新钥匙串实例读不到旧 token"
        );
    }

    #[test]
    fn plain_fallback_when_keyring_is_unavailable() {
        let dir = temp_dir("fallback");
        let pocket = account(&dir, Box::new(FailingVault));
        // 旧凭证写着 seine48：落盘可以带旧值，但读回来一律是 pocket48
        pocket
            .save_auth("13912345678", "token-2", &info(), "86", "seine48")
            .unwrap();
        let state = pocket.auth_state();
        assert!(state.logged_in);
        assert!(state.plain_fallback);
        assert_eq!(state.client_profile, "pocket48");

        let text = fs::read_to_string(dir.join(AUTH_FILE_NAME)).unwrap();
        assert!(text.contains("token-2"), "钥匙串不可用时应降级明文");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(AUTH_FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // 明文回退可以从磁盘恢复登录态。
        let reloaded = account(&dir, Box::new(FailingVault));
        assert!(reloaded.auth_state().logged_in);
        assert_eq!(reloaded.token().as_deref(), Some("token-2"));
    }

    /// 记录写入的假钥匙串：用于验证「明文 token 迁回钥匙串」。
    struct RecordingVault {
        saved: Mutex<Option<String>>,
    }

    impl RecordingVault {
        fn new() -> Self {
            Self {
                saved: Mutex::new(None),
            }
        }
    }

    impl TokenVault for RecordingVault {
        fn save(&self, token: &str) -> Result<(), String> {
            *self.saved.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
        fn load(&self) -> Result<Option<String>, String> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn delete(&self) -> Result<(), String> {
            *self.saved.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn plaintext_token_is_migrated_back_to_keyring() {
        let dir = temp_dir("migrate-plain");
        // 造一份「明文 token」的凭证文件（模拟上一版留下的状态）
        let stored = StoredAuth {
            mobile: "13912345678".to_string(),
            area: "86".to_string(),
            client_profile: "pocket48".to_string(),
            token_plain: "legacy-plain-token".to_string(),
            nickname: "海市".to_string(),
            avatar: String::new(),
            user_id: "42".to_string(),
            saved_at: 1,
        };
        write_auth(&dir.join(AUTH_FILE_NAME), &stored).unwrap();

        let pocket = account(&dir, Box::new(RecordingVault::new()));
        // 加载时应当把明文迁回钥匙串，并清空文件里的 tokenPlain
        assert!(pocket.auth_state().logged_in);
        let text = fs::read_to_string(dir.join(AUTH_FILE_NAME)).unwrap();
        assert!(
            !text.contains("legacy-plain-token"),
            "迁移后文件不该再有明文 token"
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["tokenPlain"], "");

        // 迁移失败（钥匙串不可用）时继续用明文，不能让用户掉登录
        let dir2 = temp_dir("migrate-fail");
        write_auth(&dir2.join(AUTH_FILE_NAME), &stored).unwrap();
        let fallback = account(&dir2, Box::new(FailingVault));
        assert!(fallback.auth_state().logged_in);
        assert!(fallback.auth_state().plain_fallback);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    /// 回归：钥匙串读取可能阻塞数十秒，所以启动时只做同步构造，装载放到后台。
    /// 装载前必须「不登录」但「不是已装载」，否则未读检测会把刚启动的账号当成登出清空。
    #[test]
    fn vault_is_read_during_construction() {
        // 启动时序：钥匙串在 `PocketAccount::load` 里同步读完（用户授权就发生在这一步），
        // 之后主窗才显示。这里固定「构造完成即已装载」，防止又退回后台延时读取。
        let dir = temp_dir("eager-vault");
        let stored = StoredAuth {
            mobile: "13912345678".to_string(),
            area: "86".to_string(),
            client_profile: "pocket48".to_string(),
            token_plain: String::new(),
            nickname: "海市".to_string(),
            avatar: String::new(),
            user_id: "42".to_string(),
            saved_at: 1,
        };
        write_auth(&dir.join(AUTH_FILE_NAME), &stored).unwrap();

        let pocket = PocketAccount::load(
            dir.clone(),
            "0123456789abcdef".to_string(),
            reqwest::Client::new(),
            Box::new(MemoryVault::default()),
        );
        assert!(pocket.is_loaded(), "构造完成就应该读完钥匙串");
        // 凭证文件里没有明文 token，新钥匙串实例读不到 → 未登录
        assert!(!pocket.authorized());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_clears_file_and_memory() {
        let dir = temp_dir("logout");
        let pocket = account(&dir, Box::new(MemoryVault::default()));
        pocket
            .save_auth("13912345678", "token-3", &info(), "86", "pocket48")
            .unwrap();
        assert!(dir.join(AUTH_FILE_NAME).exists());
        pocket.logout().unwrap();
        assert!(!dir.join(AUTH_FILE_NAME).exists());
        assert!(!pocket.auth_state().logged_in);
        assert!(pocket.token().is_none());
    }

    #[test]
    fn expire_clears_credentials() {
        let dir = temp_dir("expire");
        let pocket = account(&dir, Box::new(MemoryVault::default()));
        pocket
            .save_auth("13912345678", "token-4", &info(), "86", "pocket48")
            .unwrap();
        pocket.expire();
        assert!(!dir.join(AUTH_FILE_NAME).exists());
        assert!(!pocket.auth_state().logged_in);
    }

    #[test]
    fn identity_headers_are_pocket48_only() {
        let headers = crate::identity::identity_headers("0123456789abcdef");
        assert_eq!(
            headers.len(),
            5,
            "只剩 content-type / ua / appinfo / lang / pa"
        );
        assert!(
            headers
                .iter()
                .any(|(_, value)| value.contains("PocketFans201807/7.1.44_26092001"))
        );
        assert!(
            !headers
                .iter()
                .any(|(name, _)| *name == "x-custom-device-type")
        );
    }

    #[test]
    fn loose_number_text_matches_js_number() {
        // 实测 getLiveOne 的 roomId 是字符串："16285946319"；as_i64() 会拿到 None
        assert_eq!(loose_number_text(&json!("16285946319")), "16285946319");
        assert_eq!(loose_number_text(&json!("  16285946319  ")), "16285946319");
        assert_eq!(loose_number_text(&json!(16285946319i64)), "16285946319");
        assert_eq!(loose_number_text(&json!(0)), "0");
        // 非数值一律当空（不要拼出 "null" / "[object Object]" 去连聊天室）
        assert_eq!(loose_number_text(&json!(null)), "");
        assert_eq!(loose_number_text(&json!({})), "");
        assert_eq!(loose_number_text(&json!([])), "");
    }

    #[test]
    fn mobile_validation_and_area_mask() {
        assert!(is_valid_mobile("13912345678", "86"));
        assert!(!is_valid_mobile("1391234567", "86"));
        assert!(is_valid_mobile("123456", "852"));
        assert_eq!(mask_mobile("13912345678", "86"), "139****5678");
        assert_eq!(mask_mobile("123456789", "852"), "+852 12****89");
        assert_eq!(normalize_area("+852"), "852");
        assert_eq!(normalize_area(""), "86");
    }
}
