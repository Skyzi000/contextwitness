//! Windows Graphics Capture fallback, on `windows-capture`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::d3d11::StagingTexture;
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
    /// Shots stamped at or before this are refused. One timestamp covers every session: a session
    /// opened after the last discard only ever produces shots newer than it.
    discard_before: Option<chrono::DateTime<chrono::Utc>>,
}

impl WgcCapturer {
    pub(crate) fn new() -> Self {
        Self {
            known: Vec::new(),
            sessions: HashMap::new(),
            discard_before: None,
        }
    }

    /// Drop a monitor's session, which is what the engine does once the primary is back.
    pub(crate) fn release(&mut self, monitor_id: &str) {
        self.sessions.remove(monitor_id);
    }

    /// Drop sessions for monitors that are gone. `monitors` does this too, but only when something
    /// actually captures through this backend: were the last fallback monitor detached, nothing
    /// would call in again and its capture thread, D3D device and mailbox would outlive it by the
    /// life of the process.
    pub(crate) fn retain_monitors(&mut self, monitors: &[MonitorInfo]) {
        self.sessions
            .retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
    }

    /// Empty every mailbox and refuse everything captured up to now. The callback threads keep
    /// filling mailboxes while the daemon's privacy gate is closed, so without this the first tick
    /// after the gate reopens could hand over a screen the gate existed to keep out.
    pub(crate) fn discard_pending(&mut self) {
        // Stamped before the mailboxes are emptied so that a frame landing in between is covered by
        // the watermark rather than slipping past both.
        self.discard_before = Some(chrono::Utc::now());
        for session in self.sessions.values() {
            lock(&session.mailbox).take();
        }
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
        let discard_before = self.discard_before;
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

        let captured = session.capture(monitor_id, dpi_scale, discard_before);
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

    /// Guarantees, with `discard_before`, that no frame captured before the most recent discard is
    /// ever returned.
    fn capture(
        &mut self,
        monitor_id: &str,
        dpi_scale: f32,
        discard_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Frame, CaptureError> {
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
            // Captured before the last discard: the gate was closed then, so this is the same
            // answer as an empty mailbox. The session did speak, though, so it is not the silence
            // the first-frame grace is watching for.
            Some(shot) if discard_before.is_some_and(|at| shot.captured_at <= at) => {
                self.delivered = true;
                Err(Recoverable::NoNewFrame.into())
            }
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
        // Stamped before anything else, so the stamp bounds when this frame's pixels are from. A
        // readback takes real time, and a stamp taken after it would let a `discard_before`
        // watermark drawn mid-readback fall between the pixels and their stamp — the shot would
        // then outrank the watermark while showing the gated screen. What remains is the frame
        // pool's own composition latency, one frame time.
        let captured_at = chrono::Utc::now();
        if self
            .last_readback
            .is_some_and(|at| at.elapsed() < MIN_READBACK_INTERVAL)
        {
            return Ok(());
        }

        // Hand-rolled instead of `Frame::buffer`: that one creates its staging texture as a local,
        // maps it, and hands back a slice into the mapping — then releases the texture, still
        // mapped, before the caller reads a byte. This owns the texture across the read and unmaps
        // before dropping it. A new texture per call, not one cached in the sink, because
        // `start_free_threaded` requires the handler to be `Send` and a D3D texture is not.
        let (width, height) = (frame.width(), frame.height());
        let staging = StagingTexture::new(frame.device(), width, height, frame.desc().Format)?;
        let context = frame.device_context();
        // 4 bytes per pixel throughout: the session asks the OS for `ColorFormat::Bgra8`, so the
        // frame pool converts whatever the desktop really is.
        let row = width as usize * 4;
        let mut bgra = vec![0u8; row * height as usize];
        unsafe {
            context.CopyResource(staging.texture(), frame.as_raw_texture());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(staging.texture(), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                    bgra.as_mut_ptr().add(y * row),
                    row,
                );
            }
            context.Unmap(staging.texture(), 0);
        }
        // WGC hands back the composed alpha; the desktop image is opaque downstream.
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }

        self.last_readback = Some(Instant::now());
        *lock(&self.mailbox) = Some(Shot {
            width,
            height,
            bgra,
            captured_at,
        });
        Ok(())
    }
}

/// Nothing but a move happens under this lock, so a poisoned one still holds a usable mailbox and
/// panicking here would take the capture thread with it.
fn lock(mailbox: &Mailbox) -> std::sync::MutexGuard<'_, Option<Shot>> {
    mailbox.lock().unwrap_or_else(PoisonError::into_inner)
}
