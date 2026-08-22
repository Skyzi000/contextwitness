#![deny(unsafe_op_in_unsafe_fn)]
//! Screen capture functionality for ContextWitness.

mod dxgi;
mod failover;
mod wgc;

use std::collections::HashMap;
use std::time::Instant;

use failover::{Backend, Failover, Outcome};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW};
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
use windows_capture::monitor::Monitor;

/// A frame composed longer ago than this at pull time — on either the QPC or the wall clock — is
/// refused, and the session is torn down so a rebuilt session can compose anew instead of
/// waiting for a repaint. This must stay under the episode closer's 60-second grace, which
/// `cw-daemon` pins with a compile-time assert.
pub const STALE_SHOT_SECONDS: u64 = 30;

/// One captured monitor frame, tightly packed BGRA8, alpha forced to 255.
pub struct Frame {
    pub monitor_id: String,
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
    pub bgra: Vec<u8>,
    /// Stamped inside the backend's own capture path — not when a consumer got around to the
    /// frame. OCR takes seconds per frame and the fallback holds its newest frame until the next
    /// tick, so timestamps taken downstream would drift by that much.
    pub captured_at: chrono::DateTime<chrono::Utc>,
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
    #[error("monitor enumeration failed: {0}")]
    EnumerationFailed(String),
    #[error("monitor is no longer attached")]
    MonitorGone,
    #[error("no desktop update to capture")]
    NoNewFrame,
    #[error("held frame is too old to deliver")]
    StaleFrame,
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
    /// Diagnostics only (`capture-once --wgc`): the fallback path otherwise runs solely when the
    /// primary has genuinely failed, which is not a condition a smoke test can order up.
    force_fallback: bool,
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
            tracing::warn!(
                "process is not PER_MONITOR_AWARE_V2, so capture resolution and dpi scale are virtualized"
            );
        }
        Self {
            dxgi: dxgi::DxgiCapturer::new(),
            wgc: wgc::WgcCapturer::new(),
            failover: HashMap::new(),
            reported: HashMap::new(),
            force_fallback: false,
        }
    }

    /// Route every capture through the fallback backend, permanently. Diagnostics only.
    pub fn force_fallback(&mut self) {
        self.force_fallback = true;
    }

    pub fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError> {
        self.dxgi.monitors()
    }

    /// Drop the fallback's capture sessions. The daemon's privacy gate only stops the tick from
    /// *reading*; the fallback's callback threads keep composing frames regardless, and a frame
    /// can sit in a held texture or wait in a frame pool arbitrarily long — a suspend included.
    /// After this call returns, no frame composed before it is deliverable: the sessions alive
    /// before it are destroyed, and later sessions refuse stamps from before the call.
    pub fn discard_pending(&mut self) {
        self.wgc.discard_pending();
    }

    /// Capture every monitor, each with the backend its own failover state points at. A monitor
    /// that fails to capture is skipped, this tick only. A failed enumeration is answered beside
    /// the frames rather than instead of them — a captured frame is already consumed from its
    /// backend, so an early return would lose it for good — and the caller hands them to the
    /// ordinary change judgment and store before failing the pass, so the failure cannot be
    /// read as every monitor having been detached.
    pub fn capture_all(&mut self) -> (Vec<Frame>, Option<CaptureError>) {
        let monitors = match self.dxgi.monitors() {
            Ok(monitors) => monitors,
            Err(error) => return (Vec::new(), Some(error)),
        };
        self.failover
            .retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
        self.wgc.retain_monitors(&monitors);

        let mut frames = Vec::new();
        let mut enumeration_failed = None;
        for monitor in monitors {
            let now = Instant::now();
            let state = self.failover.entry(monitor.id.clone()).or_default();
            let target = if self.force_fallback {
                Backend::Fallback
            } else {
                match state.target(now) {
                    Some(target) => target,
                    None => continue,
                }
            };
            let mut used = target;
            let mut result = match target {
                Backend::Primary => self.dxgi.capture(&monitor.id),
                Backend::Fallback => self.wgc.capture(&monitor.id),
            };
            if !self.force_fallback {
                let switched = state.record(target, outcome(&result), now);
                if target == Backend::Primary && state.target(now) == Some(Backend::Fallback) {
                    used = Backend::Fallback;
                    result = self.wgc.capture(&monitor.id);
                    state.record(Backend::Fallback, outcome(&result), now);
                }

                if let Some(backend) = switched {
                    tracing::info!(
                        "capture backend for {} switched to {}",
                        monitor.id,
                        backend_name(backend)
                    );
                    if backend == Backend::Primary {
                        drain_then_release(&mut result, |op| match op {
                            HoldOp::Drain => self.wgc.drain_existing(&monitor),
                            HoldOp::Release => {
                                self.wgc.release(&monitor.id);
                                None
                            }
                        });
                    }
                }
            }

            match result {
                Ok(frame) => {
                    self.reported.remove(&monitor.id);
                    frames.push(frame);
                }
                Err(CaptureError::Recoverable(Recoverable::NoNewFrame)) => {
                    self.reported.remove(&monitor.id);
                }
                // The fallback re-enumerates on its way to a session, so this failure can surface
                // mid-loop — and it is the pass-level refusal, not this monitor's. The loop goes
                // on: the primary backend reads cached handles, so its monitors still answer.
                Err(CaptureError::Recoverable(Recoverable::EnumerationFailed(message))) => {
                    enumeration_failed.get_or_insert(message);
                }
                Err(error) => {
                    self.report(&monitor.id, used, &error.to_string());
                }
            }
        }
        (
            frames,
            enumeration_failed.map(|message| Recoverable::EnumerationFailed(message).into()),
        )
    }

    /// The same failure repeating every tick is one line of news, not one line per tick. The
    /// backend name is part of the news: the primary's recovery probe and the fallback's re-open
    /// fail on the same monitor, and only the name says which one is speaking.
    fn report(&mut self, monitor_id: &str, backend: Backend, message: &str) -> bool {
        let line = format!("[{}] {message}", backend_name(backend));
        if self
            .reported
            .get(monitor_id)
            .is_some_and(|last| last == &line)
        {
            return false;
        }
        tracing::warn!("capture failed for {monitor_id}: {line}");
        self.reported.insert(monitor_id.to_owned(), line);
        true
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Primary => "dxgi",
        Backend::Fallback => "wgc",
    }
}

