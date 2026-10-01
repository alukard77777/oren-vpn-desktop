use crate::preferences::WindowBounds;

pub const MIN_WIDTH: u32 = 700;
pub const MIN_HEIGHT: u32 = 620;
pub const DEFAULT_WIDTH: u32 = 840;
pub const DEFAULT_HEIGHT: u32 = 720;

const REACHABLE_TITLE_BAR: i64 = 64;

#[cfg(target_os = "windows")]
pub fn apply_tray_popup_native_rounding(window: &slint::winit_030::winit::window::Window) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
    };

    let Ok(window_handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = window_handle.as_raw() else {
        return;
    };
    let preference = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            win32.hwnd.get() as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&preference as *const i32).cast(),
            std::mem::size_of_val(&preference) as u32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub fn apply_tray_popup_native_rounding(_window: &slint::winit_030::winit::window::Window) {}

/// Moves and resizes the tray popup in one step, so the corner at the tray does
/// not jitter while an opposite edge is dragged.
pub fn set_tray_popup_rect(window: &slint::winit_030::winit::window::Window, rect: PhysicalRect) {
    #[cfg(target_os = "windows")]
    {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos,
        };
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            let moved = unsafe {
                SetWindowPos(
                    win32.hwnd.get() as _,
                    std::ptr::null_mut(),
                    rect.x,
                    rect.y,
                    rect.width as i32,
                    rect.height as i32,
                    SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
                )
            };
            if moved != 0 {
                return;
            }
        }
    }
    use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
    window.set_outer_position(PhysicalPosition::new(rect.x, rect.y));
    let _ = window.request_inner_size(PhysicalSize::new(rect.width, rect.height));
}

/// The pointer in physical desktop coordinates, where the platform reports it.
#[cfg(target_os = "windows")]
pub fn global_cursor_position() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

#[cfg(not(target_os = "windows"))]
pub fn global_cursor_position() -> Option<(i32, i32)> {
    None
}

