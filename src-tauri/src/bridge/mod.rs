//! 通用 IPC 桥（对照 Electron 版 `src/preload/index.ts` 的 `mainAPI` 契约）。
//!
//! 渲染层只认一个 command：`invoke('main_api', { channel, args })`。
//!
//! 本文件只放三件事：上下文 `BridgeContext`、窗口权限表（`FLOAT_WINDOW_CHANNELS` /
//! `channel_allowed_for_window`）、以及按通道名路由到各域的 `dispatch_from`。
//! 具体通道实现在 `channels/` 下按域分文件（net / data / media / danmaku / rooms /
//! float / system / tasks / player / settings），每个文件一个 `match`。
//!
//! 新增通道：在对应域加分支 → `CHANNELS` 补名字 →（仅播放窗可用时）补 `FLOAT_WINDOW_CHANNELS`
//! → `src/bridge.ts` 补类型。参数与返回值仍以契约文件为唯一事实来源。

/// 按域的通道实现：每个文件一个 `dispatch`，`mod.rs` 只做权限收敛与路由。
mod channels;

use std::path::Path;
use std::sync::{Arc, OnceLock};

use tauri::{AppHandle, Manager};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    allowed_path, bilibili, download, identity, image_file, media, migrate, net, pocket,
    power_save::SleepBlocker,
};

/// 已实现的通道清单（`probe` 与文档共用；也是契约覆盖率的检查锚点）。
pub const CHANNELS: &[&str] = &[
    "netRequest",
    "imageDataUrl",
    "netDiagnose",
    "getAppInfo",
    // database.json 只读（成员 / 名单 / 配置 / 成员树）
    "hasMembers",
    "getAllMembers",
    "getMemberInfo",
    "getMemberFlags",
    "getConfig",
    "getMemberTree",
    "setConfig",
    "setMemberFlags",
    "addMemberFlag",
    "addMemberFlags",
    "removeMemberFlag",
    "saveMemberData",
    // 升格：归属由 Rust 按登录凭证推导，渲染层不接触 ownerId
    "getMemberTeamPromotions",
    "setMemberTeamPromotion",
    "removeMemberTeamPromotion",
    // FFmpeg 环境与直播本地转流
    "getFfmpegStatus",
    "downloadFfmpeg",
    "selectDirectory",
    "createLiveStream",
    "stopLiveStream",
    "preventSleep",
    "allowSleep",
    // 系统集成：打开本地路径 / 外链 / 图片剪贴板 / 保存图片 / B 站房间信息
    "openPath",
    "openExternal",
    "copyImageToClipboard",
    "saveImageFile",
    // 下载 / 录制任务（ffmpeg 子进程；事件走 downloadTask* / recordTask* 广播）
    "downloadTaskStart",
    "downloadTaskStop",
    "downloadTaskList",
    "downloadTaskRemove",
    "recordTaskStart",
    "recordTaskStop",
    "recordTaskList",
    "recordTaskRemove",
    "bilibiliRoomInfos",
    // B 站直播弹幕（WSS + brotli，事件走 danmakuBatch）
    "notifyLiveUnavailable",
    "danmakuStart",
    "danmakuStop",
    // 设置页：路径拼接 / 版本号 / 问题反馈
    // （自动更新已整块移除：设置页只留一个跳 GitHub Releases 的静态入口，不再有检查/下载通道）
    "pathJoin",
    "getAppVersion",
    "feedbackMeta",
    "submitFeedback",
    // 置顶偏好：设置页写入，独立播放窗按它决定默认层级（当前作用于主窗）
    "floatWindowSetTopmostPreference",
    // 独立播放窗：创建 / 载荷回取 / 按比例定形 / 播放状态 / 本窗置顶
    "openFloatWindow",
    "floatPlayerGetPayload",
    "floatWindowFitAspect",
    "floatWindowSetPlaying",
    "floatWindowGetTopmost",
    "floatWindowSetTopmost",
    // 账号域（发码 / 登录 / 校验 / 签到 / 关注同步）
    "pocketSendSms",
    "pocketLoginByCode",
    "pocketLoginByToken",
    "pocketGetAuthState",
    "pocketLogout",
    "pocketVerifyLogin",
    "pocketCheckIn",
    "pocketGetLiveResult",
    "pocketGetFollowedIds",
    "pocketFollowMember",
    "pocketUnfollowMember",
    // 口袋聊天室弹幕（Rust 持有 TCP 长连接，事件走 pocketDanmakuBatch）
    "pocketGetChatroomId",
    "pocketDanmakuStart",
    "pocketDanmakuStop",
    // 房间域（只读：频道解析 / 频道列表 / 权限 / 历史 / 精华 / 末条 + QChat 订阅）
    "pocketGetRoomChannel",
    "pocketGetRoomChannels",
    "pocketGetServerChannels",
    "pocketGetRoomPermissions",
    "pocketGetRoomMessages",
    "pocketGetRoomEssenceMessages",
    "pocketGetRoomLastMessages",
    "pocketUnreadState",
    "pocketMarkRead",
    // 小房间（暗号频道）：发现 / 作废 / 暗号问题
    "pocketRefreshSmallChannel",
    "pocketInvalidateSmallChannel",
    "pocketGetRoomQuestion",
    "pocketCheckRoomAnswer",
    "roomAlbumDownload",
    "pocketRoomQChatEnter",
    "pocketRoomQChatLeave",
];

