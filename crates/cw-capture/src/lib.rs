#![deny(unsafe_op_in_unsafe_fn)]
//! Screen capture functionality for ContextWitness.

use windows::Win32::Foundation::{CloseHandle, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetDIBits, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW, ROP_CODE, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};
use windows::core::BOOL;

/// One captured monitor frame, tightly packed BGRA8, alpha forced to 255.
pub struct Frame {
    pub monitor_id: String,
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
    pub bgra: Vec<u8>,
}

#[derive(Default, Clone)]
pub struct Foreground {
    pub process: Option<String>,
    pub title: Option<String>,
}

pub fn make_dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Capture every monitor. A monitor that fails to capture is skipped.
pub fn capture_all() -> Vec<Frame> {
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut monitors as isize),
        );
    }
    monitors.into_iter().filter_map(capture_monitor).collect()
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    monitors.push(monitor);
    BOOL(1)
}

fn capture_monitor(monitor: HMONITOR) -> Option<Frame> {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()).as_bool() {
            return None;
        }
        let rect = info.monitorInfo.rcMonitor;
        let width = u32::try_from(rect.right - rect.left).ok()?;
        let height = u32::try_from(rect.bottom - rect.top).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        let device = String::from_utf16_lossy(&info.szDevice);
        let monitor_id = device.trim_end_matches('\0').to_string();

        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let dpi_scale = dpi_x as f32 / 96.0;

        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width as i32, height as i32);
        let previous = SelectObject(memory, bitmap.into());
        let blt = BitBlt(
            memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            rect.left,
            rect.top,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        if blt.is_err() || lines == 0 {
            return None;
        }
        // GDI leaves alpha at 0; downstream consumers treat the frame as opaque.
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Some(Frame {
            monitor_id,
            width,
            height,
            dpi_scale,
            bgra,
        })
    }
}

pub fn foreground() -> Foreground {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return Foreground::default();
        }
        let mut title_buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut title_buf);
        let title = (len > 0).then(|| String::from_utf16_lossy(&title_buf[..len as usize]));
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = (pid != 0).then(|| process_name(pid)).flatten();
        Foreground { process, title }
    }
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}