#[cfg(test)]
pub const RESIZE_BORDER: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDisposition {
    HideToTray,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinimizeDisposition {
    HideToTray,
    MinimizeToTaskbar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorRect {
    /// Physical desktop coordinate in Winit's shared monitor coordinate space.
    pub x: i32,
    /// Physical desktop coordinate in Winit's shared monitor coordinate space.
    pub y: i32,
    /// Physical work-area width.
    pub width: u32,
    /// Physical work-area height.
    pub height: u32,
    /// Monitor scale factor multiplied by 1000. Valid values are 500..=8000.
    pub scale_milli: u32,
    pub primary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorSignature {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
}

pub fn monitor_cap_changed(
    previous: Option<MonitorSignature>,
    current: Option<MonitorSignature>,
) -> bool {
    previous != current
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayTaskbarEdge {
    Top,
    Right,
    Bottom,
    Left,
}

pub fn place_tray_popup(
    anchor: PhysicalPosition,
    popup_size: (u32, u32),
    monitors: &[MonitorRect],
    edge_hint: Option<TrayTaskbarEdge>,
) -> Option<PhysicalPosition> {
    let monitor = select_tray_monitor(anchor, monitors)?;
    let edge = edge_hint.unwrap_or_else(|| infer_taskbar_edge(anchor, monitor));
    Some(place_tray_popup_on_monitor(
        anchor, popup_size, monitor, edge,
    ))
}

fn select_tray_monitor(anchor: PhysicalPosition, monitors: &[MonitorRect]) -> Option<&MonitorRect> {
    monitors
        .iter()
        .filter(|monitor| valid_monitor(monitor))
        .min_by_key(|monitor| {
            let right = i64::from(monitor.x) + i64::from(monitor.width);
            let bottom = i64::from(monitor.y) + i64::from(monitor.height);
            let x = i64::from(anchor.x);
            let y = i64::from(anchor.y);
            let dx = if x < i64::from(monitor.x) {
                i64::from(monitor.x) - x
            } else if x > right {
                x - right
            } else {
                0
            };
            let dy = if y < i64::from(monitor.y) {
                i64::from(monitor.y) - y
            } else if y > bottom {
                y - bottom
            } else {
                0
            };
            dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
        })
}

fn infer_taskbar_edge(anchor: PhysicalPosition, monitor: &MonitorRect) -> TrayTaskbarEdge {
    let x = i64::from(anchor.x);
    let y = i64::from(anchor.y);
    let left = i64::from(monitor.x);
    let top = i64::from(monitor.y);
    let right = left + i64::from(monitor.width);
    let bottom = top + i64::from(monitor.height);

    [
        (top.saturating_sub(y), TrayTaskbarEdge::Top),
        (x.saturating_sub(right), TrayTaskbarEdge::Right),
        (y.saturating_sub(bottom), TrayTaskbarEdge::Bottom),
        (left.saturating_sub(x), TrayTaskbarEdge::Left),
    ]
    .into_iter()
    .filter(|(outside, _)| *outside > 0)
    .max_by_key(|(outside, _)| *outside)
    .map(|(_, edge)| edge)
    .unwrap_or_else(|| inside_anchor_edge(y, top, bottom))
}

#[cfg(windows)]
fn inside_anchor_edge(_y: i64, _top: i64, _bottom: i64) -> TrayTaskbarEdge {
    TrayTaskbarEdge::Bottom
}

#[cfg(not(windows))]
fn inside_anchor_edge(y: i64, top: i64, bottom: i64) -> TrayTaskbarEdge {
    if y - top < bottom - y {
        TrayTaskbarEdge::Top
    } else {
        TrayTaskbarEdge::Bottom
    }
}

fn place_tray_popup_on_monitor(
    anchor: PhysicalPosition,
    popup_size: (u32, u32),
    monitor: &MonitorRect,
    edge: TrayTaskbarEdge,
) -> PhysicalPosition {
    const ANCHOR_GAP: i64 = 10;

    let width = i64::from(popup_size.0.min(monitor.width));
    let height = i64::from(popup_size.1.min(monitor.height));
    let min_x = i64::from(monitor.x);
    let min_y = i64::from(monitor.y);
    let max_x = min_x + i64::from(monitor.width) - width;
    let max_y = min_y + i64::from(monitor.height) - height;
    let work_right = min_x + i64::from(monitor.width);
    let work_bottom = min_y + i64::from(monitor.height);
    let anchor_x = i64::from(anchor.x);
    let anchor_y = i64::from(anchor.y);
    let (desired_x, desired_y) = match edge {
        TrayTaskbarEdge::Top => (anchor_x - width, anchor_y + ANCHOR_GAP),
        TrayTaskbarEdge::Right => (
            anchor_x - width - ANCHOR_GAP,
            anchor_y - height - ANCHOR_GAP,
        ),
        TrayTaskbarEdge::Bottom => (anchor_x - width, anchor_y - height - ANCHOR_GAP),
        TrayTaskbarEdge::Left => (anchor_x + ANCHOR_GAP, anchor_y - height - ANCHOR_GAP),
    };

    let desired_x = match edge {
        TrayTaskbarEdge::Left if anchor_x < min_x => min_x,
        TrayTaskbarEdge::Right if anchor_x > work_right => max_x,
        _ => desired_x,
    };
    let desired_y = match edge {
        TrayTaskbarEdge::Top if anchor_y < min_y => min_y,
        TrayTaskbarEdge::Bottom if anchor_y > work_bottom => max_y,
        _ => desired_y,
    };

    PhysicalPosition {
        x: desired_x.clamp(min_x, max_x) as i32,
        y: desired_y.clamp(min_y, max_y) as i32,
    }
}

/// Edges of the tray popup that resize it: the ones away from the tray corner it
/// is anchored to (a bottom taskbar: top and left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayPopupGrip {
    pub up: bool,
    pub left: bool,
}

/// The monitor whose work area the tray popup opens in.
pub fn tray_popup_monitor(
    anchor: PhysicalPosition,
    monitors: &[MonitorRect],
) -> Option<&MonitorRect> {
    select_tray_monitor(anchor, monitors)
}

pub fn tray_popup_grip(anchor: PhysicalPosition, monitors: &[MonitorRect]) -> TrayPopupGrip {
    let edge = select_tray_monitor(anchor, monitors).map_or(TrayTaskbarEdge::Bottom, |monitor| {
        infer_taskbar_edge(anchor, monitor)
    });
    match edge {
        TrayTaskbarEdge::Bottom | TrayTaskbarEdge::Right => TrayPopupGrip {
            up: true,
            left: true,
        },
        TrayTaskbarEdge::Top => TrayPopupGrip {
            up: false,
            left: true,
        },
        TrayTaskbarEdge::Left => TrayPopupGrip {
            up: true,
            left: false,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Tray popup bounds in logical pixels: the approved default is the minimum.
pub const TRAY_POPUP_MIN: (u32, u32) = (304, 460);
pub const TRAY_POPUP_MAX: (u32, u32) = (520, 900);

/// The popup's physical size for a logical one: within its limits and the work
/// area (minus the gap kept to the taskbar).
pub fn tray_popup_size(
    logical: (u32, u32),
    scale_milli: u32,
    work: Option<&MonitorRect>,
) -> (u32, u32) {
    const GAP: u32 = 20;
    let scale = |value: u32| (u64::from(value) * u64::from(scale_milli.max(500)) / 1000) as u32;
    let fit = |value: u32, min: u32, max: u32, room: Option<u32>| {
        let max = room.map_or(scale(max), |room| scale(max).min(room.saturating_sub(GAP)));
        scale(value.max(min)).min(max.max(scale(min)))
    };
    (
        fit(
            logical.0,
            TRAY_POPUP_MIN.0,
            TRAY_POPUP_MAX.0,
            work.map(|work| work.width),
        ),
        fit(
            logical.1,
            TRAY_POPUP_MIN.1,
            TRAY_POPUP_MAX.1,
            work.map(|work| work.height),
        ),
    )
}

/// Which edges a tray-popup drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayResizeHandle {
    pub horizontal: bool,
    pub vertical: bool,
}

/// New popup bounds while an edge is dragged by `delta` (physical) from `start`:
/// the corner at the tray stays put, the size stays within `min`/`max` and the
/// grown edge never leaves the work area of the monitor the popup is on.
pub fn resize_tray_popup(
    start: PhysicalRect,
    delta: (i32, i32),
    handle: TrayResizeHandle,
    grip: TrayPopupGrip,
    limits: ((u32, u32), (u32, u32)),
    monitors: &[MonitorRect],
) -> PhysicalRect {
    let (min, max) = limits;
    let center = PhysicalPosition {
        x: start.x.saturating_add((start.width / 2) as i32),
        y: start.y.saturating_add((start.height / 2) as i32),
    };
    let work = select_tray_monitor(center, monitors);
    let right = i64::from(start.x) + i64::from(start.width);
    let bottom = i64::from(start.y) + i64::from(start.height);
    let span = |size: u32, delta: i32, grows_back: bool, room: Option<i64>, min: u32, max: u32| {
        let wanted = i64::from(size)
            + if grows_back {
                -i64::from(delta)
            } else {
                i64::from(delta)
            };
        let max = room.map_or(i64::from(max), |room| i64::from(max).min(room));
        wanted.min(max).max(i64::from(min)) as u32
    };
    let width = if handle.horizontal {
        let room = work.map(|work| {
            if grip.left {
                right - i64::from(work.x)
            } else {
                i64::from(work.x) + i64::from(work.width) - i64::from(start.x)
            }
        });
        span(start.width, delta.0, grip.left, room, min.0, max.0)
    } else {
        start.width
    };
    let height = if handle.vertical {
        let room = work.map(|work| {
            if grip.up {
                bottom - i64::from(work.y)
            } else {
                i64::from(work.y) + i64::from(work.height) - i64::from(start.y)
            }
        });
        span(start.height, delta.1, grip.up, room, min.1, max.1)
    } else {
        start.height
    };
    PhysicalRect {
        x: if grip.left {
            (right - i64::from(width)) as i32
        } else {
            start.x
        },
        y: if grip.up {
            (bottom - i64::from(height)) as i32
        } else {
            start.y
        },
        width,
        height,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoredPlacement {
    pub position: PhysicalPosition,
    pub inner_size: LogicalSize,
    pub max_inner_size: LogicalSize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HitRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
impl HitRect {
    pub fn intersects(self, other: Self) -> bool {
        self.x < other.x.saturating_add(other.width)
            && other.x < self.x.saturating_add(self.width)
            && self.y < other.y.saturating_add(other.height)
            && other.y < self.y.saturating_add(self.height)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeHitLayout {
    pub visual_frame: HitRect,
    window_width: u32,
    window_height: u32,
}

#[cfg(test)]
impl ResizeHitLayout {
    pub fn new(window_width: u32, window_height: u32) -> Option<Self> {
        let inset = RESIZE_BORDER.checked_mul(2)?;
        let frame_width = window_width.checked_sub(inset)?;
        let frame_height = window_height.checked_sub(inset)?;
        (frame_width >= 132 && frame_height >= 44).then_some(Self {
            visual_frame: HitRect {
                x: RESIZE_BORDER,
                y: RESIZE_BORDER,
                width: frame_width,
                height: frame_height,
            },
            window_width,
            window_height,
        })
    }

    pub fn resize_hits(self) -> [HitRect; 8] {
        let border = RESIZE_BORDER;
        let middle_width = self.window_width - border * 2;
        let middle_height = self.window_height - border * 2;
        [
            HitRect {
                x: border,
                y: 0,
                width: middle_width,
                height: border,
            },
            HitRect {
                x: self.window_width - border,
                y: border,
                width: border,
                height: middle_height,
            },
            HitRect {
                x: border,
                y: self.window_height - border,
                width: middle_width,
                height: border,
            },
            HitRect {
                x: 0,
                y: border,
                width: border,
                height: middle_height,
            },
            HitRect {
                x: 0,
                y: 0,
                width: border,
                height: border,
            },
            HitRect {
                x: self.window_width - border,
                y: 0,
                width: border,
                height: border,
            },
            HitRect {
                x: self.window_width - border,
                y: self.window_height - border,
                width: border,
                height: border,
            },
            HitRect {
                x: 0,
                y: self.window_height - border,
                width: border,
                height: border,
            },
        ]
    }

    pub fn title_buttons(self) -> [HitRect; 3] {
        let first_x = self.visual_frame.x + self.visual_frame.width - 132;
        [0, 44, 88].map(|offset| HitRect {
            x: first_x + offset,
            y: self.visual_frame.y,
            width: 44,
            height: 44,
        })
    }
}

pub fn minimize_disposition(tray_available: bool) -> MinimizeDisposition {
    if tray_available {
        MinimizeDisposition::HideToTray
    } else {
        MinimizeDisposition::MinimizeToTaskbar
    }
}

pub fn close_disposition(tray_available: bool, smoke_mode: bool) -> CloseDisposition {
    if tray_available && !smoke_mode {
        CloseDisposition::HideToTray
    } else {
        CloseDisposition::Exit
    }
}

pub fn restore_placement(
    saved: WindowBounds,
    monitors: &[MonitorRect],
) -> Option<RestoredPlacement> {
    let saved_is_valid = saved.width > 0 && saved.height > 0;
    let selection = saved_is_valid
        .then(|| select_intersecting_monitor(&saved, monitors))
        .flatten();
    let (monitor, intersects) = match selection {
        Some(monitor) => (monitor, true),
        None => (select_fallback_monitor(monitors)?, false),
    };

    let requested_width = if saved_is_valid {
        saved.width
    } else {
        DEFAULT_WIDTH
    };
    let requested_height = if saved_is_valid {
        saved.height
    } else {
        DEFAULT_HEIGHT
    };
    let max_width = physical_to_logical(monitor.width, monitor.scale_milli);
    let max_height = physical_to_logical(monitor.height, monitor.scale_milli);
    let width = bounded_dimension(requested_width, max_width, MIN_WIDTH);
    let height = bounded_dimension(requested_height, max_height, MIN_HEIGHT);
    let physical_width = logical_to_physical(width, monitor.scale_milli);
    let physical_height = logical_to_physical(height, monitor.scale_milli);
    let max_inner_size = LogicalSize {
        width: max_width,
        height: max_height,
    };

    if !intersects {
        return Some(RestoredPlacement {
            position: PhysicalPosition {
                x: saturating_i32(
                    i64::from(monitor.x)
                        + (i64::from(monitor.width) - i64::from(physical_width)) / 2,
                ),
                y: saturating_i32(
                    i64::from(monitor.y)
                        + (i64::from(monitor.height) - i64::from(physical_height)) / 2,
                ),
            },
            inner_size: LogicalSize { width, height },
            max_inner_size,
        });
    }

    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left + i64::from(monitor.width);
    let monitor_bottom = monitor_top + i64::from(monitor.height);
    let reachable = i64::from(logical_to_physical(
        REACHABLE_TITLE_BAR as u32,
        monitor.scale_milli,
    ));
    let x_min = monitor_left - i64::from(physical_width) + reachable.min(i64::from(physical_width));
    let x_max = monitor_right - reachable.min(i64::from(physical_width));
    let y_max = monitor_bottom - reachable.min(i64::from(monitor.height));

    Some(RestoredPlacement {
        position: PhysicalPosition {
            x: saturating_i32(clamp_i64(i64::from(saved.x), x_min, x_max)),
            y: saturating_i32(clamp_i64(i64::from(saved.y), monitor_top, y_max)),
        },
        inner_size: LogicalSize { width, height },
        max_inner_size,
    })
}

#[cfg(test)]
pub fn restore_bounds(saved: WindowBounds, monitors: &[MonitorRect]) -> Option<WindowBounds> {
    restore_placement(saved, monitors).map(|placement| WindowBounds {
        x: placement.position.x,
        y: placement.position.y,
        width: placement.inner_size.width,
        height: placement.inner_size.height,
    })
}

fn select_intersecting_monitor<'a>(
    saved: &WindowBounds,
    monitors: &'a [MonitorRect],
) -> Option<&'a MonitorRect> {
    monitors
        .iter()
        .enumerate()
        .filter(|(_, monitor)| valid_monitor(monitor))
        .map(|(index, monitor)| (intersection_area(saved, monitor), index, monitor))
        .filter(|(area, _, _)| *area > 0)
        .max_by_key(|(area, index, _)| (*area, std::cmp::Reverse(*index)))
        .map(|(_, _, monitor)| monitor)
}

fn select_fallback_monitor(monitors: &[MonitorRect]) -> Option<&MonitorRect> {
    monitors
        .iter()
        .filter(|monitor| valid_monitor(monitor))
        .find(|monitor| monitor.primary)
        .or_else(|| monitors.iter().find(|monitor| valid_monitor(monitor)))
}

fn intersection_area(saved: &WindowBounds, monitor: &MonitorRect) -> u64 {
    let saved_left = i64::from(saved.x);
    let saved_top = i64::from(saved.y);
    let saved_right = saved_left + i64::from(logical_to_physical(saved.width, monitor.scale_milli));
    let saved_bottom =
        saved_top + i64::from(logical_to_physical(saved.height, monitor.scale_milli));
    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left + i64::from(monitor.width);
    let monitor_bottom = monitor_top + i64::from(monitor.height);

    let width = (saved_right.min(monitor_right) - saved_left.max(monitor_left)).max(0) as u64;
    let height = (saved_bottom.min(monitor_bottom) - saved_top.max(monitor_top)).max(0) as u64;
    width.saturating_mul(height)
}

fn valid_monitor(monitor: &MonitorRect) -> bool {
    monitor.width > 0 && monitor.height > 0 && (500..=8_000).contains(&monitor.scale_milli)
}

fn logical_to_physical(logical: u32, scale_milli: u32) -> u32 {
    let scaled = u64::from(logical)
        .saturating_mul(u64::from(scale_milli))
        .saturating_add(500)
        / 1_000;
    scaled.min(u64::from(u32::MAX)) as u32
}

fn physical_to_logical(physical: u32, scale_milli: u32) -> u32 {
    let scaled = u64::from(physical).saturating_mul(1_000) / u64::from(scale_milli);
    scaled.min(u64::from(u32::MAX)) as u32
}

fn bounded_dimension(saved: u32, available: u32, minimum: u32) -> u32 {
    if available >= minimum {
        saved.clamp(minimum, available)
    } else {
        available
    }
}

fn clamp_i64(value: i64, minimum: i64, maximum: i64) -> i64 {
    if minimum <= maximum {
        value.clamp(minimum, maximum)
    } else {
        minimum
    }
}

fn saturating_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub fn parse_resize_edge(edge: &str) -> Option<ResizeEdge> {
    match edge {
        "n" => Some(ResizeEdge::North),
        "ne" => Some(ResizeEdge::NorthEast),
        "e" => Some(ResizeEdge::East),
        "se" => Some(ResizeEdge::SouthEast),
        "s" => Some(ResizeEdge::South),
        "sw" => Some(ResizeEdge::SouthWest),
        "w" => Some(ResizeEdge::West),
        "nw" => Some(ResizeEdge::NorthWest),
        _ => None,
    }
}

pub fn next_maximized(current: bool) -> bool {
    !current
}

/// Logical width the expanded navigation rail adds (176px rail - 64px rail is
/// what the content loses; the window grows by the whole expanded rail so the
/// content keeps its collapsed-layout width).
pub const EXPANDED_RAIL_WIDTH: u32 = 176;
/// Collapsed (icon-only) rail width; the window grows by the difference.
pub const COLLAPSED_RAIL_WIDTH: u32 = 64;

/// How much a window grew when the rail expanded, so collapsing can undo it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RailGrowth {
    /// Physical pixels added to the inner width.
    pub delta: u32,
    /// Physical inner width right after growing; a different width at collapse
    /// time means the user resized in between.
    pub grown_width: u32,
}

/// Physical width to add when the rail expands: the full `wanted` amount, or
/// what still fits between the window's right edge and the work area's right
/// edge (the left edge stays put). Maximized windows never grow.
pub fn rail_grow_delta(
    maximized: bool,
    outer_x: i32,
    outer_width: u32,
    work_x: i32,
    work_width: u32,
    wanted: u32,
) -> u32 {
    if maximized {
        return 0;
    }
    let right_edge = i64::from(outer_x) + i64::from(outer_width);
    let work_right = i64::from(work_x) + i64::from(work_width);
    let room = (work_right - right_edge).max(0);
    u32::try_from(room.min(i64::from(wanted))).unwrap_or(0)
}

/// Inner width to restore when the rail collapses: only when the window still
/// has exactly the width it had right after growing (and is not maximized).
pub fn rail_shrink_width(
    growth: Option<RailGrowth>,
    maximized: bool,
    current_inner_width: u32,
) -> Option<u32> {
    let growth = growth?;
    // A couple of pixels of slack absorb rounding when the growth was restored
    // from logical pixels on another scale factor.
    if maximized || growth.delta == 0 || current_inner_width.abs_diff(growth.grown_width) > 2 {
        return None;
    }
    Some(current_inner_width.saturating_sub(growth.delta))
}

/// Duration of the window-width animation that follows the rail; matches the
/// rail's own 240ms width animation in app.slint.
pub const RAIL_RESIZE_MS: u32 = 240;

/// Inner width `elapsed_ms` into a rail resize from `from` to `to`, eased with
/// ease-out-quint (the rail uses cubic-bezier(0.22, 1, 0.36, 1), which it
/// closely tracks), so window and rail grow together and the content stays put.
pub fn rail_resize_frame(from: u32, to: u32, elapsed_ms: u32, duration_ms: u32) -> u32 {
    if duration_ms == 0 || elapsed_ms >= duration_ms {
        return to;
    }
    let t = f64::from(elapsed_ms) / f64::from(duration_ms);
    let eased = 1.0 - (1.0 - t).powi(5);
    let width = f64::from(from) + (f64::from(to) - f64::from(from)) * eased;
    width.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

impl From<ResizeEdge> for slint::winit_030::winit::window::ResizeDirection {
    fn from(edge: ResizeEdge) -> Self {
        use slint::winit_030::winit::window::ResizeDirection;

        match edge {
            ResizeEdge::North => ResizeDirection::North,
            ResizeEdge::NorthEast => ResizeDirection::NorthEast,
            ResizeEdge::East => ResizeDirection::East,
            ResizeEdge::SouthEast => ResizeDirection::SouthEast,
            ResizeEdge::South => ResizeDirection::South,
            ResizeEdge::SouthWest => ResizeDirection::SouthWest,
            ResizeEdge::West => ResizeDirection::West,
            ResizeEdge::NorthWest => ResizeDirection::NorthWest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseDisposition, DEFAULT_HEIGHT, DEFAULT_WIDTH, MinimizeDisposition, MonitorRect,
        MonitorSignature, PhysicalPosition, RailGrowth, ResizeEdge, ResizeHitLayout,
        TrayTaskbarEdge, close_disposition, minimize_disposition, monitor_cap_changed,
        next_maximized, parse_resize_edge, place_tray_popup, rail_grow_delta, rail_shrink_width,
        restore_bounds, restore_placement,
    };
    use super::{
        PhysicalRect, TrayPopupGrip, TrayResizeHandle, resize_tray_popup, tray_popup_grip,
        tray_popup_size,
    };
    use crate::preferences::WindowBounds;

    const LIMITS: ((u32, u32), (u32, u32)) = ((304, 460), (520, 900));
    const BOTH: TrayResizeHandle = TrayResizeHandle {
        horizontal: true,
        vertical: true,
    };
    const TOP: TrayResizeHandle = TrayResizeHandle {
        horizontal: false,
        vertical: true,
    };

    fn rect(x: i32, y: i32, width: u32, height: u32) -> PhysicalRect {
        PhysicalRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn tray_popup_grows_from_the_edges_away_from_the_tray_corner() {
        let monitors = [monitor(0, 0, 1_920, 1_040, true)];
        let up_left = TrayPopupGrip {
            up: true,
            left: true,
        };
        assert_eq!(
            tray_popup_grip(PhysicalPosition { x: 1_910, y: 1_070 }, &monitors),
            up_left
        );
        assert_eq!(
            tray_popup_grip(PhysicalPosition { x: -10, y: 900 }, &monitors),
            TrayPopupGrip {
                up: true,
                left: false
            }
        );
        let start = rect(1_606, 570, 304, 460);
        // Dragging the top-left corner up-left grows it; the bottom-right stays.
        let grown = resize_tray_popup(start, (-96, -180), BOTH, up_left, LIMITS, &monitors);
        assert_eq!(grown, rect(1_510, 390, 400, 640));
        // Only the top edge: the width is untouched.
        let taller = resize_tray_popup(start, (-50, -40), TOP, up_left, LIMITS, &monitors);
        assert_eq!(taller, rect(1_606, 530, 304, 500));
        // Never below the minimum or above the maximum.
        let small = resize_tray_popup(start, (200, 300), BOTH, up_left, LIMITS, &monitors);
        assert_eq!(small, start);
        let huge = resize_tray_popup(start, (-900, -900), BOTH, up_left, LIMITS, &monitors);
        assert_eq!(huge, rect(1_390, 130, 520, 900));
        // A short work area caps the height at its top edge.
        let short = [monitor(0, 0, 1_920, 700, true)];
        let low = rect(1_606, 230, 304, 460);
        let capped = resize_tray_popup(low, (0, -500), TOP, up_left, LIMITS, &short);
        assert_eq!(capped, rect(1_606, 0, 304, 690));
        // A top panel (Linux): the popup hangs down and grows from its bottom edge.
        let down_left = TrayPopupGrip {
            up: false,
            left: true,
        };
        let lower = resize_tray_popup(
            rect(1_596, 34, 304, 460),
            (0, 100),
            TOP,
            down_left,
            LIMITS,
            &monitors,
        );
        assert_eq!(lower, rect(1_596, 34, 304, 560));
    }

    #[test]
    fn tray_popup_size_respects_limits_scale_and_work_area() {
        let work = monitor(0, 0, 1_920, 1_040, true);
        assert_eq!(tray_popup_size((400, 640), 1_000, Some(&work)), (400, 640));
        assert_eq!(tray_popup_size((100, 100), 1_000, Some(&work)), (304, 460));
        assert_eq!(
            tray_popup_size((900, 2_000), 1_000, Some(&work)),
            (520, 900)
        );
        assert_eq!(tray_popup_size((400, 640), 1_500, Some(&work)), (600, 960));
        let short = monitor(0, 0, 1_366, 728, true);
        assert_eq!(tray_popup_size((400, 900), 1_000, Some(&short)), (400, 708));
        assert_eq!(tray_popup_size((400, 640), 1_000, None), (400, 640));
    }

    #[test]
    fn rail_grows_by_the_full_width_when_there_is_room() {
        // 840px window at x=100 on a 1920px work area: 980px to spare.
        assert_eq!(rail_grow_delta(false, 100, 840, 0, 1920, 176), 176);
    }

    #[test]
    fn rail_grow_is_clamped_to_the_work_area_and_never_negative() {
        // Right edge at 1850 on a 1920px work area: only 70px fit.
        assert_eq!(rail_grow_delta(false, 1010, 840, 0, 1920, 176), 70);
        // Already past the right edge (window hangs off-screen): no growth.
        assert_eq!(rail_grow_delta(false, 1200, 840, 0, 1920, 176), 0);
        // Secondary monitor to the left with negative coordinates.
        assert_eq!(rail_grow_delta(false, -1900, 840, -1920, 1920, 176), 176);
    }

    #[test]
    fn maximized_windows_neither_grow_nor_shrink() {
        assert_eq!(rail_grow_delta(true, 0, 1920, 0, 1920, 176), 0);
        let growth = RailGrowth {
            delta: 176,
            grown_width: 1016,
        };
        assert_eq!(rail_shrink_width(Some(growth), true, 1016), None);
    }

    #[test]
    fn collapse_undoes_exactly_the_growth() {
        let growth = RailGrowth {
            delta: 176,
            grown_width: 1016,
        };
        assert_eq!(rail_shrink_width(Some(growth), false, 1016), Some(840));
        let partial = RailGrowth {
            delta: 70,
            grown_width: 910,
        };
        assert_eq!(rail_shrink_width(Some(partial), false, 910), Some(840));
    }

    #[test]
    fn collapse_keeps_a_size_the_user_chose_meanwhile() {
        let growth = RailGrowth {
            delta: 176,
            grown_width: 1016,
        };
        assert_eq!(rail_shrink_width(Some(growth), false, 1200), None);
        assert_eq!(rail_shrink_width(None, false, 1016), None);
        // The window width eases out and always lands exactly on the target.
        assert_eq!(super::rail_resize_frame(840, 1016, 0, 240), 840);
        assert_eq!(super::rail_resize_frame(840, 1016, 240, 240), 1016);
        assert_eq!(super::rail_resize_frame(840, 1016, 999, 240), 1016);
        let early = super::rail_resize_frame(840, 1016, 48, 240);
        assert!(
            early > 840 + 88,
            "ease-out moves most of the way early: {early}"
        );
        assert_eq!(super::rail_resize_frame(1016, 840, 240, 240), 840);
        assert!(super::rail_resize_frame(1016, 840, 120, 240) < 850);
        assert_eq!(super::rail_resize_frame(840, 1016, 10, 0), 1016);
        // Growth restored from preferences tolerates scale rounding.
        assert_eq!(rail_shrink_width(Some(growth), false, 1017), Some(841));
        let nothing = RailGrowth {
            delta: 0,
            grown_width: 840,
        };
        assert_eq!(rail_shrink_width(Some(nothing), false, 840), None);
    }

    fn monitor(x: i32, y: i32, width: u32, height: u32, primary: bool) -> MonitorRect {
        MonitorRect {
            x,
            y,
            width,
            height,
            scale_milli: 1_000,
            primary,
        }
    }

    #[test]
    fn tray_popup_stays_inside_bottom_right_work_area() {
        assert_eq!(
            place_tray_popup(
                PhysicalPosition { x: 1_910, y: 1_070 },
                (304, 460),
                &[monitor(0, 0, 1_920, 1_040, true)],
                None,
            ),
            Some(PhysicalPosition { x: 1_606, y: 580 })
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn tray_popup_anchor_inside_the_monitor_opens_away_from_the_nearer_vertical_edge() {
        let monitors = [monitor(0, 0, 1_920, 1_080, true)];
        assert_eq!(
            place_tray_popup(
                PhysicalPosition { x: 1_900, y: 24 },
                (304, 460),
                &monitors,
                None,
            ),
            Some(PhysicalPosition { x: 1_596, y: 34 })
        );
        assert_eq!(
            place_tray_popup(
                PhysicalPosition { x: 1_900, y: 1_050 },
                (304, 460),
                &monitors,
                None,
            ),
            Some(PhysicalPosition { x: 1_596, y: 580 })
        );
    }

    #[test]
    fn tray_popup_supports_negative_monitor_coordinates() {
        assert_eq!(
            place_tray_popup(
                PhysicalPosition { x: -10, y: 900 },
                (304, 460),
                &[
                    monitor(-1_280, 0, 1_280, 984, false),
                    monitor(0, 0, 1_920, 1_040, true),
                ],
                None,
            ),
            Some(PhysicalPosition { x: -314, y: 430 })
        );
    }

    #[test]
    fn tray_popup_opens_below_a_top_taskbar() {
        let anchor = PhysicalPosition { x: 1_910, y: 20 };
        let monitors = [monitor(0, 40, 1_920, 1_040, true)];
        let expected = Some(PhysicalPosition { x: 1_606, y: 40 });
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, Some(TrayTaskbarEdge::Top)),
            expected
        );
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, None),
            expected
        );
    }

    #[test]
    fn tray_popup_opens_right_of_a_left_taskbar() {
        let anchor = PhysicalPosition { x: 20, y: 1_070 };
        let monitors = [monitor(40, 0, 1_880, 1_080, true)];
        let expected = Some(PhysicalPosition { x: 40, y: 600 });
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, Some(TrayTaskbarEdge::Left)),
            expected
        );
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, None),
            expected
        );
    }

    #[test]
    fn tray_popup_opens_left_of_a_right_taskbar() {
        let anchor = PhysicalPosition { x: 1_900, y: 1_070 };
        let monitors = [monitor(0, 0, 1_880, 1_080, true)];
        let expected = Some(PhysicalPosition { x: 1_576, y: 600 });
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, Some(TrayTaskbarEdge::Right)),
            expected
        );
        assert_eq!(
            place_tray_popup(anchor, (304, 460), &monitors, None),
            expected
        );
    }

    #[test]
    fn removed_monitor_bounds_are_centered_on_primary() {
        let restored = restore_bounds(
            WindowBounds {
                x: 2_000,
                y: 200,
                width: 900,
                height: 700,
            },
            &[
                monitor(-1_280, 0, 1_280, 1_024, false),
                monitor(0, 0, 1_920, 1_080, true),
            ],
        );

        assert_eq!(
            restored,
            Some(WindowBounds {
                x: 510,
                y: 190,
                width: 900,
                height: 700,
            })
        );
    }

    #[test]
    fn negative_monitor_coordinates_are_preserved() {
        let saved = WindowBounds {
            x: -1_100,
            y: -800,
            width: 800,
            height: 650,
        };
        assert_eq!(
            restore_bounds(
                saved.clone(),
                &[monitor(-1_280, -1_024, 1_280, 1_024, true)]
            ),
            Some(saved)
        );
    }

    #[test]
    fn partial_intersection_keeps_title_bar_reachable_on_that_monitor() {
        let restored = restore_bounds(
            WindowBounds {
                x: 950,
                y: 790,
                width: 800,
                height: 700,
            },
            &[monitor(0, 0, 1_000, 800, true)],
        );

        assert_eq!(restored.as_ref().map(|bounds| bounds.x), Some(936));
        assert_eq!(restored.as_ref().map(|bounds| bounds.y), Some(736));
        assert_eq!(restored.as_ref().map(|bounds| bounds.width), Some(800));
        assert_eq!(restored.as_ref().map(|bounds| bounds.height), Some(700));
    }

    #[test]
    fn oversized_and_zero_geometry_is_bounded_to_monitor() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 10,
                    y: 10,
                    width: u32::MAX,
                    height: u32::MAX,
                },
                &[monitor(0, 0, 1_200, 900, true)],
            ),
            Some(WindowBounds {
                x: 10,
                y: 10,
                width: 1_200,
                height: 900,
            })
        );
    }

    #[test]
    fn invalid_saved_dimensions_use_centered_default_size() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 100,
                    y: 100,
                    width: 0,
                    height: 900,
                },
                &[monitor(0, 0, 1_200, 900, true)],
            ),
            Some(WindowBounds {
                x: 180,
                y: 90,
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            })
        );
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 100,
                    y: 100,
                    width: 900,
                    height: 0,
                },
                &[monitor(-1_200, -900, 1_200, 900, true)],
            ),
            Some(WindowBounds {
                x: -1_020,
                y: -810,
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            })
        );
    }

    #[test]
    fn monitors_smaller_than_minimum_limit_bounds_to_available_area() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                &[monitor(-400, -300, 400, 300, true)],
            ),
            Some(WindowBounds {
                x: -400,
                y: -300,
                width: 400,
                height: 300,
            })
        );
    }

    #[test]
    fn no_valid_monitor_means_platform_default_placement() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: i32::MAX,
                    y: i32::MIN,
                    width: 0,
                    height: 10,
                },
                &[
                    monitor(100, 100, 0, 900, true),
                    monitor(-100, -100, 900, 0, false),
                ],
            ),
            None
        );
        assert_eq!(restore_bounds(WindowBounds::default(), &[]), None);
    }

    #[test]
    fn zero_area_monitors_are_ignored_when_a_valid_monitor_exists() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 50,
                    y: 50,
                    width: 0,
                    height: 700,
                },
                &[
                    monitor(0, 0, 0, 1_080, true),
                    monitor(-1_600, -200, 1_600, 900, false),
                    monitor(0, 0, 1_920, 0, false),
                ],
            ),
            Some(WindowBounds {
                x: -1_220,
                y: -110,
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            })
        );
    }

    #[test]
    fn extreme_coordinates_and_dimensions_saturate_without_overflow() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: i32::MIN,
                    y: i32::MIN,
                    width: 0,
                    height: u32::MAX,
                },
                &[monitor(i32::MAX, i32::MIN, u32::MAX, u32::MAX, true,)],
            ),
            Some(WindowBounds {
                x: i32::MAX,
                y: -361,
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            })
        );

        let maximum_saved = WindowBounds {
            x: i32::MIN,
            y: i32::MIN,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            restore_bounds(
                maximum_saved.clone(),
                &[monitor(i32::MIN, i32::MIN, u32::MAX, u32::MAX, true,)],
            ),
            Some(maximum_saved)
        );
    }

    #[test]
    fn parses_all_resize_edges_exactly() {
        let cases = [
            ("n", ResizeEdge::North),
            ("ne", ResizeEdge::NorthEast),
            ("e", ResizeEdge::East),
            ("se", ResizeEdge::SouthEast),
            ("s", ResizeEdge::South),
            ("sw", ResizeEdge::SouthWest),
            ("w", ResizeEdge::West),
            ("nw", ResizeEdge::NorthWest),
        ];
        for (text, edge) in cases {
            assert_eq!(parse_resize_edge(text), Some(edge));
        }
        for invalid in ["", "N", " north", "ne ", "north", "x"] {
            assert_eq!(parse_resize_edge(invalid), None);
        }
    }

    #[test]
    fn maps_all_resize_edges_to_winit_030() {
        use slint::winit_030::winit::window::ResizeDirection;

        let cases = [
            (ResizeEdge::North, ResizeDirection::North),
            (ResizeEdge::NorthEast, ResizeDirection::NorthEast),
            (ResizeEdge::East, ResizeDirection::East),
            (ResizeEdge::SouthEast, ResizeDirection::SouthEast),
            (ResizeEdge::South, ResizeDirection::South),
            (ResizeEdge::SouthWest, ResizeDirection::SouthWest),
            (ResizeEdge::West, ResizeDirection::West),
            (ResizeEdge::NorthWest, ResizeDirection::NorthWest),
        ];
        for (edge, direction) in cases {
            assert_eq!(ResizeDirection::from(edge), direction);
        }
    }

    #[test]
    fn lifecycle_dispositions_cover_tray_and_smoke_modes() {
        assert_eq!(minimize_disposition(true), MinimizeDisposition::HideToTray);
        assert_eq!(
            minimize_disposition(false),
            MinimizeDisposition::MinimizeToTaskbar
        );
        assert_eq!(close_disposition(true, false), CloseDisposition::HideToTray);
        assert_eq!(close_disposition(false, false), CloseDisposition::Exit);
        assert_eq!(close_disposition(true, true), CloseDisposition::Exit);
        assert_eq!(close_disposition(false, true), CloseDisposition::Exit);
    }

    #[test]
    fn resize_border_is_disjoint_from_visual_frame_and_title_controls() {
        let layout = ResizeHitLayout::new(840, 720).expect("desktop minimum exceeds border");
        assert_eq!(layout.visual_frame.x, 6);
        assert_eq!(layout.visual_frame.y, 6);
        assert_eq!(layout.visual_frame.width, 828);
        assert_eq!(layout.visual_frame.height, 708);
        for hit in layout.resize_hits() {
            assert!(!hit.intersects(layout.visual_frame));
            for button in layout.title_buttons() {
                assert!(!hit.intersects(button));
            }
        }
    }

    #[test]
    fn resize_corners_own_the_corner_pixels_instead_of_edges() {
        let layout = ResizeHitLayout::new(700, 620).expect("desktop minimum exceeds border");
        let hits = layout.resize_hits();
        for corner in &hits[4..] {
            for edge in &hits[..4] {
                assert!(!corner.intersects(*edge));
            }
        }
        assert_eq!(hits[4].width, 6);
        assert_eq!(hits[4].height, 6);
    }

    #[test]
    fn mixed_dpi_saved_physical_origin_selects_secondary_monitor() {
        let placement = restore_placement(
            WindowBounds {
                x: 3_000,
                y: 200,
                width: 900,
                height: 700,
            },
            &[
                MonitorRect {
                    x: 0,
                    y: 0,
                    width: 1_920,
                    height: 1_040,
                    scale_milli: 1_000,
                    primary: true,
                },
                MonitorRect {
                    x: 1_920,
                    y: 0,
                    width: 2_560,
                    height: 1_400,
                    scale_milli: 2_000,
                    primary: false,
                },
            ],
        )
        .expect("mixed-DPI placement");

        assert!(placement.position.x >= 1_920);
        assert_eq!(placement.inner_size.width, 900);
        assert_eq!(placement.inner_size.height, 700);
        assert_eq!(placement.max_inner_size.width, 1_280);
        assert_eq!(placement.max_inner_size.height, 700);
    }

    #[test]
    fn removed_monitor_falls_back_to_primary_in_physical_space() {
        let placement = restore_placement(
            WindowBounds {
                x: 7_000,
                y: 200,
                width: 900,
                height: 700,
            },
            &[MonitorRect {
                x: 0,
                y: 0,
                width: 1_920,
                height: 1_040,
                scale_milli: 1_000,
                primary: true,
            }],
        )
        .unwrap();
        assert_eq!(placement.position.x, 510);
        assert_eq!(placement.position.y, 170);
    }

    #[test]
    fn negative_mixed_scale_monitor_keeps_physical_origin_negative() {
        let placement = restore_placement(
            WindowBounds {
                x: -2_200,
                y: -100,
                width: 800,
                height: 620,
            },
            &[
                MonitorRect {
                    x: 0,
                    y: 0,
                    width: 1_920,
                    height: 1_040,
                    scale_milli: 1_000,
                    primary: true,
                },
                MonitorRect {
                    x: -2_560,
                    y: -200,
                    width: 2_560,
                    height: 1_440,
                    scale_milli: 1_500,
                    primary: false,
                },
            ],
        )
        .unwrap();
        assert!(placement.position.x < 0);
        assert_eq!(placement.position.y, -100);
        assert_eq!(placement.inner_size.width, 800);
    }

    #[test]
    fn monitor_cap_refreshes_only_when_safe_signature_changes() {
        let first = MonitorSignature {
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_040,
            scale_milli: 1_000,
        };
        let second = MonitorSignature {
            x: 1_920,
            y: 0,
            width: 2_560,
            height: 1_400,
            scale_milli: 2_000,
        };
        assert!(monitor_cap_changed(None, Some(first)));
        assert!(!monitor_cap_changed(Some(first), Some(first)));
        assert!(monitor_cap_changed(Some(first), Some(second)));
        assert!(monitor_cap_changed(Some(second), None));
    }

    #[test]
    fn invalid_scale_data_is_not_used_for_native_placement() {
        for scale_milli in [0, 499, 8_001, u32::MAX] {
            assert_eq!(
                restore_placement(
                    WindowBounds {
                        x: 0,
                        y: 0,
                        width: 840,
                        height: 720
                    },
                    &[MonitorRect {
                        x: 0,
                        y: 0,
                        width: 1_920,
                        height: 1_040,
                        scale_milli,
                        primary: true,
                    }],
                ),
                None
            );
        }
    }

    #[test]
    fn maximize_toggle_inverts_current_state() {
        assert!(next_maximized(false));
        assert!(!next_maximized(true));
    }
}
