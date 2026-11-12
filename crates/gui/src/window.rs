//! G6.4: адаптивный стартовый размер окна.
//!
//! Размер вычисляется по РАБОЧЕЙ области монитора запуска (без панели задач)
//! и его фактическому DPI — а не по «типовому» разрешению. Один и тот же
//! алгоритм покрывает 1024×600 и 3840×2160, 100 % и 200 %:
//!
//! - на маленьком экране окно занимает почти всю рабочую область, но не
//!   выходит за неё;
//! - на большом — ограничивается разумным максимумом и не растягивается на
//!   весь монитор;
//! - `min_inner_size` тоже не может превысить доступное пространство
//!   (на очень маленьком экране приоритет — прокрутка, а не «всё сразу»).
//!
//! До создания окна размеры считаются в ЛОГИЧЕСКИХ точках egui
//! (`физический размер / (dpi/96)`). Позиция выставляется один раз после
//! создания окна через Win32 `SetWindowPos` уже в физических пикселях:
//! так учитывается монитор, на котором пользователь запускает приложение
//! (курсор), а не обязательно главный. Никаких resize-циклов в `update()`.

/// Рабочая область монитора (физические пиксели) и его эффективный DPI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorWorkArea {
    pub left_px: f32,
    pub top_px: f32,
    pub width_px: f32,
    pub height_px: f32,
    pub dpi: u32,
}

impl MonitorWorkArea {
    /// Масштаб Windows: 1.0 = 100 % (96 DPI).
    pub fn scale(&self) -> f32 {
        (self.dpi as f32 / 96.0).clamp(0.5, 4.0)
    }
}

/// Итоговый план стартового окна.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowPlan {
    /// Желаемый размер КЛИЕНТСКОЙ области в логических точках egui.
    pub inner_size: [f32; 2],
    /// Минимальный размер клиентской области в логических точках.
    pub min_inner_size: [f32; 2],
    /// Центр рабочей области монитора запуска в физических пикселях.
    pub center_px: [f32; 2],
    /// Рабочая область, по которой считался план.
    pub work_area: MonitorWorkArea,
}

/// Предпочтительный размер на больших экранах (не «во весь монитор»).
pub const DESIGN_WIDTH: f32 = 1080.0;
pub const DESIGN_HEIGHT: f32 = 900.0;
/// Верхняя граница: на 2560×1440/4K окно не растягивается бессмысленно.
pub const MAX_WIDTH: f32 = 1440.0;
pub const MAX_HEIGHT: f32 = 1050.0;
/// Минимальные удобные размеры (уменьшаются, если экран меньше).
pub const MIN_WIDTH: f32 = 560.0;
pub const MIN_HEIGHT: f32 = 420.0;
/// Отступ от краёв рабочей области (логические точки).
pub const EDGE_MARGIN: f32 = 20.0;

/// Чистый расчёт плана: без Win32 и egui, покрывается юнит-тестами матрицы
/// разрешений/DPI.
pub fn compute_plan(area: MonitorWorkArea) -> WindowPlan {
    let scale = area.scale();
    let work_w = (area.width_px / scale).max(160.0);
    let work_h = (area.height_px / scale).max(120.0);
    // На очень маленьком экране отступ тоже уменьшается, иначе он съедает
    // заметную часть доступного места.
    let margin = EDGE_MARGIN.min(work_w * 0.03).min(work_h * 0.03).max(0.0);
    let avail_w = (work_w - 2.0 * margin).max(80.0);
    let avail_h = (work_h - 2.0 * margin).max(60.0);

    // min не больше доступного: жёсткий минимум не должен делать окно
    // недоступным на маленьком дисплее.
    let min_w = MIN_WIDTH.min(avail_w);
    let min_h = MIN_HEIGHT.min(avail_h);
    let width = DESIGN_WIDTH.min(MAX_WIDTH).min(avail_w).max(min_w);
    let height = DESIGN_HEIGHT.min(MAX_HEIGHT).min(avail_h).max(min_h);

    WindowPlan {
        inner_size: [width, height],
        min_inner_size: [min_w, min_h],
        center_px: [
            area.left_px + area.width_px / 2.0,
            area.top_px + area.height_px / 2.0,
        ],
        work_area: area,
    }
}

