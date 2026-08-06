//! Windows Graphics Capture fallback, on `windows-capture`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Gdi::HMONITOR;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame as WgcFrame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{CaptureError, Capturer, Frame, MonitorInfo, Recoverable, enumerate_monitors};

/// WGC pushes a frame per screen update, up to the refresh rate, and every one of them costs a
/// full-screen readback. The tick only ever reads the newest, so the callback drops whatever
/// arrives inside this window; a screen that then goes still stays that much behind, once.
const MIN_READBACK_INTERVAL: Duration = Duration::from_millis(500);

/// How long a fresh session may stay silent before it is called broken instead of idle. After the
/// first frame, silence is just an unchanging screen and says nothing about the session.
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(5);

type Mailbox = Arc<Mutex<Option<Shot>>>;
type Control = CaptureControl<Sink, <Sink as GraphicsCaptureApiHandler>::Error>;

/// Sessions are persistent, same as the duplication's: a session per capture is what ruled out the
/// alternatives in the backend benchmark.
pub(crate) struct WgcCapturer {
    known: Vec<(HMONITOR, MonitorInfo)>,
    sessions: HashMap<String, Session>,
}

impl WgcCapturer {
    pub(crate) fn new() -> Self {
        Self {
            known: Vec::new(),
            sessions: HashMap::new(),
        }
    }

    /// Drop a monitor's session, which is what the engine does once the primary is back.
    pub(crate) fn release(&mut self, monitor_id: &str) {
        self.sessions.remove(monitor_id);
    }
}

impl Capturer for WgcCapturer {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError> {
        self.known = enumerate_monitors();
        let known = &self.known;
        self.sessions
            .retain(|id, _| known.iter().any(|(_, monitor)| &monitor.id == id));
        Ok(self
            .known
            .iter()
            .map(|(_, monitor)| monitor.clone())
            .collect())
    }

    fn capture(&mut self, monitor_id: &str) -> Result<Frame, CaptureError> {
        // This backend runs only for the monitors the primary lost, so it reads the topology per
        // attempt: an enumeration costs microseconds and a stale dpi scale silently mis-scales
        // everything downstream.
        self.monitors()?;
        let (handle, dpi_scale) = self
            .known
            .iter()
            .find(|(_, monitor)| monitor.id == monitor_id)
            .map(|(handle, monitor)| (*handle, monitor.dpi_scale))
            .ok_or(Recoverable::MonitorGone)?;

        let session = match self.sessions.entry(monitor_id.to_owned()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(Session::open(handle)?),
        };

        let captured = session.capture(monitor_id, dpi_scale);
        // Same rule as the duplication: anything but an idle screen ends the session, and the next
        // tick builds a new one.
        if matches!(&captured, Err(error) if !matches!(error, CaptureError::Recoverable(Recoverable::NoNewFrame)))
        {
            self.sessions.remove(monitor_id);
        }
        captured
    }
}

/// One monitor's frame, as the callback thread leaves it for the tick thread.
struct Shot {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    /// Stamped in the callback: the mailbox holds a frame until the next tick reads it, and that
    /// wait is not part of when the screen looked like this.
    captured_at: chrono::DateTime<chrono::Utc>,
}

/// A live capture thread plus the mailbox it writes into.
struct Session {
    control: Option<Control>,
    mailbox: Mailbox,
    opened: Instant,
    delivered: bool,
}

impl Session {
    fn open(handle: HMONITOR) -> Result<Self, CaptureError> {
        let mailbox: Mailbox = Arc::new(Mutex::new(None));
        // Where the OS has no say over its capture border, asking for one costs the whole session,
        // so the frame carries the border the OS draws instead.
        let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
            DrawBorderSettings::WithoutBorder
        } else {
            DrawBorderSettings::Default
        };
        let settings = Settings::new(
            Monitor::from_raw_hmonitor(handle.0),
            // The duplication never composes the cursor; a fallback that did would change what the
            // record means depending on which backend answered.
            CursorCaptureSettings::WithoutCursor,
            border,
            SecondaryWindowSettings::Default,
            // Frames are thinned in the callback instead: the session-level interval needs an API
            // that only the newest Windows builds have, and asking for it there fails the session.
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            // Same ask as the duplication, so an HDR or 10-bit desktop arrives converted.
            ColorFormat::Bgra8,
            mailbox.clone(),
        );
        let control = Sink::start_free_threaded(settings)
            .map_err(|error| Recoverable::DeviceLost(error.to_string()))?;
        Ok(Self {
            control: Some(control),
            mailbox,
            opened: Instant::now(),
            delivered: false,
        })
    }

    fn capture(&mut self, monitor_id: &str, dpi_scale: f32) -> Result<Frame, CaptureError> {
        // The capture thread ends when the item closes or the handler fails, and that is the only
        // thing a mailbox can be asked about its own liveness.
        if self
            .control
            .as_ref()
            .is_none_or(|control| control.is_finished())
        {
            return Err(Recoverable::AccessLost.into());
        }

        let shot = lock(&self.mailbox).take();
        match shot {
            Some(shot) => {
                self.delivered = true;
                Ok(Frame {
                    monitor_id: monitor_id.to_owned(),
                    width: shot.width,
                    height: shot.height,
                    dpi_scale,
                    bgra: shot.bgra,
                    captured_at: shot.captured_at,
                })
            }
            // A session that has never spoken is broken; one that has is looking at a still screen.
            None if !self.delivered && self.opened.elapsed() > FIRST_FRAME_GRACE => {
                Err(Recoverable::AccessLost.into())
            }
            None => Err(Recoverable::NoNewFrame.into()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Dropping the control leaves its thread and its D3D device running forever, so the stop
        // belongs here rather than at each of the places a session is dropped.
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

/// The capture-thread half: it holds no state the tick thread reads except the mailbox.
struct Sink {
    mailbox: Mailbox,
    last_readback: Option<Instant>,
}

impl GraphicsCaptureApiHandler for Sink {
    type Flags = Mailbox;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            mailbox: ctx.flags,
            last_readback: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WgcFrame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self
            .last_readback
            .is_some_and(|at| at.elapsed() < MIN_READBACK_INTERVAL)
        {
            return Ok(());
        }

        // `buffer` maps a staging texture per call and releases it still mapped; the WGC path of
        // the crate has no equivalent of the duplication's caller-owned staging texture, so the
        // ceiling here is how often it is called.
        let buffer = frame.buffer()?;
        let (width, height) = (buffer.width(), buffer.height());
        let mut packed = Vec::new();
        let mut bgra = buffer.as_nopadding_buffer(&mut packed).to_vec();
        // WGC hands back the composed alpha; the desktop image is opaque downstream.
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }

        self.last_readback = Some(Instant::now());
        *lock(&self.mailbox) = Some(Shot {
            width,
            height,
            bgra,
            captured_at: chrono::Utc::now(),
        });
        Ok(())
    }
}

/// Nothing but a move happens under this lock, so a poisoned one still holds a usable mailbox and
/// panicking here would take the capture thread with it.
fn lock(mailbox: &Mailbox) -> std::sync::MutexGuard<'_, Option<Shot>> {
    mailbox.lock().unwrap_or_else(PoisonError::into_inner)
}
