//! 独立播放窗（对照 Electron `src/main/window/float-window.ts`）。
//!
//! 按 `kind:liveId` 去重；载荷在这里集中持有，渲染端经 `floatPlayerGetPayload` 回取
//! （hash 只带短 key）。窗口控制按「发起窗口」定位——多窗共用同一份渲染层。
//! 几何（初始尺寸 / 级联 / 自动定形 / 置顶）都是纯函数，与 `common/float-window.ts` 对应。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::app_log::log_line;

/// 播放窗的 hash 路由路径（不含前导 `#`），与主窗口路由表不冲突。
pub const FLOAT_WINDOW_HASH_PATH: &str = "/float";
/// 标题栏高度上端值，同时是主窗口标题栏的固定高度。
const FLOAT_BAR_HEIGHT: f64 = 36.0;
/// 窄窗下的标题栏高度。
const FLOAT_BAR_HEIGHT_MIN: f64 = 30.0;
const FLOAT_BAR_WIDTH_START: f64 = 320.0;
const FLOAT_BAR_WIDTH_END: f64 = 900.0;
/// 视频元数据未就绪时的兜底宽高比（多数直播为竖屏）。
const FLOAT_DEFAULT_ASPECT: f64 = 9.0 / 16.0;
const FLOAT_MIN_WIDTH: f64 = 240.0;
const FLOAT_MIN_HEIGHT: f64 = 160.0;
const FLOAT_INITIAL_MIN_WIDTH_PORTRAIT: f64 = 320.0;
const FLOAT_INITIAL_MIN_WIDTH_LANDSCAPE: f64 = 680.0;
const FLOAT_EDGE_MARGIN: f64 = 24.0;
const FLOAT_CASCADE_STEP: f64 = 28.0;
/// 判定「用户手动缩放过」的尺寸容差（DIP）。
const RESIZE_DETECT_EPSILON: f64 = 2.0;

// 初始尺寸策略的锚点（1280×800 / 1920×1080 两档之间线性插值）
const COMPACT_W: f64 = 1280.0;
const COMPACT_H: f64 = 800.0;
const REGULAR_W: f64 = 1920.0;
const REGULAR_H: f64 = 1080.0;
const COMPACT_WIDTH_RATIO: f64 = 0.53;
const REGULAR_WIDTH_RATIO: f64 = 0.45;
const COMPACT_HEIGHT_RATIO: f64 = 0.8;
const REGULAR_HEIGHT_RATIO: f64 = 0.7;

/// 播放载荷（与 `ipc-contract.d.ts` 的 `FloatPlayerPayload` 同形）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FloatPlayerPayload {
    pub live_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub start_time: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_type: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_mode: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bilibili_room_id: Option<i64>,
}

/// 窗口矩形（DIP）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

/// 单个播放窗的可变状态（与 Electron `FloatWindowRecord` 同字段）。
#[derive(Debug)]
struct RecordState {
    payload: FloatPlayerPayload,
    user_resized: bool,
    auto_fit_settled: bool,
    last_programmatic_size: Option<Size>,
    playing: bool,
    topmost: String,
    applied: bool,
}

#[derive(Default)]
pub struct FloatWindows {
    /// 只存渲染层可见的状态；窗口句柄按 label 从 AppHandle 现取（避免持有已销毁窗口）。
    records: Mutex<HashMap<String, RecordState>>,
}

impl FloatWindows {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, HashMap<String, RecordState>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 已打开的播放窗数量（级联摆放的档位）。
    pub fn count(&self) -> usize {
        self.guard().len()
    }

    /// 按 `kind:liveId` 查载荷（窗口 reload 后仍可用）。
    pub fn payload(&self, kind: &str, live_id: &str) -> Option<FloatPlayerPayload> {
        self.guard()
            .get(&key_of(kind, live_id))
            .map(|record| record.payload.clone())
    }