/// 独立播放窗（label = `live:<id>` / `playback:<id>`）允许调用的通道。
///
/// 播放器与主窗复用同一份渲染层，但实际调用面只有播放、弹幕、下载/录制任务和窗口壳。
/// 这里按「最小可用集合」收敛：账号写操作、成员库写入、图片保存、反馈 / 更新等
/// 不开放给播放窗；新增播放器功能时要同步补这份清单。
const FLOAT_WINDOW_CHANNELS: &[&str] = &[
    // 窗口壳（浮窗自己的标题栏与全屏）
    // 浮窗载荷 / 几何 / 置顶
    "floatPlayerGetPayload",
    "floatWindowFitAspect",
    "floatWindowSetPlaying",
    "floatWindowGetTopmost",
    "floatWindowSetTopmost",
    // 播放会话与防休眠
    "createLiveStream",
    "stopLiveStream",
    "preventSleep",
    "allowSleep",
    "notifyLiveUnavailable",
    // 播放器数据面：详情走通用 HTTP 代理，头像 / 封面允许转 data URL
    "netRequest",
    "imageDataUrl",
    "openExternal",
    // 配置：弹幕摆放 / 下载目录读取
    "getConfig",
    "setConfig",
    "pathJoin",
    "getFfmpegStatus",
    "openPath",
    // 下载 / 录制任务（播放器内的录制与回放下载按钮）
    "downloadTaskStart",
    "downloadTaskStop",
    "downloadTaskList",
    "downloadTaskRemove",
    "recordTaskStart",
    "recordTaskStop",
    "recordTaskList",
    "recordTaskRemove",
    // 弹幕与登录态只读
    "pocketGetAuthState",
    "pocketGetChatroomId",
    "pocketGetLiveResult",
    "pocketDanmakuStart",
    "pocketDanmakuStop",
    "danmakuStart",
    "danmakuStop",
    "bilibiliRoomInfos",
];

/// 通道是否允许由该窗口发起：`main` 全放行，`live:*` / `playback:*` 只放行
/// [`FLOAT_WINDOW_CHANNELS`]，其它 label 默认拒绝。无窗口上下文的调用不走这里。
fn channel_allowed_for_window(label: &str, channel: &str) -> bool {
    if label == "main" {
        return true;
    }
    if crate::float_window::split_label(label).is_some() {
        return FLOAT_WINDOW_CHANNELS.contains(&channel);
    }
    false
}

/// 桥的后端上下文：设备身份、业务 API 客户端、HTTP 出口闸门。
/// 一个进程一份，由 Tauri 的 `manage` 持有。
pub struct BridgeContext {
    gate: Arc<net::NetGate>,
    http: reqwest::Client,
    /// 诊断用短超时客户端（8s，见 `diagnose`）。
    diagnose_http: reqwest::Client,
    /// 本地 HTTP-FLV 服务；`new_isolated` 默认不监听，App setup / 探针显式 `start_media()`。
    media: media::MediaServer,
    sleep: SleepBlocker,
    /// FFmpeg 在线下载：独立目录 + 进程内防重入；只在 App 启动时注入 AppHandle 以推送进度。
    downloader: download::FfmpegDownloader,
    /// 账号域：凭证读盘 / 系统钥匙串 / 登录态广播。
    pocket: Arc<pocket::PocketAccount>,
    /// 成员库：启动时由后台线程装载，装载完成前为空库（`has_members() == false`）。
    db: Arc<crate::db_slot::DatabaseSlot>,
    /// B 站官方直播间信息缓存（5 分钟）。
    bilibili: bilibili::BilibiliRoomCache,
    /// B 站出口客户端：不走 pocketapi 白名单（对照 Electron 的直连 fetch）。
    bilibili_http: reqwest::Client,
    /// 独立播放窗：载荷表 + 各窗的尺寸 / 置顶状态。
    floats: crate::float_window::FloatWindows,
    /// 聊天室弹幕会话：按 chatroomId 复用连接，窗口只是订阅者。
    danmaku: crate::danmaku::DanmakuSessions,
    /// 房间域 HTTP 缓存（发言权限 30s / server 频道列表 5min）。
    room_cache: crate::room::RoomCache,
    /// 房间 QChat 会话表（账号维度，按频道订阅）。
    room_qchat: crate::room_qchat::RoomQChatSessions,
    /// 房间未读检测（Rust 是唯一真相，渲染层只做镜像）。
    unread: crate::unread::UnreadTracker,
    /// B 站弹幕会话（按房间复用一条 WSS 连接）。
    danmaku_sessions: crate::bilibili_danmaku::DanmakuSessions,
    /// 下载 / 录制任务（ffmpeg 子进程，事件广播给全部窗口）。
    tasks: crate::tasks::TaskRuntime,
    app: OnceLock<AppHandle>,
    device_id: String,
}

