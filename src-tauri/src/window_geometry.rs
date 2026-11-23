//! 主窗口几何（对照 Electron `main/window/window-bounds.ts`）。
//!
//! 基准 1400×900，大屏按工作区等比放大（封顶 1.2 倍），小屏按轴收敛并夹住长宽比，
//! 创建时居中；显示器插拔 / 分辨率 / 缩放变化后把窗口收回新工作区 —— **只在越界时动**，
//! 不覆盖用户手动调过的尺寸（与 Electron `refitMainWindowToWorkArea` 同语义）。
//!
//! 原版不持久化窗口位置：每次启动都按当前工作区重新计算并居中，行为可预期，
//! 也避免「窗口被拖到已拔掉的显示器上再也看不见」这类问题。

/// 期望尺寸；`min_*` 是系统会强行抻到的下限。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowSizeSpec {
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
}

pub const WINDOW_PREFERRED: WindowSizeSpec = WindowSizeSpec {
    width: 1400.0,
    height: 900.0,
    min_width: 720.0,
    min_height: 560.0,
};

/// 大屏放大判定：窗口最多占工作区任一轴的 65%，两轴各自换算倍率后取小者。
const WIDE_SCREEN_RATIO: f64 = 0.65;
/// 相对基准尺寸的最大放大倍率；宽高共用，保持基准长宽比。
const WIDE_SCREEN_MAX_SCALE: f64 = 1.2;
/// 窗口四边与工作区之间的留白。
const WINDOW_EDGE_GAP: f64 = 32.0;
/// 高度被工作区压低时，窗口宽不超过这个长宽比。
const WINDOW_MAX_ASPECT: f64 = 1.7;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 单边收敛：按工作区放大（倍率封顶）→ 让出留白 → 夹到 [下限, 工作区]；
/// 工作区小于下限时放弃留白。
fn fit_edge(area: f64, preferred: f64, min: f64) -> f64 {
    let threshold = preferred / WIDE_SCREEN_RATIO;
    let scale = WIDE_SCREEN_MAX_SCALE.min(area / threshold);
    let wanted = preferred.max((preferred * scale).round());
    let with_gap = area - 2.0 * WINDOW_EDGE_GAP;
    wanted.min(area.min(with_gap.max(min)))
}

/// 大屏等比放大：两轴倍率取小者并封顶，宽高共用。
fn fit_wide_screen(area: Area, preferred: WindowSizeSpec) -> Option<WindowSizeSpec> {
    let scale = (area.width / (preferred.width / WIDE_SCREEN_RATIO))
        .min(area.height / (preferred.height / WIDE_SCREEN_RATIO));
    if scale <= 1.0 {
        return None;
    }
    let capped = WIDE_SCREEN_MAX_SCALE.min(scale);
    let width = (preferred.width * capped).round();
    let height = (preferred.height * capped).round();
    Some(WindowSizeSpec {
        width,
        height,
        min_width: preferred.min_width.min(width),
        min_height: preferred.min_height.min(height),
    })
}

/// 计算主窗口尺寸。下限必须跟着收敛：系统会把小于下限的初始尺寸强行抻大。
pub fn compute_window_bounds(area: Area, preferred: WindowSizeSpec) -> WindowSizeSpec {
    if let Some(wide) = fit_wide_screen(area, preferred) {
        return wide;
    }
    let height = fit_edge(area.height, preferred.height, preferred.min_height);
    let raw_width = fit_edge(area.width, preferred.width, preferred.min_width);
    let aspect_cap = (height * WINDOW_MAX_ASPECT)
        .round()
        .max(preferred.min_width);
    let width = if height < preferred.height {
        raw_width.min(aspect_cap)
    } else {
        raw_width
    };
    WindowSizeSpec {
        width,
        height,
        min_width: preferred.min_width.min(width),
        min_height: preferred.min_height.min(height),
    }
}

/// 显式给坐标并居中：不传坐标时各平台行为不一致（Windows 会从左上角层叠）。
pub fn center_window(area: Area, width: f64, height: f64) -> (f64, f64) {
    (
        area.x + ((area.width - width) / 2.0).round(),
        area.y + ((area.height - height) / 2.0).round(),
    )
}

/// 显示器参数变化后把窗口收回新工作区；只在越界时动，不覆盖用户手动调过的尺寸。
pub fn fit_window_to_work_area(current: Area, area: Area) -> Area {
    let width = current.width.min(area.width);
    let height = current.height.min(area.height);
    Area {
        x: current.x.max(area.x).min(area.x + area.width - width),
        y: current.y.max(area.y).min(area.y + area.height - height),
        width,
        height,
    }
}

/// 主显示器工作区（逻辑像素）。
pub fn primary_area(app: &tauri::AppHandle) -> Area {
    let monitor = app.primary_monitor().ok().flatten();
    area_of(monitor.as_ref())
}

/// 窗口所在显示器的工作区（逻辑像素）；拿不到时退回主显示器。
pub fn area_for_window(window: &tauri::WebviewWindow) -> Area {
    use tauri::Manager;
    match window.current_monitor() {
        Ok(Some(monitor)) => area_of(Some(&monitor)),
        _ => primary_area(window.app_handle()),
    }
}