    /// 本窗尺寸交给自动定形（元数据到达 / 旋屏）。
    pub fn fit_aspect(&self, label: &str, aspect: f64) -> bool {
        if aspect.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return false;
        }
        self.decide_auto_fit(label)
    }

    /// 用户手动缩放窗口（或最大化）：之后不再自动定形。
    pub fn mark_user_resized(&self, label: &str) {
        let mut guard = self.guard();
        if let Some(record) = guard.get_mut(label) {
            record.user_resized = true;
            record.auto_fit_settled = true;
        }
    }

    /// 窗口刚变成 `size`：判断这是程序设置的结果还是用户拖出来的。
    ///
    /// `Resized` 是异步送达的，事件到达时窗口早就是新值了，所以不能拿当前尺寸和「上次
    /// 程序设置尺寸」比。判据是这次事件**落在我刚设置的尺寸上吗**：落上了是程序干的，
    /// 落不上就是用户拖的（或缩放动画的中间帧）。落定之前一律不算。
    pub fn note_resized(&self, label: &str, size: Size) {
        let mut guard = self.guard();
        let Some(record) = guard.get_mut(label) else {
            return;
        };
        if !record.auto_fit_settled {
            return;
        }
        match record.last_programmatic_size {
            Some(expected)
                if (size.w - expected.w).abs() <= RESIZE_DETECT_EPSILON
                    && (size.h - expected.h).abs() <= RESIZE_DETECT_EPSILON =>
            {
                // 这就是程序刚设的尺寸：消费掉，避免下一次用户真的拖到这个尺寸时被误认
                record.last_programmatic_size = None;
            }
            // 没有期望值 = 用户正在自由缩放
            _ => record.user_resized = true,
        }
    }

    /// 某窗口当前的置顶策略（图钉初值）。
    pub fn topmost_of(&self, label: &str) -> Option<String> {
        self.guard().get(label).map(|record| record.topmost.clone())
    }

    /// 记录一次播放状态上报，返回 (是否变化, 目标置顶态)。
    pub fn set_playing(&self, label: &str, playing: bool) -> Option<bool> {
        let mut guard = self.guard();
        let record = guard.get_mut(label)?;
        record.playing = playing;
        let desired = resolve_float_topmost(&record.topmost, record.playing);
        if record.applied == desired {
            return None;
        }
        record.applied = desired;
        Some(desired)
    }

    /// 改本窗策略（图钉）：只作用本窗，不落盘。返回是否需要下发新层级。
    pub fn set_topmost(&self, label: &str, topmost: &str) -> Option<(bool, bool)> {
        let mut guard = self.guard();
        let record = guard.get_mut(label)?;
        if record.topmost == topmost {
            return Some((false, record.applied));
        }
        record.topmost = topmost.to_string();
        let desired = resolve_float_topmost(&record.topmost, record.playing);
        let changed = record.applied != desired;
        record.applied = desired;
        Some((changed, desired))
    }

    /// 全局偏好变化后作用到所有已开窗：返回每个窗口的目标置顶态。
    pub fn apply_preference(&self, preference: &str) -> Vec<(String, bool)> {
        let mut guard = self.guard();
        let mut updates = Vec::new();
        for (label, record) in guard.iter_mut() {
            record.topmost = preference.to_string();
            let desired = resolve_float_topmost(&record.topmost, record.playing);
            record.applied = desired;
            updates.push((label.clone(), desired));
        }
        updates
    }

    fn insert(&self, label: String, payload: FloatPlayerPayload, size: Size, topmost: String) {
        let mut guard = self.guard();
        guard.insert(
            label,
            RecordState {
                payload,
                user_resized: false,
                auto_fit_settled: false,
                last_programmatic_size: Some(size),
                playing: false,
                applied: initial_float_topmost(&topmost),
                topmost,
            },
        );
    }

    /// 结算一次自动定形，返回是否应当定形，并把结果写回 `user_resized`。
    ///
    /// 只判定、不记账：期望尺寸由调用方在 `set_size` 之前写好，这里若改写成定形前的尺寸，
    /// 下次定形会被判成用户拖拽（表现为「第一次旋转能定形、第二次不跟手」）。
    pub fn decide_auto_fit(&self, label: &str) -> bool {
        let mut guard = self.guard();
        let Some(record) = guard.get_mut(label) else {
            return false;
        };
        let (user_resized, fit) = decide_auto_fit(record.user_resized, record.auto_fit_settled);
        record.user_resized = user_resized;
        if fit {
            record.auto_fit_settled = true;
        }
        fit
    }

    /// 窗口销毁后清掉记录（由 `WindowEvent::Destroyed` 触发）。
    pub fn forget(&self, label: &str) {
        self.guard().remove(label);
    }

    /// 记下程序刚设置的尺寸：`Resized` 事件据此区分「程序改的」与「用户拖的」。
    pub fn set_programmatic_size(&self, label: &str, size: Size) {
        let mut guard = self.guard();
        if let Some(record) = guard.get_mut(label) {
            record.last_programmatic_size = Some(size);
        }
    }
}