impl BridgeContext {
    pub fn new(data_dir: &Path) -> Result<Self, String> {
        // 先迁移旧版数据，再读设备 ID：复用旧设备 ID 才不会让服务端当成新安装。
        let report = migrate::migrate_legacy_data(data_dir);
        if let Some(note) = report.note
            && !report.migrated
        {
            eprintln!("[migrate] 跳过: {note}");
        }

        // 默认一律走系统钥匙串（与 Electron 版一致）。
        //
        // 仅在显式设置 `D48_PLAIN_VAULT=1` 时才退化为明文：debug 二进制每次重编译都会换
        // cdhash，macOS Keychain 会认为换了应用而反复弹密码框，本地高频调试时可以临时用它绕开。
        // **这是仅供开发的逃逸口，不是默认行为**——生产构建里这个变量无效。
        #[cfg(debug_assertions)]
        let plain_vault = std::env::var("D48_PLAIN_VAULT").as_deref() == Ok("1");
        #[cfg(not(debug_assertions))]
        let plain_vault = false;

        let vault: Box<dyn pocket::TokenVault> = if plain_vault {
            crate::app_log::log_line(
                "[pocket] D48_PLAIN_VAULT=1：本次运行使用明文凭证（仅调试用）",
            );
            Box::new(pocket::PlainOnlyVault)
        } else {
            Box::new(pocket::KeyringVault)
        };
        Self::build(data_dir, vault)
    }

    /// 不触发旧数据迁移的构造：单测与探针用，避免把本机真实 `database.json` 复制进临时目录。
    pub fn new_isolated(data_dir: &Path) -> Result<Self, String> {
        // 单测 / 探针不触碰真实系统钥匙串，凭证只活在本进程内存。
        Self::build(data_dir, Box::new(pocket::MemoryVault::default()))
    }

