#![deny(unsafe_op_in_unsafe_fn)]
//! Screen capture functionality for ContextWitness.

mod wgc;

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    GetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
};

/// A frame composed longer ago than this at pull time — on either the QPC or the wall clock — is
/// refused, and the session is torn down so a rebuilt session can compose anew instead of
/// waiting for a repaint. This must stay under the episode closer's 60-second grace, which
/// `cw-daemon` pins with a compile-time assert.
pub const STALE_SHOT_SECONDS: u64 = 30;

/// One captured frame, tightly packed BGRA8, alpha forced to 255.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
    pub bgra: Vec<u8>,
    /// Stamped inside the backend's own capture path — not when a consumer got around to the
    /// frame. OCR takes seconds per frame, so timestamps taken downstream would drift by that
    /// much.
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The tick is skipped.
    #[error("{0}")]
    Recoverable(#[from] Recoverable),
    /// Retrying cannot clear it.
    #[error("{0}")]
    Fatal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Recoverable {
    #[error("capture session ended")]
    AccessLost,
    #[error("graphics device error: {0}")]
    DeviceLost(String),
    #[error("dpi query failed: {0}")]
    DpiUnavailable(String),
    #[error("no window update to capture")]
    NoNewFrame,
    #[error("held frame is too old to deliver")]
    StaleFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
}

pub struct CaptureEngine {
    session: Option<(Target, wgc::Session)>,
    /// Delivery floor set by `discard_pending`; sessions opened after it refuse older stamps.
    floor_100ns: i64,
}

impl CaptureEngine {
    pub fn new() -> Self {
        let aware = unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };
        if !aware.as_bool() {
            tracing::warn!("process is not PER_MONITOR_AWARE_V2, so dpi scale is virtualized");
        }
        Self {
            session: None,
            floor_100ns: 0,
        }
    }

    pub fn capture(&mut self, target: Target) -> Result<Frame, CaptureError> {
        if self
            .session
            .as_ref()
            .is_some_and(|(held, _)| *held != target)
        {
            self.session = None;
        }

        // No 96 stand-in on failure: a wrong scale silently skews the logical-pixel change
        // threshold.
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let queried = unsafe {
            let monitor = MonitorFromWindow(HWND(target.hwnd as _), MONITOR_DEFAULTTONEAREST);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)
        };
        queried.map_err(|error| Recoverable::DpiUnavailable(error.to_string()))?;
        let dpi_scale = dpi_x as f32 / 96.0;

        let (mut session, opened) = match self.session.take() {
            Some((_, session)) => (session, false),
            None => (wgc::Session::open(target.hwnd, self.floor_100ns)?, true),
        };
        let captured = if opened {
            session.first_frame(dpi_scale)
        } else {
            session.capture(dpi_scale)
        };
        if matches!(
            captured,
            Ok(_) | Err(CaptureError::Recoverable(Recoverable::NoNewFrame))
        ) {
            self.session = Some((target, session));
        }
        captured
    }

    pub fn release(&mut self) {
        self.session = None;
    }

    /// Drop the capture session. The daemon's privacy gate only stops the tick from *reading*;
    /// the capture thread keeps composing frames regardless, and a frame can sit in a held
    /// texture or wait in a frame pool arbitrarily long — a suspend included. After this call
    /// returns, no frame composed before it is deliverable: the sessions alive before it are
    /// destroyed, and later sessions refuse stamps from before the call. Destroying the sessions
    /// disposes every held texture and queued frame; the floor covers what destruction cannot —
    /// nothing documents that a fresh session's first compose postdates its creation, so the
    /// boundary is enforced on the composition stamp instead of assumed of the pipeline.
    pub fn discard_pending(&mut self) {
        self.session = None;
        // Refusing every capture forever is the correct answer to a clock that cannot be read.
        self.floor_100ns = wgc::qpc_now_100ns().unwrap_or(i64::MAX);
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
    pub target: Option<Target>,
}

/// Ask for PER_MONITOR_AWARE_V2 and answer whether the process actually has it. The setter's own
/// result is not the answer: it fails with ERROR_ACCESS_DENIED when awareness was already set (a
/// manifest, an AppCompat shim), and that state may still be the right one. Without V2, Windows
/// virtualizes every DPI read, silently — the caller decides whether to keep going
/// on a `false`.
pub fn make_dpi_aware() -> bool {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .as_bool()
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
        let mut class_buf = [0u16; 256];
        let class_len = GetClassNameW(hwnd, &mut class_buf);
        let class = String::from_utf16_lossy(&class_buf[..class_len.max(0) as usize]);
        let desktop = matches!(class.as_str(), "Progman" | "WorkerW");
        let target = (!IsIconic(hwnd).as_bool() && !desktop).then_some(Target {
            hwnd: hwnd.0 as isize,
            pid,
        });
        Foreground {
            process,
            title,
            target,
        }
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