fn area_of(monitor: Option<&tauri::Monitor>) -> Area {
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
        // 拿不到显示器信息时按 1440×900 这类常见工作区兜底，保证窗口至少可见
        None => Area {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        },
    }
}

/// 按工作区创建主窗口前的尺寸与位置（供 `tauri.conf.json` 之外的动态调整用）。
pub fn initial_main_geometry(app: &tauri::AppHandle) -> (WindowSizeSpec, (f64, f64)) {
    let area = primary_area(app);
    let size = compute_window_bounds(area, WINDOW_PREFERRED);
    let (x, y) = center_window(area, size.width, size.height);
    (size, (x, y))
}

/// 显示器变化后收回可见区：只动越界的那一边。
pub fn refit_main_window(window: &tauri::WebviewWindow) {
    if window.is_minimized().unwrap_or(false)
        || window.is_maximized().unwrap_or(false)
        || window.is_fullscreen().unwrap_or(false)
    {
        return;
    }
    let (Ok(position), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let current = Area {
        x: position.x as f64 / scale,
        y: position.y as f64 / scale,
        width: size.width as f64 / scale,
        height: size.height as f64 / scale,
    };
    let area = area_for_window(window);
    let next = fit_window_to_work_area(current, area);
    if next != current {
        let _ = window.set_position(tauri::LogicalPosition::new(next.x, next.y));
        let _ = window.set_size(tauri::LogicalSize::new(next.width, next.height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: f64, height: f64) -> Area {
        Area {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    #[test]
    fn large_screen_scales_up_with_cap() {
        // 1920×1080：1400/0.65 = 2153 > 1920 → 倍率 < 1，走收敛路径而非放大
        let small = compute_window_bounds(area(1920.0, 1080.0), WINDOW_PREFERRED);
        assert!(small.width <= 1400.0);
        // 3840×2160：两轴倍率都 > 1.2，按 1.2 倍封顶
        let large = compute_window_bounds(area(3840.0, 2160.0), WINDOW_PREFERRED);
        assert_eq!(large.width, 1400.0 * 1.2);
        assert_eq!(large.height, 900.0 * 1.2);
        // 2999×1689：恰好跨过 1400/0.65 的阈值，但高度倍率不足 1 → 仍走收敛
        let wide = compute_window_bounds(area(2400.0, 1400.0), WINDOW_PREFERRED);
        assert!(wide.width >= 1400.0 && wide.height >= 900.0);
    }

    #[test]
    fn small_screen_respects_gap_and_min() {
        let bounds = compute_window_bounds(area(1280.0, 720.0), WINDOW_PREFERRED);
        assert!(bounds.width <= 1280.0 - 2.0 * WINDOW_EDGE_GAP);
        assert!(bounds.height <= 720.0);
        assert!(bounds.min_width <= bounds.width);
        assert!(bounds.min_height <= bounds.height);
    }

    #[test]
    fn tiny_screen_gives_up_gap_to_keep_minimum() {
        // 800×600：去掉留白后不够 720 宽，应当放弃留白保住下限
        let bounds = compute_window_bounds(area(800.0, 600.0), WINDOW_PREFERRED);
        assert!(bounds.width >= WINDOW_PREFERRED.min_width.min(800.0));
        assert!(bounds.width <= 800.0);
    }

    #[test]
    fn short_screen_caps_aspect() {
        let bounds = compute_window_bounds(area(2560.0, 700.0), WINDOW_PREFERRED);
        // 高度被压低时宽度不超过 高度的 1.7 倍
        assert!(bounds.width <= (bounds.height * WINDOW_MAX_ASPECT).round() + 1.0);
    }

    #[test]
    fn center_is_exact() {
        let (x, y) = center_window(area(1920.0, 1080.0), 1400.0, 900.0);
        assert_eq!(x, 260.0);
        assert_eq!(y, 90.0);
    }

    #[test]
    fn refit_only_moves_when_out_of_bounds() {
        let work = Area {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        // 完全在区内：原样返回
        let inside = Area {
            x: 100.0,
            y: 100.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(fit_window_to_work_area(inside, work), inside);
        // 越界：收回到可见区
        let outside = Area {
            x: 1800.0,
            y: 900.0,
            width: 800.0,
            height: 600.0,
        };
        let fixed = fit_window_to_work_area(outside, work);
        assert_eq!(fixed.x, 1920.0 - 800.0);
        assert_eq!(fixed.y, 1080.0 - 600.0);
    }

    #[test]
    fn refit_shrinks_window_larger_than_area() {
        let work = area(1280.0, 720.0);
        let huge = Area {
            x: 0.0,
            y: 0.0,
            width: 1600.0,
            height: 1000.0,
        };
        let fixed = fit_window_to_work_area(huge, work);
        assert_eq!(fixed.width, 1280.0);
        assert_eq!(fixed.height, 720.0);
    }
}