    fn build(data_dir: &Path, vault: Box<dyn pocket::TokenVault>) -> Result<Self, String> {
        let device_id = identity::load_or_create_device_id(data_dir);
        let http = net::new_client()?;
        let pocket = pocket::PocketAccount::load(
            data_dir.to_path_buf(),
            device_id.clone(),
            http.clone(),
            vault,
        );
        Ok(Self {
            gate: Arc::new(net::NetGate::new()),
            http,
            diagnose_http: net::new_client_with_timeout(std::time::Duration::from_secs(8))?,
            // 迁移完成后再装载；只读，不创建/改写数据库文件。
            // 装载放在后台（见 lib.rs setup），这里先给空库，首屏不用等 1.6 MB 解析。
            db: Arc::new(crate::db_slot::DatabaseSlot::empty()),
            media: media::MediaServer::new(),
            sleep: SleepBlocker::new(),
            downloader: download::FfmpegDownloader::new(data_dir.to_path_buf()),
            pocket: Arc::new(pocket),
            bilibili: bilibili::BilibiliRoomCache::new(),
            bilibili_http: net::new_client_with_timeout(std::time::Duration::from_secs(12))?,
            floats: crate::float_window::FloatWindows::new(),
            danmaku: crate::danmaku::DanmakuSessions::new(),
            room_cache: crate::room::RoomCache::load(data_dir),
            room_qchat: crate::room_qchat::RoomQChatSessions::new(),
            unread: crate::unread::UnreadTracker::load(data_dir),
            danmaku_sessions: crate::bilibili_danmaku::DanmakuSessions::new(),
            tasks: crate::tasks::TaskRuntime::new(),
            app: OnceLock::new(),
            device_id,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn gate(&self) -> &net::NetGate {
        &self.gate
    }

    /// 成员库句柄（后台装载完成后指向真实库；装载前读到的是一份空库快照）。
    pub fn database_slot(&self) -> &crate::db_slot::DatabaseSlot {
        &self.db
    }

    /// 读一条配置（测试 / 探针用）。读接口一律走 [`Self::database_slot`] 的 `with`。
    #[cfg(any(test, debug_assertions))]
    pub fn debug_config(&self, key: &str) -> Result<Value, String> {
        self.db.with(|db| db.config(key))
    }

    pub fn sleep(&self) -> &SleepBlocker {
        &self.sleep
    }

    pub fn pocket(&self) -> &pocket::PocketAccount {
        &self.pocket
    }

    /// 后台装载凭证（钥匙串可能阻塞，只交给阻塞线程池）。
    pub fn pocket_handle(&self) -> Arc<pocket::PocketAccount> {
        Arc::clone(&self.pocket)
    }

    pub fn media(&self) -> &media::MediaServer {
        &self.media
    }

    /// 下载 / 录制任务表（退出时清理）。
    pub fn tasks(&self) -> &crate::tasks::TaskRuntime {
        &self.tasks
    }

    /// B 站弹幕会话表（退出时清理）。
    pub fn bilibili_danmaku(&self) -> &crate::bilibili_danmaku::DanmakuSessions {
        &self.danmaku_sessions
    }

    /// 房间未读检测器（App 启动时挂轮询；测试 / 探针可直接读快照）。
    pub fn unread(&self) -> &crate::unread::UnreadTracker {
        &self.unread
    }

    /// 一轮未读检测：关注 = 口袋关注 ∪ 本地关注 − 屏蔽（与渲染层房间页同口径）。
    pub async fn unread_tick(&self) -> Result<bool, String> {
        let pocket_ids = self.pocket.followed_ids().await.unwrap_or_default();
        let local_ids = self
            .db
            .with(|db| db.member_flag_ids("followed").unwrap_or_default());
        let blocked = self
            .db
            .with(|db| db.member_flag_ids("blocked").unwrap_or_default());
        let mut followed: Vec<i64> = pocket_ids;
        for id in local_ids {
            if !followed.contains(&id) {
                followed.push(id);
            }
        }
        let changed = self.unread.tick(&self.pocket, &followed, &blocked).await?;
        // 小房间发现排在计数之后：它要读 channels 里已解析的大房间，也要把新频道纳入下一轮计数。
        // 失败只记日志——小房间拿不到不该影响大房间未读。
        let discovered = match self
            .unread
            .discover_small_channels(&self.pocket, &self.room_cache, None, false)
            .await
        {
            Ok(dirty) => dirty,
            Err(err) => {
                crate::app_log::log_line(&format!(
                    "[pocket-unread] 小房间枚举失败（下一轮再试）: {err}"
                ));
                false
            }
        };
        if changed || discovered {
            self.publish_unread();
        }
        Ok(changed || discovered)
    }

    /// 登录成功后立刻补一轮未读，别等下一轮轮询（最坏 60s+）。
    ///
    /// 失败只记日志：拉不到未读不影响登录本身成功。
    fn refresh_unread_after_login(&self) {
        let handle = self.app.get();
        let Some(app) = handle else {
            return;
        };
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let Some(context) = app.try_state::<BridgeContext>() else {
                return;
            };
            if let Err(err) = context.unread_tick().await {
                crate::app_log::log_line(&format!("[pocket-unread] 登录后首轮检测失败: {err}"));
            }
        });
    }

    /// 未读变了就广播给渲染层。
    ///
    /// 未读有两条变化路径（定时轮询、`pocketMarkRead`），都收敛到这一个入口，
    /// 新增变化点只调这里。
    pub fn publish_unread(&self) {
        match to_value(&self.unread.snapshot()) {
            Ok(snapshot) => self.broadcast("pocketUnreadBatch", snapshot),
            Err(err) => crate::app_log::log_line(&format!(
                "[pocket-unread] 快照序列化失败，跳过本轮广播: {err}"
            )),
        }
    }

    /// 聊天室弹幕会话表（测试 / 探针的清理入口）。
    pub fn danmaku(&self) -> &crate::danmaku::DanmakuSessions {
        &self.danmaku
    }

    /// 独立播放窗状态表（窗口生命周期与置顶策略）。
    pub fn float_windows(&self) -> &crate::float_window::FloatWindows {
        &self.floats
    }

    /// 发起请求的窗口：窗口控制通道按它定位，而不是主窗口。
    /// 由 App setup 注入；隔离测试/探针没有 AppHandle，下载进度自然丢弃。
    pub fn set_app_handle(&self, app: AppHandle) {
        self.pocket.set_app_handle(app.clone());
        let _ = self.app.set(app);
    }

    /// 主窗口句柄：窗口壳通道（最小化 / 最大化 / 关闭）与最大化事件用。
    fn main_window(&self) -> Result<tauri::WebviewWindow, String> {
        self.app
            .get()
            .ok_or_else(|| "窗口尚未就绪".to_string())?
            .get_webview_window("main")
            .ok_or_else(|| "主窗口不存在".to_string())
    }

    /// 置顶偏好落到窗口层：`always` 立即生效，`playing` / `never` 不置顶。
    /// 独立播放窗会按同一份偏好初始化，这里先作用于主窗，避免设置「看起来没反应」。
    fn apply_float_topmost(&self, topmost: &str) {
        let always = topmost == "always";
        if let Ok(window) = self.main_window()
            && let Err(err) = window.set_always_on_top(always)
        {
            crate::app_log::log_line(&format!("[float] 置顶设置失败: {err}"));
        }
    }

    fn app_handle(&self) -> Result<AppHandle, String> {
        self.app
            .get()
            .cloned()
            .ok_or_else(|| "窗口尚未就绪".to_string())
    }

    /// 只推给指定窗口（例如图钉状态只属于那一扇窗）。
    fn broadcast_to(&self, label: &str, event: &str, payload: Value) {
        if let Some(app) = self.app.get() {
            use tauri::Emitter;
            if let Some(window) = app.get_webview_window(label) {
                let _ = window.emit(event, payload);
            }
        }
    }

    /// 广播事件到全部窗口（主窗与独立播放窗共用同一份状态）。
    fn broadcast(&self, event: &str, payload: Value) {
        if let Some(app) = self.app.get() {
            use tauri::Emitter;
            let _ = app.emit(event, payload);
        }
    }

    /// 升格归属：只认登录态里的 userId，缺失时退脱敏手机号；未登录 / 身份不全为 None。
    /// 与 Electron `promotionOwnerId(getAuthState())` 同口径。
    fn promotion_owner_id(&self) -> Option<String> {
        let state = self.pocket.auth_state();
        if !state.logged_in {
            return None;
        }
        let owner = if state.user_id.trim().is_empty() {
            state.mobile_masked.trim().to_string()
        } else {
            state.user_id.trim().to_string()
        };
        if owner.is_empty() { None } else { Some(owner) }
    }

    fn require_promotion_owner(&self) -> Result<String, String> {
        self.promotion_owner_id()
            .ok_or_else(|| "登录口袋账号后才能设置升格".to_string())
    }

    /// 系统标准用户目录 + 用户配置的目录（与 Electron `registerSystemIpc` 的放行根一致）。
    fn allowed_open_roots(&self) -> Vec<std::path::PathBuf> {
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .or_else(dirs_home);
        if let Some(home) = home {
            for name in [
                "Desktop",
                "Downloads",
                "Documents",
                "Movies",
                "Pictures",
                "Music",
            ] {
                roots.push(home.join(name));
            }
        }
        if let Some(app) = self.app.get()
            && let Ok(dir) = app.path().app_data_dir()
        {
            roots.push(dir);
        }
        for key in ["downloadDirectory", "ffmpegDirectory"] {
            if let Ok(Value::String(dir)) = self.db.with(|db| db.config(key))
                && !dir.is_empty()
            {
                roots.push(std::path::PathBuf::from(dir));
            }
        }
        roots
    }

    /// `openPath`：先过白名单，再用系统默认程序打开；失败以错误返回（不静默丢失）。
    fn open_path(&self, file_path: &str) -> Result<(), String> {
        let roots = self.allowed_open_roots();
        if !allowed_path::is_path_in_allowed_roots(
            file_path,
            &roots,
            allowed_path::case_insensitive(),
        ) {
            return Err(format!("路径不在允许范围内: {file_path}"));
        }
        let status = crate::platform::open_with_default_app(file_path);
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("打开路径失败: 退出码 {status}")),
            Err(err) => Err(format!("打开路径失败: {err}")),
        }
    }

    /// 把 PNG 字节写进系统剪贴板（经 `tauri-plugin-clipboard-manager`）。
    ///
    /// 以前用 `osascript` / PowerShell 临时文件绕过：既多一次子进程和磁盘写，
    /// Linux 又直接不支持。插件底层是 arboard，三平台同一套代码。
    /// 剪贴板不是主线程安全的（Linux 上在主线程读会死锁），放到阻塞线程池里写。
    async fn copy_png_to_clipboard(&self, bytes: Vec<u8>) -> Result<(), String> {
        let app = self
            .app
            .get()
            .cloned()
            .ok_or_else(|| "复制图片仅在桌面应用内可用".to_string())?;
        let image = crate::image_file::decode_png_to_rgba(&bytes)?;
        tokio::task::spawn_blocking(move || {
            use tauri_plugin_clipboard_manager::ClipboardExt;
            app.clipboard()
                .write_image(&image)
                .map_err(|err| format!("复制图片失败: {err}"))
        })
        .await
        .map_err(|err| format!("复制图片失败: {err}"))?
    }

    /// 原生保存对话框（无父窗口：frameless 主窗在有父窗口的模态对话框期间会刷白）。
    async fn save_png(&self, bytes: Vec<u8>, file_name: String) -> Result<Option<String>, String> {
        let app = self
            .app
            .get()
            .cloned()
            .ok_or_else(|| "保存图片仅在桌面应用内可用".to_string())?;
        let dialog = tokio::task::spawn_blocking(move || {
            use tauri_plugin_dialog::DialogExt;
            app.dialog()
                .file()
                .set_title("保存生日贺卡")
                .set_file_name(&file_name)
                .add_filter("PNG 图片", &["png"])
                .blocking_save_file()
        })
        .await
        .map_err(|err| format!("保存对话框失败: {err}"))?;
        let Some(target) = dialog else {
            return Ok(None);
        };
        let path = target
            .into_path()
            .map_err(|err| format!("保存路径无效: {err}"))?;
        let final_path = image_file::ensure_png_extension(&path.to_string_lossy());
        std::fs::write(&final_path, &bytes).map_err(|err| format!("写入图片失败: {err}"))?;
        Ok(Some(final_path))
    }

    /// App setup / 探针显式启动本地流服务；失败只影响直播，不影响其余通道。
    pub fn start_media(&self) -> Result<u16, String> {
        self.media.start()
    }

    /// 通道分发：args 为位置参数数组，与 Electron `InvokeSpec<[...]>` 一一对应。
    pub async fn dispatch(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        self.dispatch_from(channel, args, None).await
    }

    /// 带发起窗口的通道分发：窗口控制 / 浮窗通道需要知道是哪个窗在问。
    pub async fn dispatch_from(
        &self,
        channel: &str,
        args: &[Value],
        caller: Option<&tauri::WebviewWindow>,
    ) -> Result<Value, String> {
        // 主窗与独立播放窗共用同一份渲染层，但能力面不同：按发起窗口的 label 收敛通道，
        // 避免播放窗（以及将来任何新窗口）悄悄拿到账号 / 文件 / 任务写的全量权限。
        if let Some(window) = caller
            && !channel_allowed_for_window(window.label(), channel)
        {
            return Err(format!(
                "通道 {channel} 不允许在窗口 {} 中调用",
                window.label()
            ));
        }

        match channel {
            // 通用 HTTP 代理：白名单 / 限流 / 身份头都在 net 模块，桥只转发。
            "netRequest" | "imageDataUrl" | "netDiagnose" => {
                channels::net::dispatch(self, channel, args, caller).await
            }
            // 成员库 / 配置 / 升格
            "hasMembers"
            | "getAllMembers"
            | "getMemberInfo"
            | "getMemberFlags"
            | "getConfig"
            | "getMemberTree"
            | "setConfig"
            | "setMemberFlags"
            | "addMemberFlag"
            | "addMemberFlags"
            | "removeMemberFlag"
            | "saveMemberData"
            | "getMemberTeamPromotions"
            | "setMemberTeamPromotion"
            | "removeMemberTeamPromotion" => {
                channels::data::dispatch(self, channel, args, caller).await
            }
            // FFmpeg 环境
            "getFfmpegStatus" | "downloadFfmpeg" | "selectDirectory" => {
                channels::media::dispatch(self, channel, args, caller).await
            }
            // 口袋直播弹幕
            "pocketDanmakuStart"
            | "pocketGetChatroomId"
            | "pocketGetLiveResult"
            | "pocketDanmakuStop"
            | "roomAlbumDownload" => channels::danmaku::dispatch(self, channel, args, caller).await,
            // 房间域（只读）
            "pocketRefreshSmallChannel"
            | "pocketInvalidateSmallChannel"
            | "pocketGetRoomQuestion"
            | "pocketCheckRoomAnswer"
            | "pocketUnreadState"
            | "pocketMarkRead"
            | "pocketGetRoomChannel"
            | "pocketGetRoomChannels"
            | "pocketGetServerChannels"
            | "pocketGetRoomPermissions"
            | "pocketGetRoomMessages"
            | "pocketGetRoomEssenceMessages"
            | "pocketGetRoomLastMessages"
            | "pocketRoomQChatEnter"
            | "pocketRoomQChatLeave" => {
                channels::rooms::dispatch(self, channel, args, caller).await
            }
            // 独立播放窗
            "openFloatWindow"
            | "floatPlayerGetPayload"
            | "floatWindowFitAspect"
            | "floatWindowSetPlaying"
            | "floatWindowGetTopmost"
            | "floatWindowSetTopmost"
            | "floatWindowSetTopmostPreference" => {
                channels::float::dispatch(self, channel, args, caller).await
            }
            // 系统集成
            "openPath" | "openExternal" | "copyImageToClipboard" | "saveImageFile" => {
                channels::system::dispatch(self, channel, args, caller).await
            }
            // 下载 / 录制任务
            "downloadTaskStart" | "downloadTaskStop" | "downloadTaskList"
            | "downloadTaskRemove" | "recordTaskStart" | "recordTaskStop" | "recordTaskList"
            | "recordTaskRemove" => channels::tasks::dispatch(self, channel, args, caller).await,
            // 播放器侧零散通道：下架中转 / B 站弹幕 / B 站房间信息
            "notifyLiveUnavailable" | "danmakuStart" | "danmakuStop" | "bilibiliRoomInfos" => {
                channels::player::dispatch(self, channel, args, caller).await
            }
            // 设置页 / 账号 / 窗口壳
            "pathJoin"
            | "getAppVersion"
            | "feedbackMeta"
            | "submitFeedback"
            | "createLiveStream"
            | "stopLiveStream"
            | "preventSleep"
            | "allowSleep"
            | "pocketSendSms"
            | "pocketLoginByCode"
            | "pocketLoginByToken"
            | "pocketGetAuthState"
            | "pocketLogout"
            | "pocketVerifyLogin"
            | "pocketCheckIn"
            | "pocketGetFollowedIds"
            | "pocketFollowMember"
            | "pocketUnfollowMember"
            | "getAppInfo" => channels::settings::dispatch(self, channel, args, caller).await,

            other => Err(format!("未实现的通道: {other}")),
        }
    }
}