fn key_of(kind: &str, live_id: &str) -> String {
    // label 只允许字母数字与 `-` `_` `:` `+` `.`，且要能从 label 反解出 kind + liveId。
    // 用 `:` 作分隔符，kind 取固定两值，liveId 原样保留（纯数字）。
    format!("{kind}:{live_id}")
}

/// 窗口 label → (kind, liveId)。
pub fn split_label(label: &str) -> Option<(&str, &str)> {
    let (kind, live_id) = label.split_once(':')?;
    if live_id.is_empty() || (kind != "live" && kind != "playback") {
        return None;
    }
    Some((kind, live_id))
}

// ===== 几何：纯函数（与 common/float-window.ts 一一对应） =====

fn lerp_ratio(value: f64, compact_anchor: f64, regular_anchor: f64, a: f64, b: f64) -> f64 {
    let progress = ((value - compact_anchor) / (regular_anchor - compact_anchor)).clamp(0.0, 1.0);
    a + (b - a) * progress
}

/// 浮窗标题栏高度：由窗口宽度决定（窄窗栏矮、宽窗回到 36）。
pub fn float_bar_height(window_width: f64) -> f64 {
    let progress = ((window_width - FLOAT_BAR_WIDTH_START)
        / (FLOAT_BAR_WIDTH_END - FLOAT_BAR_WIDTH_START))
        .clamp(0.0, 1.0);
    (FLOAT_BAR_HEIGHT_MIN + (FLOAT_BAR_HEIGHT - FLOAT_BAR_HEIGHT_MIN) * progress).round()
}

/// 在 box 内为给定宽高比内接出最大矩形（宽优先，超高再回缩）。
pub fn fit_aspect_in_box(aspect: f64, box_w: f64, box_h: f64) -> Size {
    let mut w = box_w;
    let mut h = w / aspect;
    if h > box_h {
        h = box_h;
        w = h * aspect;
    }
    Size {
        w: w.round(),
        h: h.round(),
    }
}

fn initial_min_video_width(aspect: f64) -> f64 {
    if aspect < 1.0 {
        FLOAT_INITIAL_MIN_WIDTH_PORTRAIT
    } else {
        FLOAT_INITIAL_MIN_WIDTH_LANDSCAPE
    }
}

/// 计算播放窗尺寸（含标题栏）：视频区在比例框内内接，结果不小于最小尺寸。
/// 栏高由宽度决定而宽度又受可用高度（含栏高）约束，故迭代到不动点（最多 4 轮）。
pub fn compute_initial_window_size(aspect: f64, area: Area) -> Size {
    let min_video_w = initial_min_video_width(aspect);
    let width_ratio = lerp_ratio(
        area.width,
        COMPACT_W,
        REGULAR_W,
        COMPACT_WIDTH_RATIO,
        REGULAR_WIDTH_RATIO,
    );
    let height_ratio = lerp_ratio(
        area.height,
        COMPACT_H,
        REGULAR_H,
        COMPACT_HEIGHT_RATIO,
        REGULAR_HEIGHT_RATIO,
    );

    let layout = |bar: f64| -> Size {
        // 视频可用区 = 窗口宽 × (窗口高 - 栏高)：窗口不再留自绘投影的内边距
        let chrome_w = 0.0;
        let chrome_h = bar;
        let max_w = (area.width * width_ratio)
            .max(min_video_w)
            .min(area.width - FLOAT_EDGE_MARGIN * 2.0 - chrome_w);
        let max_h = ((area.height * height_ratio).round() - chrome_h)
            .max((min_video_w / aspect).round())
            .min(area.height - chrome_h - FLOAT_EDGE_MARGIN * 2.0);
        let video = fit_aspect_in_box(aspect, max_w, max_h);
        Size {
            w: (video.w + chrome_w).max(FLOAT_MIN_WIDTH),
            h: (video.h + chrome_h).max(FLOAT_MIN_HEIGHT),
        }
    };

    let mut size = layout(FLOAT_BAR_HEIGHT);
    for _ in 0..4 {
        let next = layout(float_bar_height(size.w));
        if (next.w - size.w).abs() < f64::EPSILON && (next.h - size.h).abs() < f64::EPSILON {
            break;
        }
        size = next;
    }
    // 窗口尺寸取整：窗口只能落在整像素上，而 `video + gutter * 2 + bar` 几乎总带小数。
    // 一律**向上**取整 —— 多出的零点几像素由 gutter（透明留白）吸收，
    // 视频区仍然不小于需要值，`contain` 不会留黑边；向下取整则必然差一条缝。
    Size {
        w: size.w.ceil().max(FLOAT_MIN_WIDTH),
        h: size.h.ceil().max(FLOAT_MIN_HEIGHT),
    }
}