/// Запасная рабочая область, если Win32 недоступен (или не-Windows).
fn fallback_work_area() -> MonitorWorkArea {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SystemParametersInfoW, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA,
        };
        // SAFETY: SystemParametersInfoW(SPI_GETWORKAREA) заполняет RECT
        // рабочей области главного монитора; указатель валиден на время
        // вызова. При отказе — размеры экрана (без вычета панели задач).
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut rect as *mut RECT as *mut core::ffi::c_void,
                0,
            )
        };
        if ok != 0 && rect.right > rect.left && rect.bottom > rect.top {
            return MonitorWorkArea {
                left_px: rect.left as f32,
                top_px: rect.top as f32,
                width_px: (rect.right - rect.left) as f32,
                height_px: (rect.bottom - rect.top) as f32,
                dpi: system_dpi(),
            };
        }
        // SAFETY: GetSystemMetrics — чистый запрос метрики.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if w > 0 && h > 0 {
            return MonitorWorkArea {
                left_px: 0.0,
                top_px: 0.0,
                width_px: w as f32,
                height_px: h as f32,
                dpi: system_dpi(),
            };
        }
    }
    MonitorWorkArea {
        left_px: 0.0,
        top_px: 0.0,
        width_px: 1920.0,
        height_px: 1080.0,
        dpi: 96,
    }
}

#[cfg(windows)]
fn system_dpi() -> u32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    // SAFETY: чистый запрос системного DPI.
    unsafe { GetDpiForSystem().max(96) }
}

#[cfg(windows)]
fn monitor_work_area(
    hmonitor: windows_sys::Win32::Graphics::Gdi::HMONITOR,
) -> Option<MonitorWorkArea> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    if hmonitor.is_null() {
        return None;
    }
    // SAFETY: MONITORINFO нулевой инициализирован, cbSize задан; вызов
    // заполняет структуру для валидного HMONITOR.
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info) };
    if ok == 0 {
        return None;
    }
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: GetDpiForMonitor заполняет два u32; hmonitor валиден.
    let hr = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let dpi = if hr == 0 && dpi_x >= 48 {
        dpi_x
    } else {
        system_dpi()
    };
    Some(MonitorWorkArea {
        left_px: info.rcWork.left as f32,
        top_px: info.rcWork.top as f32,
        width_px: (info.rcWork.right - info.rcWork.left) as f32,
        height_px: (info.rcWork.bottom - info.rcWork.top) as f32,
        dpi,
    })
}

/// Рабочая область монитора, на котором находится курсор (монитор запуска),
/// с откатом на главный монитор.
pub fn launch_work_area() -> MonitorWorkArea {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

        // SAFETY: GetCursorPos заполняет POINT; MonitorFromPoint принимает
        // точку по значению. Оба вызова — чистые запросы.
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            let hmon = if GetCursorPos(&mut pt) != 0 {
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            } else {
                std::ptr::null_mut()
            };
            if let Some(area) = monitor_work_area(hmon) {
                return area;
            }
        }
    }
    fallback_work_area()
}

/// План для текущего запуска.
pub fn initial_plan() -> WindowPlan {
    compute_plan(launch_work_area())
}