fn arg<T: DeserializeOwned>(args: &[Value], index: usize) -> Result<T, String> {
    serde_json::from_value(args.get(index).cloned().unwrap_or(Value::Null))
        .map_err(|err| format!("参数 #{} 解析失败: {err}", index + 1))
}

/// `openExternal` 的 URL 闸门：只放行 http/https，拒绝 `file:` / `javascript:` 等。
pub(super) fn validate_external_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| format!("URL 无效: {err}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("只允许打开 http/https 链接: {url}"));
    }
    if parsed.host_str().is_none() {
        return Err(format!("URL 缺少主机名: {url}"));
    }
    Ok(parsed)
}

/// 用系统默认浏览器打开外链。
///
/// 参数只通过进程参数传递，不拼 shell，避免 URL 里的 `&` / `|` 被解释成命令分隔符。
pub(super) fn open_external_url(url: &str) -> Result<(), String> {
    let parsed = validate_external_url(url)?;
    // 用规范化后的字符串：URL 里的空格 / 非 ASCII 已被 percent-encode。
    let url = parsed.as_str();
    let status = crate::platform::open_with_default_app(url);
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("打开外链失败: 退出码 {status}")),
        Err(err) => Err(format!("打开外链失败: {err}")),
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("回包序列化失败: {err}"))
}