/// 结算本次是否应当自动定形：落定前无条件定形并清零 `user_resized`，落定后只看它。
pub fn decide_auto_fit(user_resized: bool, auto_fit_settled: bool) -> (bool, bool) {
    if !auto_fit_settled {
        return (false, true);
    }
    (user_resized, !user_resized)
}

/// 置顶策略 × 播放状态 → 目标置顶态。
pub fn resolve_float_topmost(preference: &str, playing: bool) -> bool {
    match preference {
        "always" => true,
        "playing" => playing,
        _ => false,
    }
}

/// 建窗瞬间的置顶态：`never` 照办，其余先顶上（免得被主窗口盖住像没打开）。
pub fn initial_float_topmost(preference: &str) -> bool {
    preference != "never"
}

/// 级联摆放：第 0 个贴右上角，后续向左下错开一步，用尽后回绕。
pub fn resolve_float_window_position(area: Area, size: Size, cascade_index: usize) -> (f64, f64) {
    let base_x = area.x + area.width - size.w - FLOAT_EDGE_MARGIN;
    let base_y = area.y + FLOAT_EDGE_MARGIN;
    let room = (area.width - size.w - FLOAT_EDGE_MARGIN * 2.0)
        .min(area.height - size.h - FLOAT_EDGE_MARGIN * 2.0)
        .max(0.0);
    let max_steps = (room / FLOAT_CASCADE_STEP).floor() + 1.0;
    let offset = (cascade_index as f64 % max_steps) * FLOAT_CASCADE_STEP;
    (
        (base_x - offset).max(area.x).round(),
        (base_y + offset)
            .min((area.y + area.height - size.h).max(area.y))
            .round(),
    )
}

// ===== 窗口生命周期 =====

