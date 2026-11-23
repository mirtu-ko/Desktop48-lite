//! Tauri 宿主：装配上下文并注册唯一入口 `main_api`。
//!
//! 进程边界从「每个功能一条 Tauri command」改成「一个通用桥 + 通道注册表」，
//! 与 Electron 版 `window.mainAPI` 的位置一一对应：
//! `invoke('main_api', { channel: 'netRequest', args: [options] })`。

pub mod allowed_path;
pub mod api;
pub mod app_log;
pub mod bilibili;
pub mod bilibili_danmaku;
pub mod bridge;
pub mod danmaku;
pub mod database;
pub mod db_slot;
pub mod diagnose;
pub mod download;
pub mod feedback;
pub mod ffmpeg;
pub mod float_chrome;
pub mod float_window;
pub mod identity;
pub mod image_file;
pub mod media;
pub mod migrate;
pub mod net;
pub mod nim;
pub mod nim_chatroom;
pub mod nim_qchat;
pub mod nim_session;
pub mod platform;
pub mod pocket;
pub mod power_save;
pub mod room;
pub mod room_album;
pub mod room_qchat;
pub mod tasks;
pub mod unread;
pub mod window_geometry;
pub mod windows;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bridge::BridgeContext;
use serde_json::Value;
use tauri::{Emitter, Manager, State};

/// 唯一的 IPC 入口：`channel` + 位置参数数组。
///
/// 保留 Tauri command 的自由度（可被 `probe` 之外的脚本直接调用），
/// 但通道实现全部集中在 `bridge::BridgeContext::dispatch`。
#[tauri::command]
async fn main_api(
    window: tauri::WebviewWindow,
    state: State<'_, BridgeContext>,
    channel: String,
    args: Option<Vec<Value>>,
) -> Result<Value, String> {
    let args = args.unwrap_or_default();
    let started = Instant::now();
    // 带上发起窗口：主窗与独立播放窗共用同一份渲染层，窗口控制 / 浮窗通道必须知道是哪个窗。
    let result = state.dispatch_from(&channel, &args, Some(&window)).await;
    let elapsed = started.elapsed();
    // 成功不落盘：一次页面加载就有几十到上百条 IPC，成功行只会把 main.log 撑满。
    // 失败必记；慢调用（超过阈值）也记，用来定位卡顿来源。
    match &result {
        Ok(_) => {
            if elapsed >= SLOW_IPC_THRESHOLD {
                app_log::log_line(&format!(
                    "[ipc] {channel} slow {}ms (args: {})",
                    elapsed.as_millis(),
                    args.len()
                ));
            }
        }
        Err(err) => app_log::log_line(&format!(
            "[ipc] {channel} err {}ms: {err}",
            elapsed.as_millis()
        )),
    }
    result
}

/// 慢调用阈值：正常通道多在 1–50ms，超过 200ms 值得进日志。
const SLOW_IPC_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(200);