/// `pathJoin` 的参数来自渲染层位置参数，非字符串一律拒绝（避免把 null 拼进路径）。
/// 推一条事件给指定窗口（订阅者模型：弹幕只发给订阅了该聊天室的窗口）。
fn emit_to_window_label<T: Serialize>(app: &AppHandle, label: &str, event: &str, payload: &T) {
    use tauri::Emitter;
    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    match serde_json::to_value(payload) {
        Ok(value) => {
            let _ = window.emit(event, value);
        }
        Err(err) => crate::app_log::log_line(&format!("[event] {event} 序列化失败: {err}")),
    }
}

fn arg_str(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

/// 系统标准用户目录的锚点（macOS / Linux 看 HOME，Windows 看 USERPROFILE）。
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn context() -> BridgeContext {
        // 每个用例独立目录：并行执行时不能共用设备 ID 文件。
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("d48-bridge-{unique}"));
        let ctx = BridgeContext::new_isolated(&dir).unwrap();
        // 生产环境由 setup 的后台线程装载成员库；测试里同步装好。
        // 数据目录不存在时 `Database::load` 给出可写的空库（与生产首次启动同一条路径）。
        ctx.database_slot()
            .store(crate::database::Database::load(dir.join("database.json")));
        ctx
    }

    #[tokio::test]
    async fn unknown_channel_is_reported() {
        let ctx = context();
        let err = ctx.dispatch("noSuchChannel", &[]).await.unwrap_err();
        assert!(err.contains("未实现的通道"));
    }

    #[tokio::test]
    async fn promotions_require_login_and_open_path_rejects_foreign_paths() {
        let ctx = context();
        // 未登录：读空、写拒绝（身份只由 Rust 推导，渲染层无法伪造 ownerId）
        assert_eq!(
            ctx.dispatch("getMemberTeamPromotions", &[]).await.unwrap(),
            json!([])
        );
        let err = ctx
            .dispatch(
                "setMemberTeamPromotion",
                &[json!({ "userId": 1, "teamName": "A队", "teamColor": "#fff" })],
            )
            .await
            .unwrap_err();
        assert!(err.contains("登录口袋账号后才能设置升格"));

        let err = ctx
            .dispatch("openPath", &[json!("/etc/hosts")])
            .await
            .unwrap_err();
        assert!(err.contains("路径不在允许范围内"));
    }

    #[tokio::test]
    async fn topmost_preference_is_persisted_with_enum_check() {
        let ctx = context();
        ctx.dispatch("floatWindowSetTopmostPreference", &[json!("always")])
            .await
            .unwrap();
        assert_eq!(ctx.debug_config("floatTopmost").unwrap(), json!("always"));
        // 枚举外的取值不落盘
        let err = ctx
            .dispatch("floatWindowSetTopmostPreference", &[json!("bogus")])
            .await
            .unwrap_err();
        assert!(err.contains("Invalid config value"));
        assert_eq!(ctx.debug_config("floatTopmost").unwrap(), json!("always"));
    }

    #[tokio::test]
    async fn image_channels_reject_non_png() {
        let ctx = context();
        let err = ctx
            .dispatch(
                "copyImageToClipboard",
                &[json!("data:image/jpeg;base64,AAAA")],
            )
            .await
            .unwrap_err();
        assert!(err.contains("图片数据格式不正确"));
        let err = ctx
            .dispatch("saveImageFile", &[json!("not-a-data-url"), json!("x.png")])
            .await
            .unwrap_err();
        assert!(err.contains("图片数据格式不正确"));
    }

    #[tokio::test]
    async fn app_info_reports_rust_side() {
        let ctx = context();
        let value = ctx.dispatch("getAppInfo", &[]).await.unwrap();
        assert_eq!(value["deviceId"], ctx.device_id());
        assert_eq!(value["platform"], std::env::consts::OS);
    }

    #[tokio::test]
    async fn missing_argument_is_rejected_before_network() {
        let ctx = context();
        let err = ctx.dispatch("imageDataUrl", &[]).await.unwrap_err();
        assert!(err.contains("参数 #1"));
    }

    #[tokio::test]
    async fn config_write_roundtrips_through_dispatch() {
        let ctx = context();
        ctx.dispatch("setConfig", &[json!("danmakuPlacement"), json!("left")])
            .await
            .unwrap();
        let value = ctx
            .dispatch("getConfig", &[json!("danmakuPlacement")])
            .await
            .unwrap();
        assert_eq!(value, json!("left"));
        assert!(
            ctx.dispatch("setConfig", &[json!("danmakuPlacement"), json!("maybe")])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn image_data_url_rejects_host_outside_whitelist() {
        let ctx = context();
        let err = ctx
            .dispatch("imageDataUrl", &[json!("https://example.com/a.png")])
            .await
            .unwrap_err();
        assert!(err.contains("白名单"));
    }

    #[tokio::test]
    async fn allow_sleep_without_active_blocker_is_noop() {
        let ctx = context();
        let result = ctx
            .dispatch("allowSleep", &[serde_json::json!(12345)])
            .await;
        assert!(result.is_ok(), "释放不存在的防休眠句柄不应报错: {result:?}");
    }

    #[tokio::test]
    async fn select_directory_requires_desktop_app() {
        let ctx = context();
        let err = ctx.dispatch("selectDirectory", &[]).await.unwrap_err();
        assert!(err.contains("桌面应用"));
    }

    #[tokio::test]
    async fn pocket_auth_state_is_logged_out_without_credentials() {
        let ctx = context();
        let value = ctx.dispatch("pocketGetAuthState", &[]).await.unwrap();
        assert_eq!(value["loggedIn"], json!(false));
        assert_eq!(value["clientProfile"], json!("pocket48"));
        assert_eq!(value["mobileMasked"], json!(""));
        assert!(ctx.dispatch("pocketLogout", &[]).await.is_ok());
    }

    #[test]
    fn channel_list_covers_system_surface() {
        for channel in [
            "netRequest",
            "imageDataUrl",
            "netDiagnose",
            "getAppInfo",
            "getFfmpegStatus",
            "downloadFfmpeg",
            "selectDirectory",
            "createLiveStream",
            "stopLiveStream",
            "preventSleep",
            "allowSleep",
            "openPath",
            "openExternal",
            "copyImageToClipboard",
            "saveImageFile",
            "bilibiliRoomInfos",
            "notifyLiveUnavailable",
            "danmakuStart",
            "danmakuStop",
            "openFloatWindow",
            "floatPlayerGetPayload",
            "floatWindowFitAspect",
            "floatWindowSetPlaying",
            "floatWindowGetTopmost",
            "floatWindowSetTopmost",
            "floatWindowSetTopmostPreference",
            "getMemberTeamPromotions",
            "setMemberTeamPromotion",
            "removeMemberTeamPromotion",
            "pocketGetAuthState",
            "pocketSendSms",
            "pocketLoginByCode",
            "pocketLoginByToken",
            "pocketLogout",
            "pocketVerifyLogin",
            "pocketCheckIn",
            "pocketGetFollowedIds",
            "pocketFollowMember",
            "pocketUnfollowMember",
        ] {
            assert!(CHANNELS.contains(&channel));
        }
    }

    #[test]
    fn external_url_gate_only_accepts_http_and_https() {
        for url in ["https://weibo.com/u/123", "http://example.com/a?b=1&c=2"] {
            assert!(validate_external_url(url).is_ok(), "应放行: {url}");
        }
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "ftp://example.com/file",
            "https://",
        ] {
            assert!(validate_external_url(url).is_err(), "应拒绝: {url}");
        }
    }

    #[tokio::test]
    async fn open_external_rejects_non_http_before_spawning_browser() {
        let ctx = context();
        let err = ctx
            .dispatch("openExternal", &[json!("file:///etc/passwd")])
            .await
            .unwrap_err();
        assert!(
            err.contains("http/https"),
            "错误应说明只允许 http/https: {err}"
        );
    }

    #[test]
    fn float_window_channel_scope_is_deny_by_default() {
        // 主窗放行全部；播放窗只放行播放器实际用到的能力。
        assert!(channel_allowed_for_window("main", "pocketLoginByToken"));
        assert!(channel_allowed_for_window("main", "saveImageFile"));

        for channel in [
            "createLiveStream",
            "stopLiveStream",
            "pocketGetChatroomId",
            "pocketDanmakuStart",
            "danmakuStart",
            "downloadTaskStart",
            "recordTaskStart",
            "getConfig",
            "setConfig",
            "openPath",
            "openExternal",
            "floatPlayerGetPayload",
        ] {
            assert!(
                channel_allowed_for_window("live:63", channel),
                "播放窗应允许 {channel}"
            );
            assert!(
                channel_allowed_for_window("playback:63", channel),
                "回放窗应允许 {channel}"
            );
        }

        for channel in [
            "pocketSendSms",
            "pocketLoginByCode",
            "pocketLoginByToken",
            "pocketLogout",
            "pocketCheckIn",
            "pocketFollowMember",
            "setMemberFlags",
            "saveMemberData",
            "saveImageFile",
            "copyImageToClipboard",
            "openFloatWindow",
            "pocketUnreadState",
            "pocketCheckRoomAnswer",
            "feedbackMeta",
        ] {
            assert!(
                !channel_allowed_for_window("live:63", channel),
                "播放窗不应允许 {channel}"
            );
            assert!(
                !channel_allowed_for_window("playback:63", channel),
                "回放窗不应允许 {channel}"
            );
        }

        // 将来新增窗口必须先登记，不能因为 label 不在已知列表里就默认放行。
        assert!(!channel_allowed_for_window("settings", "getConfig"));
    }

    #[test]
    fn float_window_allowlist_entries_exist_in_registry() {
        for channel in FLOAT_WINDOW_CHANNELS {
            assert!(
                CHANNELS.contains(channel),
                "播放窗白名单里的通道未注册: {channel}"
            );
        }
    }
}