/// 主窗口所在显示器的工作区；主窗不可用时退回主显示器。
pub fn anchor_area(app: &AppHandle) -> Area {
    let monitor = app
        .get_webview_window("main")
        .and_then(|window| window.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    match monitor {
        Some(monitor) => {
            let work = monitor.work_area();
            let scale = monitor.scale_factor();
            Area {
                x: work.position.x as f64 / scale,
                y: work.position.y as f64 / scale,
                width: work.size.width as f64 / scale,
                height: work.size.height as f64 / scale,
            }
        }
        None => Area {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        },
    }
}

/// 打开独立播放窗；同一 `kind:liveId` 已存在时聚焦复用。
pub fn open(
    app: &AppHandle,
    windows: &FloatWindows,
    kind: &str,
    payload: FloatPlayerPayload,
    topmost_preference: &str,
) -> Result<(), String> {
    let label = key_of(kind, &payload.live_id);
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.unminimize();
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }

    let area = anchor_area(app);
    let size = compute_initial_window_size(FLOAT_DEFAULT_ASPECT, area);
    let (x, y) = resolve_float_window_position(area, size, windows.count());
    let initial_topmost = initial_float_topmost(topmost_preference);

    // 同一份 index.html，靠 hash 分流到播放器根组件；hash 只带短 key，
    // 全量载荷由渲染端经 floatPlayerGetPayload 回取。
    let url = WebviewUrl::App(format!("index.html#{}", float_hash(kind, &payload.live_id)).into());
    // `mut` 只有 macOS 分支用到（补 Overlay 标题栏），其它平台要显式放行 unused_mut
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut builder = WebviewWindowBuilder::new(app, &label, url)
        .title(if kind == "live" { "直播" } else { "回放" })
        // 逻辑尺寸（DIP）：与几何计算同单位，Tauri 会按显示器缩放折算成物理像素
        .inner_size(size.w, size.h)
        .position(x, y)
        .min_inner_size(FLOAT_MIN_WIDTH, FLOAT_MIN_HEIGHT)
        // 未激活时第一下点击就作用于内容（macOS 默认要点两次才能拖）：
        // 不加这个，窗口在后台时拖标题栏毫无反应，必须先点一下激活
        .accept_first_mouse(true)
        // 与主窗同一套自绘标题栏（`fw-bar` 复用 `app-title-bar` 配方）：Windows 上留系统装饰
        // 会叠成双层标题栏，又不能像 macOS 那样借 Overlay 标题栏藏标题，所以只有 Windows 真正无装饰。
        // macOS 保持 `true` 的理由见下面 cfg 分支里的 Titled 身份说明。
        .decorations(!cfg!(target_os = "windows"))
        .resizable(true);

    // 自定义标题栏但保留 Titled 身份：tao 的 `decorations(false)` 会把 styleMask 换成
    // Borderless，窗口就不是标准窗口了（程序坞右键 / 调度中心不列它）。
    // 用 Overlay 标题栏 + 隐藏标题与按钮，视觉与无边框一致。
    // 这两个方法只在 macOS 上存在，写在链式调用里会让其它平台编译失败。
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }

    let window = builder
        // 三个系统按钮不交给系统：窗口仍可缩放，动作走自绘标题栏的 window* 通道。
        // 注意这些开关在 macOS 上只影响样式掩码 / 禁用态，真正隐藏交通灯见
        // `float_chrome::hide_macos_traffic_lights`。
        .closable(false)
        .minimizable(false)
        .maximizable(false)
        // 可全屏 ≠ 建窗即全屏：Electron 里 fullscreenable: true 只是「允许进入全屏」，
        // 初值仍是普通窗口。这里必须显式 false，否则窗口一出现就占满屏幕。
        .fullscreen(false)
        .always_on_top(initial_topmost)
        // 窗口不透明，窗影交给系统（macOS 原生窗影跟随缩放 / 阴影边缘不占窗口区域）。
        // 以前是「透明窗口 + 渲染层自绘投影」，但 macOS 只在窗口矩形上判断命中区域，
        // 自绘投影那圈内边距同样是窗口的一部分：拖拽 / 缩放都落在阴影边缘上。
        // 代价是圆角改由系统裁剪，渲染层不再画圆角与投影。
        .transparent(false)
        .shadow(true)
        .visible(false)
        .build()
        .map_err(|err| format!("创建播放窗失败: {err}"))?;

    windows.insert(label.clone(), payload, size, topmost_preference.to_string());

    // 用户手动缩放 / 最大化后不再自动改尺寸
    let handle = app.clone();
    let label_for_events = label.clone();
    window.on_window_event(move |event| match event {
        // Tauri 没有 Electron 的 `will-resize`（只响应用户拖拽、程序化设置不触发），
        // 所以自己记下「程序设置的尺寸」，把窗口当前尺寸与之比对来反推用户缩放。
        // 这一步不能省：漏了它，`floatWindowFitAspect` 第二次调用时就会把
        // 「上一次定形后的尺寸」当成用户拖拽，从此拒绝再自动定形（旋屏不跟手）。
        tauri::WindowEvent::Resized(size) => {
            if let Some(state) = handle.try_state::<crate::bridge::BridgeContext>() {
                // 事件给的是物理像素，record 里存的是逻辑像素
                let scale = handle
                    .get_webview_window(&label_for_events)
                    .and_then(|window| window.scale_factor().ok())
                    .unwrap_or(1.0);
                let current = Size {
                    w: size.width as f64 / scale,
                    h: size.height as f64 / scale,
                };
                state
                    .float_windows()
                    .note_resized(&label_for_events, current);
            }
            // AppKit 在离开全屏 / 改标题时会复位交通灯，尺寸变化时补一次（幂等）。
            if let Some(window) = handle.get_webview_window(&label_for_events) {
                crate::float_chrome::hide_macos_traffic_lights(&window);
            }
        }
        tauri::WindowEvent::Destroyed => {
            if let Some(state) = handle.try_state::<crate::bridge::BridgeContext>() {
                state.float_windows().forget(&label_for_events);
                // 播放窗关闭不触发渲染层退订：连带退掉它订阅的弹幕会话
                state.danmaku().stop_window(&label_for_events);
            }
            log_line(&format!("[float] 播放窗已关闭: {label_for_events}"));
        }
        _ => {}
    });
    crate::wire_maximize_events(window.clone());
    window
        .show()
        .map_err(|err| format!("显示播放窗失败: {err}"))?;
    // 上屏后才拿得到标准按钮。
    crate::float_chrome::hide_macos_traffic_lights(&window);
    log_line(&format!("[float] 已创建独立播放窗: {label}"));
    Ok(())
}