/// What a capture attempt says about its backend. An idle screen is an answer: only a live session
/// can report that there was nothing to capture.
fn outcome(result: &Result<Frame, CaptureError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Delivered,
        Err(CaptureError::Recoverable(Recoverable::NoNewFrame)) => Outcome::Answered,
        Err(_) => Outcome::Failed,
    }
}

/// The switch-back settlement, one call so the drain cannot land after the release destroys the
/// session: only a still probe drains, only an actual frame replaces its answer, and the release
/// runs on every path.
fn drain_then_release(
    result: &mut Result<Frame, CaptureError>,
    mut hold: impl FnMut(HoldOp) -> Option<Frame>,
) {
    if matches!(
        result,
        Err(CaptureError::Recoverable(Recoverable::NoNewFrame))
    ) && let Some(frame) = hold(HoldOp::Drain)
    {
        *result = Ok(frame);
    }
    hold(HoldOp::Release);
}

enum HoldOp {
    Drain,
    Release,
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

/// Ask for PER_MONITOR_AWARE_V2 and answer whether the process actually has it. The setter's own
/// result is not the answer: it fails with ERROR_ACCESS_DENIED when awareness was already set (a
/// manifest, an AppCompat shim), and that state may still be the right one. Without V2, Windows
/// virtualizes both the capture resolution and every DPI read, silently (design §3.2) — the caller
/// decides whether to keep going on a `false`.
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

/// Every attached monitor, paired with the handle the backend needs to open a session on it.
/// Refuses when the enumeration itself fails, and when it enumerates monitors but every one is
/// refused by the per-monitor queries — either answer would read as every monitor having been
/// detached. A handle the queries refuse is otherwise left out alone, the reason warned once per
/// uninterrupted streak — `skips` holds the reported reasons between calls, keyed by handle,
/// and an entry leaves it when its handle answers again or stops being enumerated.
fn enumerate_monitors(
    skips: &mut HashMap<isize, String>,
) -> Result<Vec<(HMONITOR, MonitorInfo)>, CaptureError> {
    let monitors =
        Monitor::enumerate().map_err(|error| Recoverable::EnumerationFailed(error.to_string()))?;
    let mut usable = Vec::new();
    let mut skipped: HashMap<isize, String> = HashMap::new();
    for monitor in monitors {
        let handle = HMONITOR(monitor.as_raw_hmonitor());
        match monitor_info(handle) {
            Ok(info) => usable.push((handle, info)),
            Err(reason) => {
                let key = handle.0 as isize;
                if skips.get(&key) != Some(&reason) {
                    tracing::warn!("monitor {key:#x} skipped: {reason}");
                }
                skipped.insert(key, reason);
            }
        }
    }
    *skips = skipped;
    enumeration_verdict(usable, skips)
}

/// The enumeration's verdict: refuses when monitors were enumerated and every one was refused
/// by the per-monitor queries — that answer would read as every monitor having been detached —
/// while an enumeration that answered no monitors at all is an empty success.
fn enumeration_verdict<T>(
    usable: Vec<T>,
    skipped: &HashMap<isize, String>,
) -> Result<Vec<T>, CaptureError> {
    if usable.is_empty() && !skipped.is_empty() {
        // Sorted: the daemon's once-per-streak failure dedup compares spellings.
        let mut reasons: Vec<&str> = skipped.values().map(String::as_str).collect();
        reasons.sort_unstable();
        return Err(Recoverable::EnumerationFailed(format!(
            "every enumerated monitor was refused: {}",
            reasons.join("; ")
        ))
        .into());
    }
    Ok(usable)
}

/// `Err` is a monitor this enumeration cannot use, carrying why: its rect is empty, or a query
/// its enumeration-fresh handle should answer failed. A hotplug departure between the two calls
/// answers this way too, but the queries do not say which it was, so the reason is reported
/// rather than presumed.
fn monitor_info(monitor: HMONITOR) -> Result<MonitorInfo, String> {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()).as_bool() {
            return Err("GetMonitorInfoW failed".to_owned());
        }
        let rect = info.monitorInfo.rcMonitor;
        let device = String::from_utf16_lossy(&info.szDevice);
        let device = device.trim_end_matches('\0');
        let width = u32::try_from(rect.right - rect.left).unwrap_or(0);
        let height = u32::try_from(rect.bottom - rect.top).unwrap_or(0);
        if width == 0 || height == 0 {
            return Err(format!("{device}: the monitor rect has no area"));
        }