/// Одноразовое размещение окна по центру рабочей области монитора запуска
/// (физические пиксели, с учётом DPI монитора). Вызывается из creation
/// callback eframe до первого кадра — окно ещё скрыто, мерцания нет.
///
/// Размер клиентской области при необходимости доводится до плана: если
/// окно создалось на мониторе с другим масштабом, `AdjustWindowRectExForDpi`
/// пересчитывает внешний размер для целевого DPI.
#[cfg(windows)]
pub fn place_on_launch_monitor<H: raw_window_handle::HasWindowHandle + ?Sized>(provider: &H) {
    use raw_window_handle::RawWindowHandle;
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowLongW, GetWindowRect, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
        SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
    };

    let Ok(handle) = provider.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get() as HWND;
    if hwnd.is_null() {
        return;
    }

    let area = launch_work_area();
    let plan = compute_plan(area);
    let scale = area.scale();
    let target_w_px = (plan.inner_size[0] * scale).round().max(1.0);
    let target_h_px = (plan.inner_size[1] * scale).round().max(1.0);

    // SAFETY: hwnd получен из raw-window-handle живого окна. Все вызовы
    // читают/меняют геометрию ТОЛЬКО этого окна; структуры валидны.
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96);
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let mut outer: RECT = std::mem::zeroed();
        outer.right = target_w_px as i32;
        outer.bottom = target_h_px as i32;
        let _ = AdjustWindowRectExForDpi(&mut outer, style, 0, exstyle, dpi);
        let outer_w = (outer.right - outer.left).max(1) as f32;
        let outer_h = (outer.bottom - outer.top).max(1) as f32;
        let x = (area.left_px + (area.width_px - outer_w) / 2.0).clamp(
            area.left_px,
            (area.left_px + area.width_px - outer_w).max(area.left_px),
        );
        let y = (area.top_px + (area.height_px - outer_h) / 2.0).clamp(
            area.top_px,
            (area.top_px + area.height_px - outer_h).max(area.top_px),
        );

        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x.round() as i32,
            y.round() as i32,
            outer_w.round() as i32,
            outer_h.round() as i32,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );

        // Проверка фактической клиентской области (WM_DPICHANGED мог
        // скорректировать размер при переносе на другой монитор). Вторая
        // попытка — не цикл: максимум два вызова за запуск. DPI
        // перечитывается: после переноса окно уже на целевом мониторе.
        for _ in 0..2 {
            let dpi = GetDpiForWindow(hwnd).max(96);
            let mut client: RECT = std::mem::zeroed();
            if GetClientRect(hwnd, &mut client) == 0 {
                break;
            }
            let cw = (client.right - client.left) as f32;
            let ch = (client.bottom - client.top) as f32;
            if (cw - target_w_px).abs() <= 2.0 && (ch - target_h_px).abs() <= 2.0 {
                break;
            }
            let mut wr: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut wr) == 0 {
                break;
            }
            let mut adj: RECT = std::mem::zeroed();
            adj.right = target_w_px as i32;
            adj.bottom = target_h_px as i32;
            let _ = AdjustWindowRectExForDpi(&mut adj, style, 0, exstyle, dpi);
            let ow = (adj.right - adj.left).max(1) as f32;
            let oh = (adj.bottom - adj.top).max(1) as f32;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                wr.left,
                wr.top,
                ow.round() as i32,
                oh.round() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }
}

