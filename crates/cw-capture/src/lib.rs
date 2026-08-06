#![deny(unsafe_op_in_unsafe_fn)]
//! Screen capture functionality for ContextWitness.

mod dxgi;
mod failover;
mod wgc;

use std::collections::HashMap;
use std::time::Instant;

use failover::{Backend, Failover, Outcome};

use windows::Win32::Foundation::{CloseHandle, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    GetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, MONITORINFOF_PRIMARY,
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

/// One attached monitor. `width`/`height` are physical pixels and `dpi_scale` is what turns them
/// into logical ones; the capture layer only measures, `cw-core` decides.
#[derive(Clone)]
pub struct MonitorInfo {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
    pub is_primary: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// Re-enumerating monitors or rebuilding the session can clear it; the tick is skipped.
    #[error("{0}")]
    Recoverable(#[from] Recoverable),
    /// Retrying cannot clear it.
    #[error("{0}")]
    Fatal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Recoverable {
    #[error("duplication access lost")]
    AccessLost,
    #[error("graphics device error: {0}")]
    DeviceLost(String),
    #[error("monitor is no longer attached")]
    MonitorGone,
    #[error("no desktop update to capture")]
    NoNewFrame,
    #[error("unsupported pixel format: {0}")]
    UnsupportedFormat(String),
}

/// The capture boundary. Pull type: `capture` returns that monitor's latest frame, which is what a
/// tick-driven snapshot collector wants. Persistent sessions are implementation state and stay off
/// this surface, so the backend under it can be replaced without the daemon noticing.
pub trait Capturer {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError>;
    fn capture(&mut self, monitor_id: &str) -> Result<Frame, CaptureError>;
}

/// Holds the capture sessions across ticks, and per monitor the choice of which backend owns them.
pub struct CaptureEngine {
    dxgi: dxgi::DxgiCapturer,
    /// Costs nothing until a monitor fails over: sessions inside it are opened on first use.
    wgc: wgc::WgcCapturer,
    failover: HashMap<String, Failover>,
    reported: HashMap<String, String>,
}

impl CaptureEngine {
    pub fn new() -> Self {
        // Without PER_MONITOR_AWARE_V2 Windows virtualizes both the DPI we read and the resolution
        // the duplication hands us, with no error anywhere: a 2560x1440 screen arrives as
        // 2048x1152 and the OCR gets the blur.
        let aware = unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };
        if !aware.as_bool() {
            tracing::warn!(
                "process is not PER_MONITOR_AWARE_V2, so capture resolution and dpi scale are virtualized"
            );
        }
        Self {
            dxgi: dxgi::DxgiCapturer::new(),
            wgc: wgc::WgcCapturer::new(),
            failover: HashMap::new(),
            reported: HashMap::new(),
        }
    }

    pub fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError> {
        self.dxgi.monitors()
    }

    /// Capture every monitor, each with the backend its own failover state points at. A monitor
    /// that fails to capture is skipped, this tick only.
    pub fn capture_all(&mut self) -> Vec<Frame> {
        let monitors = match self.dxgi.monitors() {
            Ok(monitors) => monitors,
            Err(error) => {
                self.report("monitor enumeration", &error.to_string());
                return Vec::new();
            }
        };
        // A detached monitor takes its failover state with it, so one that comes back is tried on
        // the primary again rather than inheriting the run of failures that lost it.
        self.failover
            .retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));

        let mut frames = Vec::new();
        for monitor in monitors {
            let now = Instant::now();
            let state = self.failover.entry(monitor.id.clone()).or_default();
            let target = state.target(now);
            let mut result = match target {
                Backend::Primary => self.dxgi.capture(&monitor.id),
                Backend::Fallback => self.wgc.capture(&monitor.id),
            };
            let switched = state.record(target, outcome(&result), now);
            // A primary attempt that failed and left the monitor on the fallback must not cost the
            // tick its frame: that is where the probe, and the failover itself, gets to be cheap.
            if target == Backend::Primary && state.target(now) == Backend::Fallback {
                result = self.wgc.capture(&monitor.id);
                state.record(Backend::Fallback, outcome(&result), now);
            }

            if let Some(backend) = switched {
                let name = match backend {
                    Backend::Primary => "dxgi",
                    Backend::Fallback => "wgc",
                };
                tracing::info!("capture backend for {} switched to {name}", monitor.id);
                if backend == Backend::Primary {
                    self.wgc.release(&monitor.id);
                }
            }

            match result {
                Ok(frame) => {
                    self.reported.remove(&monitor.id);
                    frames.push(frame);
                }
                // An idle screen produces no update at all, which is the answer, not a failure.
                Err(CaptureError::Recoverable(Recoverable::NoNewFrame)) => {}
                Err(error) => self.report(&monitor.id, &error.to_string()),
            }
        }
        frames
    }

    /// The same failure repeating every tick is one line of news, not one line per tick.
    fn report(&mut self, monitor_id: &str, message: &str) {
        if self
            .reported
            .get(monitor_id)
            .is_some_and(|last| last == message)
        {
            return;
        }
        tracing::warn!("capture failed for {monitor_id}: {message}");
        self.reported
            .insert(monitor_id.to_owned(), message.to_owned());
    }
}

/// What a capture attempt says about its backend. An idle screen is an answer: only a live session
/// can report that there was nothing to capture.
fn outcome(result: &Result<Frame, CaptureError>) -> Outcome {
    match result {
        Ok(_) | Err(CaptureError::Recoverable(Recoverable::NoNewFrame)) => Outcome::Answered,
        Err(_) => Outcome::Failed,
    }
}

impl Default for CaptureEngine {
    fn default() -> Self {
        Self::new()
    }
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

/// Every attached monitor, paired with the handle the backend needs to open a session on it.
fn enumerate_monitors() -> Vec<(HMONITOR, MonitorInfo)> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut handles as isize),
        );
    }
    handles
        .into_iter()
        .filter_map(|handle| monitor_info(handle).map(|info| (handle, info)))
        .collect()
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

fn monitor_info(monitor: HMONITOR) -> Option<MonitorInfo> {
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

        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

        Some(MonitorInfo {
            id: device.trim_end_matches('\0').to_string(),
            width,
            height,
            dpi_scale: dpi_x as f32 / 96.0,
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
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