        // No 96 stand-in on failure: a wrong scale silently skews the logical-pixel change
        // threshold.
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        if let Err(error) = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
            return Err(format!("{device}: GetDpiForMonitor failed: {error}"));
        }

        Ok(MonitorInfo {
            id: device.to_string(),
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn a_report_names_the_backend_that_failed() {
        let mut engine = CaptureEngine::new();
        assert!(engine.report("m", Backend::Primary, "boom"));
        assert_eq!(engine.reported["m"], "[dxgi] boom");
        assert!(!engine.report("m", Backend::Primary, "boom"));
        assert!(engine.report("m", Backend::Fallback, "boom"));
        assert_eq!(engine.reported["m"], "[wgc] boom");
    }

    #[test]
    fn only_an_enumeration_with_every_monitor_refused_is_a_failure() {
        let refused = HashMap::from([(1, "b refused".to_owned()), (2, "a refused".to_owned())]);
        assert!(
            enumeration_verdict::<i32>(Vec::new(), &HashMap::new())
                .unwrap()
                .is_empty()
        );
        assert_eq!(enumeration_verdict(vec![7], &refused).unwrap(), [7]);
        let error = enumeration_verdict::<i32>(Vec::new(), &refused).unwrap_err();
        assert_eq!(
            error.to_string(),
            "monitor enumeration failed: every enumerated monitor was refused: a refused; b refused"
        );
    }

    fn frame(monitor_id: &str) -> Frame {
        Frame {
            monitor_id: monitor_id.to_owned(),
            width: 1,
            height: 1,
            dpi_scale: 1.0,
            bgra: vec![0, 0, 0, 255],
            captured_at: chrono::Utc::now(),
        }
    }

    fn settle(result: &mut Result<Frame, CaptureError>, held: Option<&str>) -> Vec<&'static str> {
        let ops = RefCell::new(Vec::new());
        drain_then_release(result, |op| match op {
            HoldOp::Drain => {
                ops.borrow_mut().push("drain");
                held.map(frame)
            }
            HoldOp::Release => {
                ops.borrow_mut().push("release");
                None
            }
        });
        ops.into_inner()
    }

    #[test]
    fn only_a_still_probe_drains_and_the_release_lands_last_on_every_path() {
        let mut still: Result<Frame, CaptureError> = Err(Recoverable::NoNewFrame.into());
        assert_eq!(settle(&mut still, Some("held")), ["drain", "release"]);
        assert!(matches!(&still, Ok(frame) if frame.monitor_id == "held"));

        let mut empty: Result<Frame, CaptureError> = Err(Recoverable::NoNewFrame.into());
        assert_eq!(settle(&mut empty, None), ["drain", "release"]);
        assert!(matches!(
            empty,
            Err(CaptureError::Recoverable(Recoverable::NoNewFrame))
        ));

        let mut delivered: Result<Frame, CaptureError> = Ok(frame("current"));
        assert_eq!(settle(&mut delivered, Some("unread")), ["release"]);
        assert!(matches!(&delivered, Ok(frame) if frame.monitor_id == "current"));

        let mut failed: Result<Frame, CaptureError> = Err(Recoverable::AccessLost.into());
        assert_eq!(settle(&mut failed, Some("unread")), ["release"]);
        assert!(matches!(
            failed,
            Err(CaptureError::Recoverable(Recoverable::AccessLost))
        ));
    }
}