/// Не-Windows: позиционирование окна не выполняется (eframe сам центрирует
/// по монитору); расчёт размеров остаётся тем же.
#[cfg(not(windows))]
pub fn place_on_launch_monitor<H: ?Sized>(_provider: &H) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Тестовая матрица из задания: разрешение × DPI. Рабочая область =
    /// экран минус панель задач (40 физических px по высоте).
    fn matrix_cases() -> Vec<(u32, u32, u32)> {
        vec![
            (1024, 600, 100),
            (1024, 600, 125),
            (1024, 600, 150),
            (1280, 720, 100),
            (1366, 768, 100),
            (1366, 768, 125),
            (1600, 900, 100),
            (1920, 1080, 100),
            (1920, 1080, 125),
            (1920, 1080, 150),
            (2560, 1440, 100),
            (2560, 1440, 150),
            (3840, 2160, 150),
            (3840, 2160, 200),
        ]
    }

    #[test]
    fn plan_fits_work_area_and_respects_minimums_on_all_matrix_cases() {
        for (w, h, dpi) in matrix_cases() {
            let area = MonitorWorkArea {
                left_px: 0.0,
                top_px: 0.0,
                width_px: w as f32,
                height_px: (h - 40) as f32,
                dpi,
            };
            let plan = compute_plan(area);
            let scale = dpi as f32 / 96.0;
            let work_w = area.width_px / scale;
            let work_h = area.height_px / scale;
            let (pw, ph) = (plan.inner_size[0], plan.inner_size[1]);
            let (mw, mh) = (plan.min_inner_size[0], plan.min_inner_size[1]);

            assert!(pw > 0.0 && ph > 0.0, "{w}x{h}@{dpi}: пустой размер");
            assert!(
                pw <= work_w + 0.01 && ph <= work_h + 0.01,
                "{w}x{h}@{dpi}: {pw}x{ph} не помещается в {work_w}x{work_h}"
            );
            assert!(
                mw <= pw + 0.01 && mh <= ph + 0.01,
                "{w}x{h}@{dpi}: min {mw}x{mh} больше стартового {pw}x{ph}"
            );
            assert!(
                mw <= work_w + 0.01 && mh <= work_h + 0.01,
                "{w}x{h}@{dpi}: min {mw}x{mh} больше рабочей области"
            );
            assert!(
                ph >= 240.0 || ph >= work_h - 1.0,
                "{w}x{h}@{dpi}: слишком низкое окно {ph}"
            );
            // центр рабочей области — точка внутри неё
            assert!(plan.center_px[0] >= area.left_px && plan.center_px[0] <= w as f32);
            assert!(plan.center_px[1] >= area.top_px && plan.center_px[1] <= (h - 40) as f32);
        }
    }

    #[test]
    fn plan_prefers_comfortable_size_on_large_screens() {
        for (w, h, dpi) in [(1920, 1080, 100), (2560, 1440, 100), (3840, 2160, 200)] {
            let plan = compute_plan(MonitorWorkArea {
                left_px: 0.0,
                top_px: 0.0,
                width_px: w as f32,
                height_px: (h - 40) as f32,
                dpi,
            });
            assert!(
                plan.inner_size[0] <= MAX_WIDTH && plan.inner_size[1] <= MAX_HEIGHT,
                "{w}x{h}@{dpi}: окно растянуто на весь монитор"
            );
            assert!(
                plan.inner_size[0] >= 1000.0 && plan.inner_size[1] >= 800.0,
                "{w}x{h}@{dpi}: слишком маленькое окно на большом экране: {:?}",
                plan.inner_size
            );
        }
    }

    #[test]
    fn plan_keeps_window_usable_on_very_small_screen() {
        // 800×480 физических при 150 % = ~533×320 логических точек.
        let plan = compute_plan(MonitorWorkArea {
            left_px: 0.0,
            top_px: 0.0,
            width_px: 800.0,
            height_px: 440.0,
            dpi: 144,
        });
        assert!(plan.inner_size[0] <= 534.0 && plan.inner_size[1] <= 294.0);
        assert!(
            plan.min_inner_size[1] <= 294.0,
            "min не должен быть больше экрана"
        );
    }

    #[test]
    fn plan_centers_on_secondary_monitor_offset() {
        let area = MonitorWorkArea {
            left_px: 1920.0,
            top_px: 0.0,
            width_px: 1366.0,
            height_px: 728.0,
            dpi: 96,
        };
        let plan = compute_plan(area);
        assert_eq!(plan.center_px, [1920.0 + 683.0, 364.0]);
        // высота ограничена рабочей областью за вычетом отступа (20 точек)
        assert_eq!(plan.inner_size, [1080.0, 688.0]);
    }
}