/// 渲染层 hash：`/float?kind=live&liveId=xxx`（liveId 做 percent 编码）。
fn float_hash(kind: &str, live_id: &str) -> String {
    format!(
        "{FLOAT_WINDOW_HASH_PATH}?kind={kind}&liveId={}",
        encode_query(live_id)
    )
}

fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_height_follows_width_curve() {
        assert_eq!(float_bar_height(320.0), 30.0);
        assert_eq!(float_bar_height(900.0), 36.0);
        // 两端之外钳住
        assert_eq!(float_bar_height(200.0), 30.0);
        assert_eq!(float_bar_height(1400.0), 36.0);
        // 单调不减
        let mut previous = 0.0;
        for width in (300..1000).step_by(50) {
            let height = float_bar_height(width as f64);
            assert!(height >= previous);
            previous = height;
        }
    }

    #[test]
    fn aspect_box_fits_inside() {
        let portrait = fit_aspect_in_box(9.0 / 16.0, 400.0, 200.0);
        assert_eq!(portrait.h, 200.0);
        assert!(portrait.w <= 400.0);
        let landscape = fit_aspect_in_box(16.0 / 9.0, 600.0, 400.0);
        assert_eq!(landscape.w, 600.0);
        assert!(landscape.h <= 400.0);
    }

    #[test]
    fn initial_size_is_monotonic_across_work_areas() {
        let small = compute_initial_window_size(
            FLOAT_DEFAULT_ASPECT,
            Area {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        );
        let large = compute_initial_window_size(
            FLOAT_DEFAULT_ASPECT,
            Area {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
        );
        // 竖屏：越高的工作区给出越高的窗口，且始终在工作区内
        assert!(large.h >= small.h);
        assert!(small.w >= FLOAT_MIN_WIDTH && small.h >= FLOAT_MIN_HEIGHT);
        assert!(large.h <= 1080.0);
    }

    #[test]
    fn hidden_size_stays_inside_work_area() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 720.0,
        };
        let size = compute_initial_window_size(16.0 / 9.0, area);
        assert!(size.w <= area.width);
        assert!(size.h <= area.height);
    }

    #[test]
    fn auto_fit_respects_user_resize() {
        // 未落定：无条件定形并清零 user_resized
        assert_eq!(decide_auto_fit(true, false), (false, true));
        // 落定后、用户没拖过：继续自动定形
        assert_eq!(decide_auto_fit(false, true), (false, true));
        // 用户拖过：不再自动定形
        assert_eq!(decide_auto_fit(true, true), (true, false));
    }

    fn sample_payload() -> FloatPlayerPayload {
        FloatPlayerPayload {
            live_id: "16285946319".to_string(),
            nickname: None,
            title: "直播".to_string(),
            start_time: 0.0,
            live_type: None,
            live_mode: None,
            source: None,
            avatar: None,
            bilibili_room_id: None,
        }
    }

    /// 窗口尺寸要留出 gutter + 标题栏，否则 `object-fit: contain` 会留黑边。
    /// 这里按渲染层的实际约束复算：视频区 = 窗口 - 2*gutter - bar，
    /// 视频放进去必须**恰好**内接（两轴至少一边顶满，另一边不溢出）。
    #[test]
    fn window_size_leaves_room_for_gutter_and_bar() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1140.0,
        };
        for aspect in [9.0 / 16.0, 16.0 / 9.0, 1.0] {
            let size = compute_initial_window_size(aspect, area);
            // 渲染层的真实可用区：整数窗口减掉栏高
            let bar = float_bar_height(size.w);
            let box_w = size.w;
            let box_h = size.h - bar;
            assert!(box_w > 0.0 && box_h > 0.0, "aspect={aspect} 可用区为负");
            // 视频按 contain 放进 box：两轴都应顶满之一，另一边也不该超出 1px
            let video = fit_aspect_in_box(aspect, box_w, box_h);
            let slack_x = box_w - video.w;
            let slack_y = box_h - video.h;
            assert!(
                slack_x.min(slack_y) < 0.02,
                "aspect={aspect} 有一边{{x:{slack_x:.2}, y:{slack_y:.2}}}未顶满 → 会留黑边"
            );
            // 另一侧允许少量余量：窗口只能落在整像素上，`ceil` 多出来的那一像素
            // 必然落到某一轴上。余量是**视频区比视频需求更大**造成的，
            // `contain` 会先把这一轴顶满、另一轴居中，不会黑边，只是画面偏零点几像素。
            // 修复前 gutter 完全没计入，视频区比需求**小** 8px 左右，那才是肉眼可见的黑边。
            assert!(
                slack_x.max(slack_y) <= 2.0,
                "aspect={aspect} 另一侧余量过大{{x:{slack_x:.2}, y:{slack_y:.2}}}"
            );
        }
    }

    /// 旋屏连续两次都要能定形：这是实际踩到的回归 ——
    /// `Resized` 事件没接，第一次定形后 `last_programmatic_size` 没跟上，
    /// 第二次进来时尺寸已偏离，被判成用户拖拽，从此不再自动定形。
    #[test]
    fn repeated_fit_keeps_working_while_only_program_resizes() {
        let windows = FloatWindows::new();
        let label = "live:16285946319";
        let portrait = Size { w: 431.0, h: 798.0 };
        windows.insert(
            label.to_string(),
            sample_payload(),
            portrait,
            "playing".to_string(),
        );

        // 第一次定形：元数据到达，竖屏 9:16
        assert!(windows.decide_auto_fit(label));
        windows.set_programmatic_size(label, portrait);

        // 旋转成横屏：定形并记账
        let landscape = Size { w: 900.0, h: 543.0 };
        assert!(windows.decide_auto_fit(label));
        windows.set_programmatic_size(label, landscape);
        // 窗口真的变成 landscape 了，事件到达时应当认得出这是程序干的
        windows.note_resized(label, landscape);

        // 再转回竖屏：仍要能定形
        assert!(windows.decide_auto_fit(label));
        windows.set_programmatic_size(label, portrait);
        windows.note_resized(label, portrait);

        // 第三次（用户反复点旋转）也不该被锁死
        assert!(windows.decide_auto_fit(label));
    }

    #[test]
    fn note_resized_treats_unexpected_size_as_user_drag() {
        let windows = FloatWindows::new();
        let label = "live:1";
        let size = Size { w: 431.0, h: 798.0 };
        windows.insert(
            label.to_string(),
            sample_payload(),
            size,
            "playing".to_string(),
        );
        // 落定前一律不算：建窗尺寸与请求值不一致是常态
        windows.note_resized(label, Size { w: 700.0, h: 700.0 });
        assert!(windows.decide_auto_fit(label));

        // 落定后，用户拖出别的尺寸 → 之后不再自动定形
        windows.note_resized(label, Size { w: 700.0, h: 700.0 });
        assert!(!windows.decide_auto_fit(label));
    }

    #[test]
    fn topmost_policy_matrix() {
        assert!(resolve_float_topmost("always", false));
        assert!(resolve_float_topmost("playing", true));
        assert!(!resolve_float_topmost("playing", false));
        assert!(!resolve_float_topmost("never", true));
        // 建窗瞬间：只有 never 不置顶
        assert!(initial_float_topmost("always"));
        assert!(initial_float_topmost("playing"));
        assert!(!initial_float_topmost("never"));
    }

    #[test]
    fn cascade_offsets_go_left_and_down_then_wrap() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let size = Size { w: 500.0, h: 800.0 };
        let first = resolve_float_window_position(area, size, 0);
        let second = resolve_float_window_position(area, size, 1);
        assert_eq!(first.0, 1920.0 - 500.0 - 24.0);
        assert_eq!(first.1, 24.0);
        assert_eq!(second.0, first.0 - FLOAT_CASCADE_STEP);
        assert_eq!(second.1, first.1 + FLOAT_CASCADE_STEP);
        // 回绕：档位用尽后回到第 0 档
        let wrapped = resolve_float_window_position(area, size, 19);
        assert!(wrapped.0 >= area.x && wrapped.1 + size.h <= area.y + area.height + 0.5);
    }

    #[test]
    fn labels_round_trip() {
        let label = key_of("live", "1330968594399170560");
        assert_eq!(label, "live:1330968594399170560");
        assert_eq!(split_label(&label), Some(("live", "1330968594399170560")));
        assert_eq!(split_label("main"), None);
        assert_eq!(split_label("live:"), None);
        assert_eq!(split_label("bogus:1"), None);
    }

    #[test]
    fn hash_encodes_live_id() {
        assert_eq!(
            float_hash("playback", "12 34"),
            "/float?kind=playback&liveId=12%2034"
        );
    }
}