/// 给一个无边框窗口接上最大化状态广播（主窗与独立播放窗共用）。
pub fn wire_maximize_events(window: tauri::WebviewWindow) {
    let label = window.label().to_string();
    let last = Arc::new(AtomicBool::new(window.is_maximized().unwrap_or(false)));
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Resized(_) => {
            if let Some(window) = handle.get_webview_window(&label) {
                let now = window.is_maximized().unwrap_or(false);
                if last.swap(now, Ordering::Relaxed) != now {
                    let _ = window.emit("windowOnMaximizeChange", now);
                    // 最大化视同用户接管尺寸（否则后续 aspect 定形会把最大化抹掉）。
                    // 主窗没有 float 记录，这一步对它自然是空操作。
                    if now && let Some(state) = handle.try_state::<BridgeContext>() {
                        state.float_windows().mark_user_resized(&label);
                    }
                }
            }
        }
        // 显示器插拔 / 缩放变化后把窗口收回可见区（只在越界时动，不覆盖用户调过的尺寸）。
        // 与 Electron 的 display-metrics-changed 同语义。
        tauri::WindowEvent::ScaleFactorChanged { .. } => {
            if let Some(window) = handle.get_webview_window(&label) {
                let _ = &last;
                window_geometry::refit_main_window(&window);
            }
        }
        _ => {}
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        // 单实例必须在最前注册：第二实例在主进程初始化前就被拦下，
        // 只做一件事——把已在运行的主窗唤回前台，然后退出。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            windows::show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            // 日志先于一切：打包版没有终端，启动期的失败只能靠这份文件复盘。
            let log_dir = app_log::log_dir_from(&data_dir);
            app_log::install(&log_dir);
            app_log::log_line(&format!("[boot] 日志: {}", log_dir.display()));
            let context = BridgeContext::new(&data_dir).map_err(std::io::Error::other)?;
            context.set_app_handle(app.handle().clone());
            if let Err(err) = context.start_media() {
                // 端口耗尽只影响直播播放；其余功能照常可用，createLiveStream 会带错误回渲染层。
                app_log::log_line(&format!("[media] 启动失败: {err}"));
            }
            // 成员库此刻仍在后台装载（人数见稍后的 `[db]`），这里不打人数。
            app_log::log_line(&format!(
                "[boot] 设备 {} · 已注册通道: {}",
                context.device_id(),
                bridge::CHANNELS.join(", ")
            ));
            app.manage(context);

            // 先按**已落盘**的未读快照刷一次：未读状态是持久化的（`pocket48-unread.json`），
            // 冷启动时就有内容，不该等到 8 秒后的首轮探测。
            if let Some(context) = app.try_state::<BridgeContext>() {
                context.publish_unread();

                // 成员库（1.6 MB）放后台解析。凭证已在 `BridgeContext::new` 里同步读完
                // （钥匙串授权在那一步就发生了，见 `PocketAccount::load`）。
                let handle = app.handle().clone();
                let db_path = data_dir.join("database.json");
                tauri::async_runtime::spawn_blocking(move || {
                    let started = Instant::now();
                    let db = crate::database::Database::load(&db_path);
                    let members = db.member_count();
                    if let Some(context) = handle.try_state::<BridgeContext>() {
                        context.database_slot().store(db);
                        app_log::log_line(&format!(
                            "[db] 成员库已装载：{members} 人 · {}ms",
                            started.elapsed().as_millis()
                        ));
                    }
                });
            }

            // 下载 / 录制任务事件：广播给全部窗口
            // （任务列表镜像在每个窗口里各一份，播放窗发起的录制主窗也要看到进度）
            {
                let handle = app.handle().clone();
                if let Some(context) = handle.try_state::<BridgeContext>()
                    && let Some(mut receiver) = context.tasks().subscribe()
                {
                    tauri::async_runtime::spawn(async move {
                        use tauri::Emitter;
                        loop {
                            match receiver.recv().await {
                                Ok(event) => {
                                    let (name, payload) = crate::tasks::event_payload(&event);
                                    let _ = handle.emit(&name, payload);
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(_) => return,
                            }
                        }
                    });
                }
            }

            // 房间未读检测：Rust 是唯一真相（页面没开 / 最小化时照样积累），
            // 变化经 pocketUnreadBatch 全量广播给所有窗口；登录态变化时立刻补一轮。
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // 启动后先等一轮，别和首屏的其它请求抢出口
                    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                    // 每轮重新取 State，且**不让 State 跨 await**：
                    // State 是带生命周期的借用，跨 await 持有会与 setup 里的其它访问互斥。
                    // 未读有变化时 unread_tick 内部会调 publish_unread()
                    // 刷新渲染层，这里不再重复广播。
                    while let Some(context) = handle.try_state::<BridgeContext>() {
                        if context.pocket().authorized()
                            && let Err(err) = context.unread_tick().await
                        {
                            app_log::log_line(&format!(
                                "[pocket-unread] 本轮检测失败（下一轮再试）: {err}"
                            ));
                        }
                        tokio::time::sleep(unread::POLL_INTERVAL).await;
                    }
                });
            }

            // 主窗口几何：按当前工作区计算尺寸并居中（对照 Electron `computeWindowBounds` + `centerWindow`）。
            // tauri.conf.json 里的尺寸只是「拿不到显示器时」的兜底值。
            if let Some(window) = app.get_webview_window("main") {
                // Windows 借不到 macOS 那套 Overlay 标题栏（`titleBarStyle` / `hiddenTitle`
                // / `trafficLightPosition` 都是 macOS 专有字段，在 Windows 上被忽略）：
                // 保留系统装饰就会在渲染层自绘标题栏之上再叠一条原生标题栏，即「双层标题栏」。
                // 窗口此时还没上屏（`visible: false`），去掉装饰不会闪一下；
                // 标题栏、拖拽、最小化 / 最大化 / 关闭全部由 AppTitleBar 负责，与 macOS 观感一致。
                // `shadow: true` 仍生效：系统会给无装饰窗口补 1px 边框，Win11 上还有圆角；
                // tao 也为无装饰窗口保留可缩放边缘（tauri-runtime-wry 的 undecorated_resizing）。
                #[cfg(target_os = "windows")]
                let _ = window.set_decorations(false);

                let (size, (x, y)) = window_geometry::initial_main_geometry(app.handle());
                let _ = window.set_min_size(Some(tauri::LogicalSize::new(
                    size.min_width,
                    size.min_height,
                )));
                let _ = window.set_position(tauri::LogicalPosition::new(x, y));
                let _ = window.set_size(tauri::LogicalSize::new(size.width, size.height));

                // 钥匙串早已在 `BridgeContext::new` 里读完：用户此刻已经点过「同意 / 拒绝」，
                // 现在才让窗口上屏，第一屏就是定好尺寸、定好位置的应用本身。
                // （tauri.conf.json 里 `visible: false` 建窗就是为这一步）
                if let Err(err) = window.show() {
                    app_log::log_line(&format!("[boot] 主窗显示失败: {err}"));
                }

                // 标题栏不再自己调 AppKit：交通灯位置在 tauri.conf.json 的
                // `trafficLightPosition` 里给（Overlay 标题栏下 Tauri 用 wry 的
                // traffic light inset 推走按钮），标题栏材质则用 Tauri 的窗口效果接口。
                // 这里显式设一次无效果，等于告诉 Tauri「不要材质」：以前靠
                // setTitlebarAppearsTransparent 抹掉的那层半透明背景，现在由这一句负责。
                #[cfg(target_os = "macos")]
                let _ = window.set_effects(None::<tauri::utils::config::WindowEffectsConfig>);
            }

            // 标题栏最大化按钮的图标随窗口状态切换：Resized 时查询当前窗口，
            // 只在状态真的变化时广播 `windowOnMaximizeChange`（避免拖动边框狂发事件）。
            // 主窗与独立播放窗都要接（播放窗同样是无边框自定义标题栏）。
            if let Some(window) = app.get_webview_window("main") {
                wire_maximize_events(window);
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![main_api])
        .build(tauri::generate_context!())
        .expect("Tauri 应用启动失败");

    app.run(|handle, event| {
        // macOS 点击 Dock 图标（应用在后台但没有可见窗口）时唤回主窗。
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            windows::show_main_window(handle);
        }

        // 日志写线程异步落盘：退出前给一个确认点，保住最后几行（日志记录的正是退出原因）。
        // 必须在清理块之前：清理过程本身也会写日志。
        if matches!(event, tauri::RunEvent::Exit) {
            app_log::flush();
        }

        // 窗口关闭后进程可能很快退出，先撤销会话、让转流任务收尾并杀掉 FFmpeg。
        // `kill_on_drop` 只覆盖任务被正常丢弃的情况，不能替代这里的显式清理。
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) && let Some(context) = handle.try_state::<BridgeContext>()
        {
            context.media().cleanup();
            context.sleep().cleanup();
            context.danmaku().cleanup();
            // B 站弹幕是异步会话，退出时只能标记关闭（连接随进程结束）
            let bilibili = context.bilibili_danmaku();
            tauri::async_runtime::block_on(bilibili.cleanup());
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}
